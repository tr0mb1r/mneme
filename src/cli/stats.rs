//! `mneme stats` — human-facing memory health summary.
//!
//! Mirrors the `mneme://stats` MCP resource (per-layer counts +
//! schema version + applied_lsn + embed_dim) and adds on-disk size
//! so a sysadmin can watch growth over time. Refuses to run when a
//! server holds the lockfile, so concurrent boots don't corrupt the
//! redb mmap.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::Config;
use crate::crypto::{KekStore, OsKeyring};
use crate::index::snapshot;
use crate::mcp::tools::size_tier;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::storage::MEM_KEY_PREFIX;
use crate::storage::archive::ColdArchive;
use crate::storage::layout;
use crate::{MnemeError, Result, migrate};

pub fn execute() -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    let json = stats_json(&root)?;
    let pretty = serde_json::to_string_pretty(&json)
        .map_err(|e| MnemeError::Storage(format!("encode stats: {e}")))?;
    println!("{pretty}");
    Ok(())
}

/// Build the stats payload for `root` using the OS keyring for KEK
/// custody. Pulled out for testability so we don't have to fork the
/// binary in tests.
///
/// On a plaintext (non-encrypted) data dir the keyring is never
/// touched — [`crate::crypto::boot`] only consults it when
/// `keystore.json` is present.
pub fn stats_json(root: &Path) -> Result<Value> {
    stats_json_with_keyring(root, &OsKeyring::new())
}

/// Build the stats payload for `root`, resolving the KEK through
/// `keyring`.
///
/// Every on-disk surface read here goes through the crypto-aware
/// constructor. Reading an encrypted data dir with the plaintext
/// readers is not a graceful degradation — the redb WAL replay hands
/// AEAD ciphertext to postcard and the open fails with
/// `write-ahead log error: postcard decode: ...` before any count is
/// produced (regression fixed 2026-07-25).
pub fn stats_json_with_keyring(root: &Path, keyring: &dyn KekStore) -> Result<Value> {
    refuse_if_locked(root)?;

    let schema_version = migrate::current_version(root).unwrap_or(0);

    // L4 semantic — count mem: rows and read snapshot metadata.
    let (storage, aead) = crate::crypto::boot::open_episodic_storage_and_aead(root, keyring)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;

    let semantic_count = runtime.block_on(async {
        storage
            .scan_prefix(MEM_KEY_PREFIX)
            .await
            .map(|v| v.len())
            .unwrap_or(0)
    });

    // L4 size-tier scan (release-planning v2.1 §5.5). Reads
    // [budgets].max_remember_chars from the on-disk config so the
    // CLI matches what `mneme run` enforces. Best-effort: a missing
    // config falls back to defaults.
    let max_remember_chars = Config::load(&root.join("config.toml"))
        .map(|c| c.budgets.max_remember_chars)
        .unwrap_or(size_tier::DEFAULT_MAX_CHARS);
    let large_memory_count = runtime
        .block_on(async { size_tier::count_corpus(&storage, max_remember_chars).await })
        .map(|s| s.to_json())
        .unwrap_or(Value::Null);

    // L3 episodic — same backing storage, different prefixes.
    let episodic = EpisodicStore::new(Arc::clone(&storage));
    let (hot_count, warm_count) = runtime.block_on(async {
        let hot = episodic.count_hot().await.unwrap_or(0);
        let warm = episodic.count_warm().await.unwrap_or(0);
        (hot, warm)
    });

    // L0 procedural — count live pinned items. Counts through
    // `ProceduralStore` rather than by line, so the count is right in
    // encrypted mode too (the whole JSONL is one MNE1 envelope there,
    // which a line count reads as a single item).
    let procedural_count = ProceduralStore::open_with_crypto(root, aead.clone())
        .and_then(|p| p.list(None).map(|v| v.len()))
        .unwrap_or(0);

    // Cold tier — count quarterly archives.
    let cold = ColdArchive::new_with_crypto(root, aead.clone());
    let cold_quarters = cold.list_quarters().map(|v| v.len()).unwrap_or(0);

    // Snapshot metadata. Absent file is fine (cold start).
    let snap_path = root.join("semantic").join("hnsw.idx");
    let (applied_lsn, embed_dim) = match snapshot::load_with_crypto(&snap_path, aead.as_deref()) {
        Ok((idx, lsn)) => (lsn, idx.dim()),
        Err(_) => (0u64, 0usize),
    };

    let on_disk_bytes = on_disk_size(root);

    Ok(json!({
        "schema_version": schema_version,
        "root": root.display().to_string(),
        "memories": {
            "semantic": semantic_count,
            "procedural": procedural_count,
            "episodic": {
                "hot": hot_count,
                "warm": warm_count,
                "cold_quarters": cold_quarters,
            },
            "total_redb": semantic_count + hot_count + warm_count,
            "large_memory_count": large_memory_count,
        },
        "semantic_index": {
            "applied_lsn": applied_lsn,
            "embed_dim": embed_dim,
            "snapshot_path": snap_path.display().to_string(),
            "snapshot_present": snap_path.exists(),
        },
        "on_disk": {
            "bytes": on_disk_bytes,
            "human": human_bytes(on_disk_bytes),
        }
    }))
}

fn refuse_if_locked(root: &Path) -> Result<()> {
    let lock = root.join(".lock");
    if lock.exists() {
        Err(MnemeError::Lock(format!(
            "{} is held — stop the running mneme instance before reading stats",
            lock.display()
        )))
    } else {
        Ok(())
    }
}

/// Sum of file sizes under `root`, skipping the model cache (we
/// don't want a 2 GB BGE-M3 download to swamp the size headline) and
/// the rotating log dir.
fn on_disk_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Top-level skip — only check at depth 1. `run/` holds
            // daemon runtime state (sockets, auth tokens) that
            // shouldn't inflate the user-facing on-disk headline.
            if dir == root && (name == "models" || name == "logs" || name == "run") {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_root() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        layout::scaffold(&root).unwrap();
        migrate::migrate_to(&root, migrate::CURRENT_SCHEMA_VERSION).unwrap();
        (tmp, root)
    }

    #[test]
    fn empty_root_returns_zeroed_stats() {
        let (_tmp, root) = fresh_root();
        let v = stats_json(&root).unwrap();
        assert_eq!(v["memories"]["semantic"], 0);
        assert_eq!(v["memories"]["procedural"], 0);
        assert_eq!(v["memories"]["episodic"]["hot"], 0);
        assert_eq!(v["memories"]["episodic"]["warm"], 0);
        assert_eq!(v["memories"]["episodic"]["cold_quarters"], 0);
        assert_eq!(v["semantic_index"]["applied_lsn"], 0);
        assert_eq!(v["semantic_index"]["snapshot_present"], false);
        assert_eq!(v["schema_version"], migrate::CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn refuses_when_lockfile_present() {
        let (_tmp, root) = fresh_root();
        std::fs::write(root.join(".lock"), b"42").unwrap();
        let err = stats_json(&root).unwrap_err();
        assert!(matches!(err, MnemeError::Lock(_)));
    }

    fn pinned_line(id: &str, scope: &str) -> String {
        format!(
            "{{\"id\":\"{id}\",\"content\":\"rule\",\"tags\":[],\"scope\":\"{scope}\",\
             \"created_at\":\"2026-07-25T12:00:00Z\"}}"
        )
    }

    #[test]
    fn counts_pinned_jsonl_skipping_blanks_and_comments() {
        let (_tmp, root) = fresh_root();
        let pinned = root
            .join("procedural")
            .join(crate::memory::procedural::PINNED_FILE);
        let body = format!(
            "{}\n# a comment\n\n{}\n",
            pinned_line("01KYCZXK68X0JZFWCX9QMR4VGJ", "global"),
            pinned_line("01KYCZXK68X0JZFWCX9QMR4VGK", "mneme"),
        );
        std::fs::write(&pinned, body.as_bytes()).unwrap();
        let v = stats_json(&root).unwrap();
        assert_eq!(v["memories"]["procedural"], 2);
    }

    /// Regression, 2026-07-25: `mneme stats` opened `<root>/episodic`
    /// with the plaintext `RedbStorage::open`, ignoring
    /// `keystore.json`. On an encrypted data dir the redb WAL replay
    /// then fed AEAD ciphertext to postcard and the whole command
    /// died with `write-ahead log error: postcard decode: Serde
    /// Deserialization Error` — while `mneme run`/`mneme daemon`,
    /// which go through `crypto::boot`, read the exact same bytes
    /// fine.
    #[test]
    fn stats_reads_an_encrypted_data_dir() {
        use crate::cli::encrypt::MnemonicPrompt;
        use crate::crypto::keyring::InMemoryKekStore;

        // `no_verify = true` below means the prompt is never called.
        struct UnusedPrompt;
        impl MnemonicPrompt for UnusedPrompt {
            fn confirm_written(&mut self) -> Result<()> {
                unreachable!("no_verify=true must not prompt")
            }
            fn answer_position(&mut self, _: usize) -> Result<String> {
                unreachable!("no_verify=true must not prompt")
            }
        }

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let keyring = InMemoryKekStore::new();
        // `encrypt_at` scaffolds, writes keystore.json, seeds the
        // keyring and migrates the (empty) tree to encrypted form.
        crate::cli::encrypt::encrypt_at(&root, false, true, &keyring, &mut UnusedPrompt).unwrap();

        // Write through the encrypted stack so the WAL carries real
        // sealed frames — an empty WAL would pass even unfixed.
        let (storage, aead) =
            crate::crypto::boot::open_episodic_storage_and_aead(&root, &keyring).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for i in 0..8 {
                storage
                    .put(
                        format!("mem:{i:020}").into_bytes().as_slice(),
                        format!("value-{i}").as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            storage.flush().await.unwrap();
        });
        drop(storage);
        drop(aead);
        drop(rt);

        let v = stats_json_with_keyring(&root, &keyring)
            .expect("stats must read an encrypted data dir");
        assert_eq!(v["memories"]["semantic"], 8);
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }

    #[test]
    fn on_disk_size_excludes_run_dir() {
        // Regression for v1.0 → v1.1 rollback discovery 2026-05-09:
        // ~/.mneme/run/ holds daemon runtime state and shouldn't
        // inflate the user-facing on-disk byte count.
        let (_tmp, root) = fresh_root();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(root.join("run").join("auth.token"), vec![0u8; 4096]).unwrap();
        std::fs::write(root.join("config.toml"), b"[foo]\n").unwrap();
        let bytes = on_disk_size(&root);
        assert!(bytes < 4096, "expected run/ skipped, got {bytes}");
    }

    #[test]
    fn on_disk_size_excludes_models_and_logs() {
        let (_tmp, root) = fresh_root();
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("models").join("big.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(root.join("logs").join("noise.log"), vec![0u8; 4096]).unwrap();
        std::fs::write(root.join("config.toml"), b"[foo]\n").unwrap();
        let bytes = on_disk_size(&root);
        // 8 KiB excluded; only the small config.toml is counted.
        assert!(bytes < 4096, "expected models/+logs/ skipped, got {bytes}");
    }
}
