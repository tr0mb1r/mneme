//! `mneme export` — dump every memory across all three layers to
//! stdout as a single JSON document (or NDJSON, one row per line).
//!
//! Same output shape as the MCP `export` tool, but reads directly off
//! disk so it works while the server is stopped. Refuses to run while
//! `~/.mneme/.lock` is held — concurrent reads against an active WAL
//! writer would race the consolidation scheduler. Stop the server
//! first (`mneme stop`).
//!
//! Two formats:
//! * `--format json` (default) — one pretty-printed object with three
//!   top-level keys (`procedural`, `episodic`, `semantic`).
//! * `--format ndjson` — one memory per line, each line tagged with a
//!   `layer` key. Easier to pipe to `jq` / `grep`.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::MemoryItem;
use crate::storage::Storage;
use crate::storage::layout;
use crate::storage::redb_impl::RedbStorage;
use crate::{MnemeError, Result};

const MEM_KEY_PREFIX: &[u8] = b"mem:";

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
    refuse_if_locked(&root)?;

    let storage: Arc<dyn Storage> = RedbStorage::open(&root.join("episodic"))?;
    let procedural = ProceduralStore::open(&root)?;
    let episodic = EpisodicStore::new(Arc::clone(&storage));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;

    let proc_json = collect_procedural(&procedural, scope.as_deref())?;
    let epi_json = runtime.block_on(collect_episodic(&episodic, scope.as_deref()))?;
    let sem_json = runtime.block_on(collect_semantic(&storage, scope.as_deref()))?;

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

async fn collect_episodic(store: &EpisodicStore, scope: Option<&str>) -> Result<Vec<Value>> {
    let mut events = store.list_all().await?;
    if let Some(s) = scope {
        events.retain(|e| e.scope == s);
    }
    Ok(events
        .into_iter()
        .map(|e| {
            json!({
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
    Ok(items
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id.to_string(),
                "content": m.content,
                "kind": m.kind.as_str(),
                "tags": m.tags,
                "scope": m.scope,
                "created_at": m.created_at.to_rfc3339(),
            })
        })
        .collect())
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
