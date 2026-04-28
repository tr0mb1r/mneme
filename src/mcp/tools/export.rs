//! `export` — Phase 6. Emit every memory across all three layers as
//! a single structured JSON document. Designed for one-shot
//! portability: the JSON the agent receives can be diffed across
//! versions, fed to a script for migration, or piped to `jq`.
//!
//! For at-rest backups use the `mneme backup` CLI command instead —
//! it captures the on-disk state byte-for-byte (incl. cold-tier
//! zstd archives, the WAL, the snapshot). `export` is for
//! human-readable, host-LLM-readable shape.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::MemoryItem;
use crate::storage::Storage;

const DESCRIPTION: &str = "Export every memory the agent currently \
holds as one JSON document, optionally filtered by scope. Use this \
for human-readable inspection or for piping to `jq` / external \
scripts. For at-rest, full-fidelity copies use the `mneme backup` \
CLI command instead.";

/// Default cap on rows per layer when the caller doesn't pass
/// `limit`. Picked low enough that an LLM context fits the response.
const DEFAULT_LIMIT_PER_LAYER: usize = 200;

const MEM_KEY_PREFIX: &[u8] = b"mem:";

pub struct Export {
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    storage: Arc<dyn Storage>,
}

impl Export {
    pub fn new(
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
        storage: Arc<dyn Storage>,
    ) -> Self {
        Self {
            procedural,
            episodic,
            storage,
        }
    }
}

#[async_trait]
impl Tool for Export {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "export",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Optional scope filter." },
                    "limit_per_layer": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 5000,
                        "description": "Max rows to include per layer. Defaults to 200."
                    }
                }
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let scope_filter = args.get("scope").and_then(Value::as_str).map(String::from);
        let limit = match args.get("limit_per_layer") {
            None => DEFAULT_LIMIT_PER_LAYER,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                ToolError::InvalidArguments("`limit_per_layer` must be a positive integer".into())
            })? as usize,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`limit_per_layer` must be a positive integer".into(),
                ));
            }
        };
        if !(1..=5000).contains(&limit) {
            return Err(ToolError::InvalidArguments(
                "`limit_per_layer` must be between 1 and 5000".into(),
            ));
        }

        // L0 procedural.
        let proc_items = self
            .procedural
            .list(scope_filter.as_deref())
            .map_err(|e| ToolError::Internal(format!("procedural list: {e}")))?;
        let proc_json: Vec<Value> = proc_items
            .into_iter()
            .take(limit)
            .map(|p| {
                json!({
                    "id": p.id.to_string(),
                    "content": p.content,
                    "tags": p.tags,
                    "scope": p.scope,
                    "created_at": p.created_at.to_rfc3339(),
                })
            })
            .collect();

        // L3 episodic — newest first.
        let mut all_episodic = self
            .episodic
            .list_all()
            .await
            .map_err(|e| ToolError::Internal(format!("episodic list_all: {e}")))?;
        if let Some(ref s) = scope_filter {
            all_episodic.retain(|e| &e.scope == s);
        }
        let epi_json: Vec<Value> = all_episodic
            .into_iter()
            .take(limit)
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
            .collect();

        // L4 semantic — scan b"mem:" and decode. Apply scope filter
        // BEFORE the limit so a scope-filtered export isn't biased
        // towards whatever happens to come first off the prefix scan.
        let raw = self
            .storage
            .scan_prefix(MEM_KEY_PREFIX)
            .await
            .map_err(|e| ToolError::Internal(format!("semantic scan: {e}")))?;
        let mut sem_items: Vec<MemoryItem> = Vec::new();
        for (_k, v) in raw {
            let item: MemoryItem = postcard::from_bytes(&v)
                .map_err(|e| ToolError::Internal(format!("decode MemoryItem: {e}")))?;
            if let Some(ref s) = scope_filter
                && &item.scope != s
            {
                continue;
            }
            sem_items.push(item);
        }
        sem_items.sort_by_key(|m| std::cmp::Reverse(m.created_at));
        let sem_json: Vec<Value> = sem_items
            .into_iter()
            .take(limit)
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
            .collect();

        let body = json!({
            "procedural": proc_json,
            "episodic": epi_json,
            "semantic": sem_json,
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise export: {e}")))?;
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::{MemoryKind, SemanticStore};
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    async fn fixture() -> (Export, Arc<SemanticStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));

        // Populate.
        semantic
            .remember(
                "hello",
                MemoryKind::Fact,
                vec!["t".into()],
                "personal".into(),
            )
            .await
            .unwrap();
        procedural
            .pin("rule".into(), vec![], "personal".into())
            .await
            .unwrap();
        episodic.record("k", "personal", "\"x\"").await.unwrap();

        (Export::new(procedural, episodic, backing), semantic, tmp)
    }

    #[tokio::test]
    async fn export_returns_each_layer() {
        let (e, _semantic, _tmp) = fixture().await;
        let res = e.invoke(json!({})).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["procedural"].as_array().unwrap().len(), 1);
        assert_eq!(v["episodic"].as_array().unwrap().len(), 1);
        assert_eq!(v["semantic"].as_array().unwrap().len(), 1);
        assert_eq!(v["semantic"][0]["kind"], "fact");
    }

    #[tokio::test]
    async fn export_filters_by_scope() {
        let (e, semantic, _tmp) = fixture().await;
        // Add a "work" memory; export with scope="work" should
        // surface only that one.
        semantic
            .remember("ship", MemoryKind::Decision, vec![], "work".into())
            .await
            .unwrap();
        let res = e.invoke(json!({ "scope": "work" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["semantic"].as_array().unwrap().len(), 1);
        assert_eq!(v["semantic"][0]["scope"], "work");
        assert_eq!(v["procedural"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn export_limit_enforced() {
        let (e, semantic, _tmp) = fixture().await;
        for i in 0..5 {
            semantic
                .remember(
                    &format!("m{i}"),
                    MemoryKind::Fact,
                    vec![],
                    "personal".into(),
                )
                .await
                .unwrap();
        }
        let res = e.invoke(json!({ "limit_per_layer": 2 })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["semantic"].as_array().unwrap().len() <= 2);
    }

    #[tokio::test]
    async fn export_invalid_limit_rejected() {
        let (e, _, _tmp) = fixture().await;
        let err = e.invoke(json!({ "limit_per_layer": 0 })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = e
            .invoke(json!({ "limit_per_layer": 99999 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
