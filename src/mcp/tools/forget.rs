//! `forget` — Phase 3. Deletes a memory by ULID.
//!
//! The schema admits `id`, `query`, and `scope` to keep the v1 surface
//! stable for the agent — but `query`/`scope` (bulk delete by search
//! or by scope) need the orchestrator that lands in Phase 5, so this
//! implementation rejects them with a clear "not yet supported"
//! message rather than pretending to act.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use ulid::Ulid;

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::ids::MemoryId;
use crate::memory::semantic::SemanticStore;

const DESCRIPTION: &str = "Delete memories. Provide exactly one of: `id` to \
delete a specific memory, `query` to delete memories matching a search, or \
`scope` to clear an entire scope. Deletion is permanent — confirm with the \
user before invoking.";

pub struct Forget {
    store: Arc<SemanticStore>,
}

impl Forget {
    pub fn new(store: Arc<SemanticStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for Forget {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "forget",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory ULID." },
                    "query": { "type": "string", "description": "Search query." },
                    "scope": { "type": "string", "description": "Scope to clear." }
                }
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let id = args.get("id").and_then(Value::as_str);
        let query = args.get("query").and_then(Value::as_str);
        let scope = args.get("scope").and_then(Value::as_str);

        let count = [id, query, scope].iter().filter(|x| x.is_some()).count();
        if count != 1 {
            return Err(ToolError::InvalidArguments(
                "exactly one of `id`, `query`, `scope` is required".into(),
            ));
        }

        if query.is_some() || scope.is_some() {
            return Err(ToolError::InvalidArguments(
                "bulk forget by `query`/`scope` is not yet supported; pass `id` instead".into(),
            ));
        }

        let id_str = id.unwrap();
        let ulid = Ulid::from_string(id_str)
            .map_err(|e| ToolError::InvalidArguments(format!("`id` is not a valid ULID: {e}")))?;
        let memory_id = MemoryId(ulid);

        let existed = self
            .store
            .forget(memory_id)
            .await
            .map_err(|e| ToolError::Internal(format!("forget failed: {e}")))?;

        Ok(ToolResult::text(if existed {
            format!("forgot memory {memory_id}")
        } else {
            format!("no such memory {memory_id}")
        }))
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

    fn store(tmp: &TempDir) -> Arc<SemanticStore> {
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        SemanticStore::open_disabled(tmp.path(), storage, embedder).unwrap()
    }

    #[tokio::test]
    async fn invalid_ulid_is_invalid_args() {
        let tmp = TempDir::new().unwrap();
        let f = Forget::new(store(&tmp));
        let err = f.invoke(json!({ "id": "not-a-ulid" })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn unknown_id_returns_no_such_memory() {
        let tmp = TempDir::new().unwrap();
        let f = Forget::new(store(&tmp));
        let res = f
            .invoke(json!({ "id": "01H0000000000000000000000Z" }))
            .await
            .unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        assert!(text.starts_with("no such memory"));
    }

    #[tokio::test]
    async fn round_trip_forget_after_remember() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("ephemeral", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let f = Forget::new(Arc::clone(&s));
        let res = f.invoke(json!({ "id": id.to_string() })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        assert!(text.starts_with("forgot memory"));
        assert!(s.get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn zero_args_invalid() {
        let tmp = TempDir::new().unwrap();
        let f = Forget::new(store(&tmp));
        let err = f.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn two_args_invalid() {
        let tmp = TempDir::new().unwrap();
        let f = Forget::new(store(&tmp));
        let err = f
            .invoke(json!({ "id": "01H...", "query": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn query_arg_rejected_as_unsupported() {
        let tmp = TempDir::new().unwrap();
        let f = Forget::new(store(&tmp));
        let err = f.invoke(json!({ "query": "x" })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
