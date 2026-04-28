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

use crate::index::snapshot;
use crate::memory::episodic::EpisodicStore;
use crate::storage::Storage;
use crate::storage::archive::ColdArchive;
use crate::storage::layout;
use crate::storage::redb_impl::RedbStorage;
use crate::{MnemeError, Result, migrate};

/// Same prefix `memory::semantic` uses. Re-declared so this module
/// doesn't reach into a private const.
const MEM_KEY_PREFIX: &[u8] = b"mem:";

/// Same procedural file the live server reads from.
const PINNED_FILE: &str = "pinned.jsonl";

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

/// Build the stats payload for `root`. Pulled out for testability so
/// we don't have to fork the binary in tests.
pub fn stats_json(root: &Path) -> Result<Value> {
    refuse_if_locked(root)?;

    let schema_version = migrate::current_version(root).unwrap_or(0);

    // L4 semantic — count mem: rows and read snapshot metadata.
    let storage: Arc<dyn Storage> = RedbStorage::open(&root.join("episodic"))?;
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

    // L3 episodic — same backing storage, different prefixes.
    let episodic = EpisodicStore::new(Arc::clone(&storage));
    let (hot_count, warm_count) = runtime.block_on(async {
        let hot = episodic.count_hot().await.unwrap_or(0);
        let warm = episodic.count_warm().await.unwrap_or(0);
        (hot, warm)
    });

    // L0 procedural — count non-blank, non-comment lines in pinned.jsonl.
    let procedural_count = count_pinned(&root.join("procedural").join(PINNED_FILE));

    // Cold tier — count quarterly archives.
    let cold = ColdArchive::new(root);
    let cold_quarters = cold.list_quarters().map(|v| v.len()).unwrap_or(0);

    // Snapshot metadata. Absent file is fine (cold start).
    let snap_path = root.join("semantic").join("hnsw.idx");
    let (applied_lsn, embed_dim) = match snapshot::load(&snap_path) {
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

fn count_pinned(path: &Path) -> usize {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count()
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
            // Top-level skip — only check at depth 1.
            if dir == root && (name == "models" || name == "logs") {
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

    #[test]
    fn counts_pinned_jsonl_skipping_blanks_and_comments() {
        let (_tmp, root) = fresh_root();
        let pinned = root.join("procedural").join(PINNED_FILE);
        std::fs::write(
            &pinned,
            b"{\"id\":\"01H...\"}\n# a comment\n\n{\"id\":\"01J...\"}\n",
        )
        .unwrap();
        let v = stats_json(&root).unwrap();
        assert_eq!(v["memories"]["procedural"], 2);
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
