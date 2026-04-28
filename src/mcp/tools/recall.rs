//! `recall` — Phase 3. Returns the top-N memories most similar to a
//! natural-language query, optionally filtered by scope or type.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::semantic::{MemoryKind, RecallFilters, SemanticStore};

const DESCRIPTION: &str = "Retrieve memories semantically similar to a query. \
Use this when you need context the user previously shared but isn't in the \
current conversation. Returns an empty list when nothing matches — that is \
not an error.";

/// Cap on the result count. Above ~100 the LLM context burn dwarfs any
/// retrieval signal. Matches spec §6.1's `recall.limit.maximum`.
const MAX_LIMIT: u64 = 100;

/// Default when the caller omits `limit`. Aligns with
/// [`crate::config::BudgetsConfig::default_recall_limit`].
const DEFAULT_LIMIT: u64 = 10;

pub struct Recall {
    store: Arc<SemanticStore>,
}

impl Recall {
    pub fn new(store: Arc<SemanticStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for Recall {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recall",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query." },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Max results. Defaults to 10."
                    },
                    "scope": { "type": "string", "description": "Optional scope filter." },
                    "type": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "conversation"],
                        "description": "Optional type filter."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`query` is required".into()))?;
        if query.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`query` must not be empty".into(),
            ));
        }

        let limit = match args.get("limit") {
            None => DEFAULT_LIMIT,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                ToolError::InvalidArguments("`limit` must be a positive integer".into())
            })?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`limit` must be a positive integer".into(),
                ));
            }
        };
        if limit == 0 || limit > MAX_LIMIT {
            return Err(ToolError::InvalidArguments(format!(
                "`limit` must be between 1 and {MAX_LIMIT}"
            )));
        }

        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .map(|s| s.to_owned());
        let kind = match args.get("type").and_then(Value::as_str) {
            None => None,
            Some(s) => Some(MemoryKind::parse(s).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "`type` must be one of fact|decision|preference|conversation, got `{s}`"
                ))
            })?),
        };

        let filters = RecallFilters { scope, kind };
        let hits = self
            .store
            .recall(query, limit as usize, &filters)
            .await
            .map_err(|e| ToolError::Internal(format!("recall failed: {e}")))?;

        // Emit one structured JSON document so MCP hosts can either
        // parse it as JSON or hand it to the model verbatim. An empty
        // list serialises to `[]` — explicit and unambiguous.
        let body: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.item.id.to_string(),
                    "content": h.item.content,
                    "type": h.item.kind.as_str(),
                    "tags": h.item.tags,
                    "scope": h.item.scope,
                    "created_at": h.item.created_at.to_rfc3339(),
                    "score": h.score,
                })
            })
            .collect();

        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise hits: {e}")))?;
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

    fn fresh_store(tmp: &TempDir) -> Arc<SemanticStore> {
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        SemanticStore::open_disabled(tmp.path(), storage, embedder).unwrap()
    }

    #[tokio::test]
    async fn empty_store_returns_empty_array() {
        let tmp = TempDir::new().unwrap();
        let r = Recall::new(fresh_store(&tmp));
        let res = r.invoke(json!({ "query": "anything" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_query_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Recall::new(fresh_store(&tmp));
        let err = r.invoke(json!({ "query": "" })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn limit_out_of_range_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Recall::new(fresh_store(&tmp));
        let err = r
            .invoke(json!({ "query": "x", "limit": 0 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = r
            .invoke(json!({ "query": "x", "limit": 9999 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn returns_remembered_item_in_json_array() {
        let tmp = TempDir::new().unwrap();
        let s = fresh_store(&tmp);
        let id = s
            .remember("hello world", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();

        let r = Recall::new(Arc::clone(&s));
        let res = r.invoke(json!({ "query": "hello world" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["id"], id.to_string());
        assert_eq!(arr[0]["content"], "hello world");
        assert_eq!(arr[0]["type"], "fact");
    }

    #[tokio::test]
    async fn type_filter_applied() {
        let tmp = TempDir::new().unwrap();
        let s = fresh_store(&tmp);
        s.remember("note A", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let dec = s
            .remember("note B", MemoryKind::Decision, vec![], "personal".into())
            .await
            .unwrap();

        let r = Recall::new(Arc::clone(&s));
        let res = r
            .invoke(json!({ "query": "note", "type": "decision" }))
            .await
            .unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        for hit in arr {
            assert_eq!(hit["type"], "decision");
        }
        assert!(arr.iter().any(|h| h["id"] == dec.to_string()));
    }
}
