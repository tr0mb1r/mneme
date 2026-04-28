//! `list_scopes` — Phase 6. Returns every distinct `scope` string
//! seen across the three memory layers. Useful for hosts that want
//! to populate a scope picker UI or for the agent to discover what
//! buckets exist.
//!
//! Walks each prefix linearly. At v0.1 cardinalities (≤thousands per
//! layer) that's microseconds; if a future workload pushes this into
//! a hot path we'll back it with a maintained side index.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
use crate::storage::Storage;

const DESCRIPTION: &str = "List every distinct scope that currently \
holds memories. Use this when you need to confirm which buckets exist \
before calling `recall` / `remember` with a `scope` filter.";

/// Scan-prefix key for semantic memories. Mirrors
/// `crate::memory::semantic`'s private constant; duplicated here
/// because the field-by-field decode below only needs the bytes
/// that come BEFORE the postcard payload (we read `scope` directly
/// from each row).
const MEM_KEY_PREFIX: &[u8] = b"mem:";

pub struct ListScopes {
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    storage: Arc<dyn Storage>,
}

impl ListScopes {
    pub fn new(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
        storage: Arc<dyn Storage>,
    ) -> Self {
        Self {
            semantic,
            procedural,
            episodic,
            storage,
        }
    }
}

#[async_trait]
impl Tool for ListScopes {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "list_scopes",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn invoke(&self, _args: Value) -> Result<ToolResult, ToolError> {
        // BTreeSet so the JSON output is deterministic regardless of
        // input order.
        let mut scopes: BTreeSet<String> = BTreeSet::new();

        // L0 procedural.
        for p in self
            .procedural
            .list(None)
            .map_err(|e| ToolError::Internal(format!("procedural list: {e}")))?
        {
            scopes.insert(p.scope);
        }

        // L3 episodic — both hot and warm tiers. Use the full scan so
        // we don't miss scope-only-in-warm.
        for e in self
            .episodic
            .list_all()
            .await
            .map_err(|e| ToolError::Internal(format!("episodic list_all: {e}")))?
        {
            scopes.insert(e.scope);
        }
        for e in self
            .episodic
            .list_warm()
            .await
            .map_err(|e| ToolError::Internal(format!("episodic list_warm: {e}")))?
        {
            scopes.insert(e.scope);
        }

        // L4 semantic — scan the b"mem:" prefix and decode each row.
        // We don't expose a list_all on SemanticStore (semantic
        // memories aren't typically iterated), so the scan happens
        // here. Reading just `scope` could be optimised with a
        // separate index, but for v0.1 cardinalities this is cheap.
        let raw = self
            .storage
            .scan_prefix(MEM_KEY_PREFIX)
            .await
            .map_err(|e| ToolError::Internal(format!("semantic scan: {e}")))?;
        for (_k, v) in raw {
            let item: crate::memory::semantic::MemoryItem = postcard::from_bytes(&v)
                .map_err(|e| ToolError::Internal(format!("decode MemoryItem: {e}")))?;
            scopes.insert(item.scope);
        }

        // The `semantic` Arc is kept on the struct so the type-checker
        // proves we depend on it being open during the scan; it's
        // referenced via `_` here so dropping the import doesn't
        // happen.
        let _ = &self.semantic;

        let body: Vec<&String> = scopes.iter().collect();
        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise scopes: {e}")))?;
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::MemoryKind;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    #[tokio::test]
    async fn returns_distinct_sorted_scopes() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));

        // Plant scopes across all three layers; some duplicates.
        semantic
            .remember("a", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        semantic
            .remember("b", MemoryKind::Fact, vec![], "work".into())
            .await
            .unwrap();
        procedural
            .pin("rule".into(), vec![], "personal".into())
            .await
            .unwrap();
        procedural
            .pin("oss".into(), vec![], "open-source".into())
            .await
            .unwrap();
        episodic.record("k", "personal", "\"x\"").await.unwrap();
        episodic.record("k", "work", "\"y\"").await.unwrap();

        let tool = ListScopes::new(semantic, procedural, episodic, Arc::clone(&backing));
        let res = tool.invoke(json!({})).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let arr: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        // Distinct + sorted.
        assert_eq!(arr, vec!["open-source", "personal", "work"]);
    }

    #[tokio::test]
    async fn empty_stores_return_empty_array() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));

        let tool = ListScopes::new(semantic, procedural, episodic, backing);
        let res = tool.invoke(json!({})).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }
}
