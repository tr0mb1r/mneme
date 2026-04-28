//! `update` — Phase 6. Edits an existing memory in place.
//!
//! Distinct from `forget` + `remember`: it preserves the original
//! `created_at`, keeps the same id, and (when `content` changes)
//! re-embeds via a single `VectorReplace` so search returns the new
//! vector immediately — no transient window where both vectors are
//! live in the HNSW.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use ulid::Ulid;

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::ids::MemoryId;
use crate::memory::semantic::{MemoryKind, SemanticStore, UpdatePatch};

const DESCRIPTION: &str = "Edit an existing memory in place. Provide \
`id` and any subset of `content`, `type`, `tags`, `scope` — only \
supplied fields change. Use this when the user revises a fact, narrows \
a preference, or re-classifies a memory; do NOT use this for \
unrelated information (call `remember` for new memories instead). \
Re-embedding happens automatically when `content` changes.";

pub struct Update {
    store: Arc<SemanticStore>,
}

impl Update {
    pub fn new(store: Arc<SemanticStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for Update {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "update",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory ULID." },
                    "content": { "type": "string", "description": "Replacement text. Triggers re-embedding." },
                    "type": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "conversation"],
                        "description": "Replacement memory type."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Replacement tag list. Pass [] to clear."
                    },
                    "scope": { "type": "string", "description": "Replacement scope." }
                },
                "required": ["id"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let id_str = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`id` is required".into()))?;
        let ulid = Ulid::from_string(id_str)
            .map_err(|e| ToolError::InvalidArguments(format!("`id` is not a valid ULID: {e}")))?;
        let memory_id = MemoryId(ulid);

        let content = match args.get("content") {
            None => None,
            Some(Value::String(s)) => {
                if s.trim().is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "`content` must not be empty".into(),
                    ));
                }
                Some(s.clone())
            }
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`content` must be a string".into(),
                ));
            }
        };

        let kind = match args.get("type") {
            None => None,
            Some(Value::String(s)) => Some(MemoryKind::parse(s).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "`type` must be one of fact|decision|preference|conversation, got `{s}`"
                ))
            })?),
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`type` must be a string".into(),
                ));
            }
        };

        let tags = match args.get("tags") {
            None => None,
            Some(Value::Array(arr)) => Some(
                arr.iter()
                    .map(|v| {
                        v.as_str().map(|s| s.to_owned()).ok_or_else(|| {
                            ToolError::InvalidArguments("`tags` must be strings".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`tags` must be an array of strings".into(),
                ));
            }
        };

        let scope = match args.get("scope") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`scope` must be a string".into(),
                ));
            }
        };

        let patch = UpdatePatch {
            content,
            kind,
            tags,
            scope,
        };

        let existed = self
            .store
            .update(memory_id, patch)
            .await
            .map_err(|e| ToolError::Internal(format!("update failed: {e}")))?;

        Ok(ToolResult::text(if existed {
            format!("updated memory {memory_id}")
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
    use crate::memory::semantic::{MemoryKind, RecallFilters};
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn store(tmp: &TempDir) -> Arc<SemanticStore> {
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        SemanticStore::open_disabled(tmp.path(), storage, embedder).unwrap()
    }

    #[tokio::test]
    async fn missing_id_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let u = Update::new(store(&tmp));
        let err = u.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invalid_ulid_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let u = Update::new(store(&tmp));
        let err = u
            .invoke(json!({ "id": "not-a-ulid", "content": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn unknown_id_returns_no_such_memory() {
        let tmp = TempDir::new().unwrap();
        let u = Update::new(store(&tmp));
        let res = u
            .invoke(json!({ "id": "01H0000000000000000000000Z", "content": "hi" }))
            .await
            .unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        assert!(text.starts_with("no such memory"));
    }

    #[tokio::test]
    async fn empty_content_rejected() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("hi", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let err = u
            .invoke(json!({ "id": id.to_string(), "content": "   " }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn unknown_type_rejected() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("hi", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let err = u
            .invoke(json!({ "id": id.to_string(), "type": "weird" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn round_trip_content_swap_via_recall() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember(
                "stale alpha bravo",
                MemoryKind::Fact,
                vec!["t1".into()],
                "personal".into(),
            )
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let res = u
            .invoke(json!({
                "id": id.to_string(),
                "content": "fresh kilo lima",
            }))
            .await
            .unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        assert!(text.starts_with("updated memory"));

        let hits = s
            .recall("fresh kilo lima", 5, &RecallFilters::default())
            .await
            .unwrap();
        let hit = hits.iter().find(|h| h.item.id == id).expect("post-update");
        assert!(hit.score < 0.001, "self-distance ~0, got {}", hit.score);
        assert_eq!(hit.item.content, "fresh kilo lima");
        // Untouched fields preserved.
        assert_eq!(hit.item.tags, vec!["t1".to_string()]);
        assert_eq!(hit.item.scope, "personal");
    }

    #[tokio::test]
    async fn metadata_only_update_preserves_content() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember(
                "important policy",
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        u.invoke(json!({
            "id": id.to_string(),
            "type": "decision",
            "tags": ["governance"],
            "scope": "work"
        }))
        .await
        .unwrap();

        let item = s.get(id).await.unwrap().unwrap();
        assert_eq!(item.content, "important policy");
        assert_eq!(item.kind, MemoryKind::Decision);
        assert_eq!(item.tags, vec!["governance".to_string()]);
        assert_eq!(item.scope, "work");
    }

    #[tokio::test]
    async fn tags_must_be_strings() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("x", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let err = u
            .invoke(json!({ "id": id.to_string(), "tags": [1, 2] }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
