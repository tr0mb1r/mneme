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

use super::size_tier::{self, DEFAULT_MAX_CHARS, Tier};
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
    /// Hard ceiling on replacement content length; updates above
    /// this are rejected with `memory_too_large` (release-planning
    /// v2.1 §5.4). Mirrors `remember`'s ceiling so the contract is
    /// uniform across writes.
    max_chars: usize,
}

impl Update {
    pub fn new(store: Arc<SemanticStore>) -> Self {
        Self {
            store,
            max_chars: DEFAULT_MAX_CHARS,
        }
    }

    /// Override the over-limit ceiling. Mirrors `Remember::with_max_chars`.
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
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

        // Size-tier check on replacement content only — metadata-only
        // updates (kind/tags/scope) bypass the check entirely. Done
        // before the storage call so we don't waste a re-embed on
        // content that's about to be rejected (§5.5).
        let content_tier = match &content {
            Some(s) => {
                let len = size_tier::count_chars(s);
                let tier = size_tier::classify(len, self.max_chars);
                if tier == Tier::OverLimit {
                    let (text, meta) = size_tier::rejection(len, self.max_chars);
                    return Ok(ToolResult::text(text).with_error().with_meta(meta));
                }
                Some((tier, len))
            }
            None => None,
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

        if let Some((Tier::Warning, len)) = content_tier {
            tracing::info!(
                content_len = len,
                limit = self.max_chars,
                memory_id = %memory_id,
                "update: replacement content is large (warning tier)"
            );
        }

        let mut result = ToolResult::text(if existed {
            format!("updated memory {memory_id}")
        } else {
            format!("no such memory {memory_id}")
        });
        // Only attach an advisory/warning if the update actually
        // landed and the content was the field changing — no point
        // in advising on a no-op or a metadata-only patch.
        if existed
            && let Some((tier, len)) = content_tier
            && let Some(meta) = size_tier::success_meta(tier, len, self.max_chars)
        {
            result = result.with_meta(meta);
        }
        Ok(result)
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

    /// Updating with content over the configured ceiling: rejected
    /// with structured `memory_too_large` meta. The original memory
    /// is unchanged.
    #[tokio::test]
    async fn over_limit_content_rejects_with_memory_too_large() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("original", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let payload = "z".repeat(15_000);
        let res = u
            .invoke(json!({ "id": id.to_string(), "content": &payload }))
            .await
            .unwrap();
        assert!(res.is_error);
        let meta = res.meta.expect("expected error meta");
        assert_eq!(meta["error"]["code"], "memory_too_large");
        assert_eq!(meta["error"]["content_length"], 15_000);
        // Original memory unchanged.
        let item = s.get(id).await.unwrap().unwrap();
        assert_eq!(item.content, "original");
    }

    /// Replacement content in the advisory tier surfaces the meta
    /// annotation; metadata-only updates do not.
    #[tokio::test]
    async fn advisory_tier_content_surfaces_length_advisory() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("hi", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let payload = "a".repeat(700);
        let res = u
            .invoke(json!({ "id": id.to_string(), "content": payload }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected length_advisory meta");
        assert!(meta.get("length_advisory").is_some());
    }

    #[tokio::test]
    async fn metadata_only_update_has_no_size_meta() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .remember("hi", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let u = Update::new(Arc::clone(&s));
        let res = u
            .invoke(json!({ "id": id.to_string(), "tags": ["x"] }))
            .await
            .unwrap();
        assert!(res.meta.is_none());
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
