//! `mneme export` — dump every memory across all three layers to
//! stdout as a single JSON document (or NDJSON, one row per line).
//!
//! Same output shape as the MCP `export` tool, but reads directly off
//! disk so it works while the server is stopped. Refuses to run while
//! `~/.mneme/.lock` is held — concurrent reads against an active WAL
//! writer would race the consolidation scheduler. Stop the server
//! first (`mneme stop`).
//!
//! The episodic layer covers the hot and warm tiers; each row has a
//! `tier` key. Until 1.5.2 only the hot tier was dumped, so an export
//! kept as a backup lost every event older than `hot_to_warm_days`.
//! The cold archive (`<root>/cold/*.zst`) is not included; `mneme
//! backup` captures it.
//!
//! There is no import command. To restore, use `mneme backup` /
//! `mneme restore`, which copy the data dir byte-for-byte and so keep
//! every row in the tier it was in.
//!
//! Two formats:
//! * `--format json` (default) — one pretty-printed object with three
//!   top-level keys (`procedural`, `episodic`, `semantic`).
//! * `--format ndjson` — one memory per line, each line tagged with a
//!   `layer` key. Easier to pipe to `jq` / `grep`.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::crypto::{KekStore, OsKeyring};
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::{MemoryItem, read_memory_meta};
use crate::storage::MEM_KEY_PREFIX;
use crate::storage::Storage;
use crate::storage::layout;
use crate::{MnemeError, Result};

pub fn execute(scope: Option<String>, format: String) -> Result<()> {
    let format = match format.to_ascii_lowercase().as_str() {
        "json" => Format::Json,
        "ndjson" => Format::Ndjson,
        other => {
            return Err(MnemeError::Config(format!(
                "--format must be `json` or `ndjson`, got `{other}`"
            )));
        }
    };

    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;

    let Layers {
        proc_json,
        epi_json,
        sem_json,
    } = collect_all(&root, scope.as_deref(), &OsKeyring::new())?;

    match format {
        Format::Json => {
            let body = json!({
                "procedural": proc_json,
                "episodic": epi_json,
                "semantic": sem_json,
            });
            let pretty = serde_json::to_string_pretty(&body)
                .map_err(|e| MnemeError::Storage(format!("serialise export: {e}")))?;
            println!("{pretty}");
        }
        Format::Ndjson => {
            for v in proc_json {
                emit_ndjson_row("procedural", v)?;
            }
            for v in epi_json {
                emit_ndjson_row("episodic", v)?;
            }
            for v in sem_json {
                emit_ndjson_row("semantic", v)?;
            }
        }
    }

    Ok(())
}

/// The three layer payloads, in export order.
pub struct Layers {
    pub proc_json: Vec<Value>,
    pub epi_json: Vec<Value>,
    pub sem_json: Vec<Value>,
}

/// Open the data dir and pull every layer. Split out of [`execute`]
/// so tests can drive it against a temp root without forking the
/// binary or touching `~/.mneme`.
///
/// Both storage surfaces are opened crypto-aware: on an encrypted
/// data dir the plaintext readers hand AEAD ciphertext to postcard
/// and the export dies before emitting a byte (regression fixed
/// 2026-07-25).
pub fn collect_all(root: &Path, scope: Option<&str>, keyring: &dyn KekStore) -> Result<Layers> {
    refuse_if_locked(root)?;

    let (storage, aead) = crate::crypto::boot::open_episodic_storage_and_aead(root, keyring)?;
    let procedural = ProceduralStore::open_with_crypto(root, aead.clone())?;
    let episodic = EpisodicStore::new(Arc::clone(&storage));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;

    Ok(Layers {
        proc_json: collect_procedural(&procedural, scope)?,
        epi_json: runtime.block_on(collect_episodic(&episodic, scope))?,
        sem_json: runtime.block_on(collect_semantic(&storage, scope))?,
    })
}

#[derive(Copy, Clone, Debug)]
enum Format {
    Json,
    Ndjson,
}

fn emit_ndjson_row(layer: &str, mut row: Value) -> Result<()> {
    if let Value::Object(ref mut map) = row {
        map.insert("layer".into(), Value::String(layer.into()));
    }
    let line = serde_json::to_string(&row)
        .map_err(|e| MnemeError::Storage(format!("serialise ndjson row: {e}")))?;
    println!("{line}");
    Ok(())
}

fn refuse_if_locked(root: &Path) -> Result<()> {
    let lock = root.join(".lock");
    if lock.exists() {
        Err(MnemeError::Lock(format!(
            "{} is held — stop the running mneme instance before exporting",
            lock.display()
        )))
    } else {
        Ok(())
    }
}

fn collect_procedural(store: &ProceduralStore, scope: Option<&str>) -> Result<Vec<Value>> {
    let items = store.list(scope)?;
    Ok(items
        .into_iter()
        .map(|p| {
            json!({
                "id": p.id.to_string(),
                "content": p.content,
                "tags": p.tags,
                "scope": p.scope,
                "created_at": p.created_at.to_rfc3339(),
            })
        })
        .collect())
}

/// Hot and warm episodic rows, newest first, each tagged with its
/// tier.
async fn collect_episodic(store: &EpisodicStore, scope: Option<&str>) -> Result<Vec<Value>> {
    let mut events = store.list_hot_and_warm(true).await?;
    if let Some(s) = scope {
        events.retain(|t| t.event.scope == s);
    }
    Ok(events
        .into_iter()
        .map(|t| {
            let e = t.event;
            json!({
                "tier": t.tier.as_str(),
                "id": e.id.to_string(),
                "kind": e.kind,
                "scope": e.scope,
                "payload": e.payload,
                "tags": e.tags,
                "retrieval_weight": e.retrieval_weight,
                "last_accessed": e.last_accessed.to_rfc3339(),
                "created_at": e.created_at.to_rfc3339(),
            })
        })
        .collect())
}

async fn collect_semantic(storage: &Arc<dyn Storage>, scope: Option<&str>) -> Result<Vec<Value>> {
    let raw = storage.scan_prefix(MEM_KEY_PREFIX).await?;
    let mut items: Vec<MemoryItem> = Vec::with_capacity(raw.len());
    for (_k, v) in raw {
        let item: MemoryItem = postcard::from_bytes(&v)
            .map_err(|e| MnemeError::Storage(format!("decode MemoryItem: {e}")))?;
        if let Some(s) = scope
            && item.scope != s
        {
            continue;
        }
        items.push(item);
    }
    items.sort_by_key(|m| std::cmp::Reverse(m.created_at));
    // `updated_at`, `status` and the supersession link live in side
    // tables next to the `mem:` row (see `storage/mod.rs`), so each
    // costs a point read. Archived and superseded memories are always
    // exported: this is a dump, not a recall.
    let mut out = Vec::with_capacity(items.len());
    for m in items {
        let meta = read_memory_meta(storage.as_ref(), &m).await?;
        let mut row = json!({
            "id": m.id.to_string(),
            "content": m.content,
            "kind": m.kind.as_str(),
            "tags": m.tags,
            "scope": m.scope,
            "created_at": m.created_at.to_rfc3339(),
            "updated_at": meta.updated_at.to_rfc3339(),
            "status": meta.status.as_str(),
        });
        if let Some(newer) = meta.superseded_by {
            row["superseded_by"] = json!(newer.to_string());
        }
        out.push(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::procedural::ProceduralStore;
    use crate::memory::semantic::{MemoryItem, MemoryKind};
    use crate::storage::redb_impl::RedbStorage;
    use chrono::Utc;
    use tempfile::TempDir;

    fn semantic_key(id: &crate::ids::MemoryId) -> Vec<u8> {
        let mut k = b"mem:".to_vec();
        k.extend_from_slice(&id.0.to_bytes());
        k
    }

    /// Drives the full collect path against a tmp `~/.mneme/`-shaped
    /// tree (no lockfile) and asserts each layer's payload shape.
    #[tokio::test]
    async fn collect_paths_pull_each_layer() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Mirror the layout::scaffold subdirs the production CLI relies on.
        std::fs::create_dir_all(root.join("episodic")).unwrap();
        std::fs::create_dir_all(root.join("procedural")).unwrap();

        let storage: Arc<dyn Storage> = RedbStorage::open(&root.join("episodic")).unwrap();

        // L4 semantic — one row.
        let item = MemoryItem {
            id: crate::ids::MemoryId::new(),
            content: "ci runs ruff and pytest".into(),
            kind: MemoryKind::Fact,
            tags: vec!["ci".into()],
            scope: "work".into(),
            created_at: Utc::now(),
        };
        let bytes = postcard::to_allocvec(&item).unwrap();
        storage.put(&semantic_key(&item.id), &bytes).await.unwrap();

        // L3 episodic — one row.
        let episodic = EpisodicStore::new(Arc::clone(&storage));
        episodic
            .record_json(
                "tool_call",
                "work",
                &serde_json::json!({"tool": "remember"}),
            )
            .await
            .unwrap();

        // L0 procedural — one row.
        let procedural = ProceduralStore::open(root).unwrap();
        procedural
            .pin(
                "never deploy on Friday".into(),
                vec!["ops".into()],
                "personal".into(),
            )
            .await
            .unwrap();

        let proc_json = collect_procedural(&procedural, None).unwrap();
        assert_eq!(proc_json.len(), 1);
        assert_eq!(proc_json[0]["content"], "never deploy on Friday");

        let epi_json = collect_episodic(&episodic, None).await.unwrap();
        assert_eq!(epi_json.len(), 1);
        assert_eq!(epi_json[0]["kind"], "tool_call");

        let sem_json = collect_semantic(&storage, None).await.unwrap();
        assert_eq!(sem_json.len(), 1);
        assert_eq!(sem_json[0]["content"], "ci runs ruff and pytest");
    }

    /// Regression, 2026-07-25: `export` opened `<root>/episodic` with
    /// the plaintext `RedbStorage::open` and `pinned.jsonl` with the
    /// plaintext `ProceduralStore::open`. On an encrypted data dir the
    /// redb WAL replay fed AEAD ciphertext to postcard and the whole
    /// export died with `write-ahead log error: postcard decode: ...`
    /// — i.e. the backup path was broken for every encrypted install.
    #[test]
    fn export_reads_an_encrypted_data_dir() {
        use crate::cli::encrypt::MnemonicPrompt;
        use crate::crypto::keyring::InMemoryKekStore;

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
        crate::cli::encrypt::encrypt_at(&root, false, true, &keyring, &mut UnusedPrompt).unwrap();

        // Seed one row in each layer through the encrypted stack, then
        // drop the handles so `collect_all` can take the redb lock.
        {
            let (storage, aead) =
                crate::crypto::boot::open_episodic_storage_and_aead(&root, &keyring).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let item = MemoryItem {
                    id: crate::ids::MemoryId::new(),
                    content: "sealed semantic row".into(),
                    kind: MemoryKind::Fact,
                    tags: vec!["ci".into()],
                    scope: "work".into(),
                    created_at: Utc::now(),
                };
                let bytes = postcard::to_allocvec(&item).unwrap();
                storage.put(&semantic_key(&item.id), &bytes).await.unwrap();

                EpisodicStore::new(Arc::clone(&storage))
                    .record_json(
                        "tool_call",
                        "work",
                        &serde_json::json!({"tool": "remember"}),
                    )
                    .await
                    .unwrap();

                ProceduralStore::open_with_crypto(&root, aead.clone())
                    .unwrap()
                    .pin("sealed rule".into(), vec!["ops".into()], "work".into())
                    .await
                    .unwrap();

                storage.flush().await.unwrap();
            });
        }

        let layers =
            collect_all(&root, None, &keyring).expect("export must read an encrypted data dir");
        assert_eq!(layers.sem_json.len(), 1);
        assert_eq!(layers.sem_json[0]["content"], "sealed semantic row");
        assert_eq!(layers.epi_json.len(), 1);
        assert_eq!(layers.proc_json.len(), 1);
        assert_eq!(layers.proc_json[0]["content"], "sealed rule");
    }

    /// The 1.5.2 bug: `mneme export` read the hot tier only, so the
    /// weekly export-as-backup dropped every warm row. Drives the
    /// real `collect_all` path against an on-disk redb.
    #[test]
    fn export_includes_warm_rows() {
        use crate::crypto::keyring::InMemoryKekStore;
        use crate::memory::consolidation::_record_backdated_for_tests;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("episodic")).unwrap();
        std::fs::create_dir_all(root.join("procedural")).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (hot, warm) = {
            let storage: Arc<dyn Storage> = RedbStorage::open(&root.join("episodic")).unwrap();
            rt.block_on(async {
                let episodic = EpisodicStore::new(Arc::clone(&storage));
                let now = Utc::now();
                for d in [1, 2] {
                    _record_backdated_for_tests(
                        &storage,
                        &format!("hot-{d}d"),
                        "work",
                        "\"h\"",
                        now - chrono::Duration::days(d),
                    )
                    .await
                    .unwrap();
                }
                for (d, scope) in [(40, "work"), (50, "personal"), (60, "work")] {
                    let id = _record_backdated_for_tests(
                        &storage,
                        &format!("warm-{d}d"),
                        scope,
                        "\"w\"",
                        now - chrono::Duration::days(d),
                    )
                    .await
                    .unwrap();
                    assert!(episodic.promote_to_warm(id).await.unwrap());
                }
                storage.flush().await.unwrap();
                (
                    episodic.count_hot().await.unwrap(),
                    episodic.count_warm().await.unwrap(),
                )
            })
        };
        assert_eq!((hot, warm), (2, 3));

        let layers = collect_all(root, None, &InMemoryKekStore::new()).unwrap();
        assert_eq!(layers.epi_json.len(), hot + warm);
        let got: Vec<(&str, &str)> = layers
            .epi_json
            .iter()
            .map(|r| (r["kind"].as_str().unwrap(), r["tier"].as_str().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("hot-1d", "hot"),
                ("hot-2d", "hot"),
                ("warm-40d", "warm"),
                ("warm-50d", "warm"),
                ("warm-60d", "warm"),
            ]
        );

        // Second run: same rows, same order.
        let again = collect_all(root, None, &InMemoryKekStore::new()).unwrap();
        assert_eq!(again.epi_json, layers.epi_json);

        // Scope filter reaches warm rows too.
        let personal = collect_all(root, Some("personal"), &InMemoryKekStore::new()).unwrap();
        assert_eq!(personal.epi_json.len(), 1);
        assert_eq!(personal.epi_json[0]["kind"], "warm-50d");
    }

    #[tokio::test]
    async fn collect_paths_apply_scope_filter() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("episodic")).unwrap();
        std::fs::create_dir_all(root.join("procedural")).unwrap();

        let storage: Arc<dyn Storage> = RedbStorage::open(&root.join("episodic")).unwrap();
        let episodic = EpisodicStore::new(Arc::clone(&storage));
        episodic
            .record_json("tool_call", "work", &serde_json::json!({"x": 1}))
            .await
            .unwrap();
        episodic
            .record_json("tool_call", "personal", &serde_json::json!({"x": 2}))
            .await
            .unwrap();

        let work_only = collect_episodic(&episodic, Some("work")).await.unwrap();
        assert_eq!(work_only.len(), 1);
        assert_eq!(work_only[0]["scope"], "work");
    }
}
