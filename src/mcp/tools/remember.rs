//! `remember` — Phase 3. Persists a memory through `SemanticStore`,
//! returning the assigned [`MemoryId`] on success.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::semantic::{MemoryKind, SemanticStore};
use crate::scope::ScopeState;

const DESCRIPTION: &str = "Store a piece of information for future recall. \
Use this when the user shares a fact, makes a decision, or expresses a \
preference that should persist across sessions. Do NOT use for transient \
information from tool outputs (those are handled automatically). Do NOT \
use to store the contents of source code files (those are read live from \
disk).";

pub struct Remember {
    store: Arc<SemanticStore>,
    /// Active default scope. Tools fall back to
    /// `scope_state.current()` when the caller omits the `scope`
    /// argument; `switch_scope` mutates this. Initialised from
    /// `[scopes] default` in `config.toml` at boot.
    scope_state: Arc<ScopeState>,
}

impl Remember {
    pub fn new(store: Arc<SemanticStore>, scope_state: Arc<ScopeState>) -> Self {
        Self { store, scope_state }
    }
}

#[async_trait]
impl Tool for Remember {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "remember",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The information to remember." },
                    "type": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "conversation"],
                        "description": "Memory type. Defaults to fact."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for retrieval."
                    },
                    "scope": { "type": "string", "description": "Optional scope override. Defaults to the session's current scope (set by `switch_scope`)." },
                    "pinned": { "type": "boolean", "description": "Promote to procedural memory." }
                },
                "required": ["content"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`content` is required".into()))?;
        if content.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`content` must not be empty".into(),
            ));
        }

        let kind = match args.get("type").and_then(Value::as_str) {
            None => MemoryKind::Fact,
            Some(s) => MemoryKind::parse(s).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "`type` must be one of fact|decision|preference|conversation, got `{s}`"
                ))
            })?,
        };

        let tags = super::parse_tags_arg(args.get("tags"))?;

        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .map(|s| s.to_owned())
            .unwrap_or_else(|| self.scope_state.current());

        // `pinned` is recognised by the schema but not yet wired to the
        // procedural layer (Phase 4). Surface that explicitly so callers
        // who set it know it's been observed-but-deferred.
        if args.get("pinned").and_then(Value::as_bool) == Some(true) {
            tracing::warn!("remember: `pinned=true` ignored — procedural layer lands in Phase 4");
        }

        let id = self
            .store
            .remember(content, kind, tags, scope)
            .await
            .map_err(|e| ToolError::Internal(format!("remember failed: {e}")))?;

        Ok(ToolResult::text(format!("stored memory {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;
    use ulid::Ulid;

    fn store(tmp: &TempDir) -> Arc<SemanticStore> {
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        SemanticStore::open_disabled(tmp.path(), storage, embedder).unwrap()
    }

    fn make_scope() -> Arc<ScopeState> {
        ScopeState::new("personal")
    }

    #[tokio::test]
    async fn returns_parseable_ulid() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let res = r.invoke(json!({ "content": "hello" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let id_str = text
            .split_whitespace()
            .nth(2)
            .expect("expected `stored memory <ULID>`");
        Ulid::from_string(id_str).expect("expected valid ULID");
    }

    #[tokio::test]
    async fn missing_content_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let err = r.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn empty_content_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let err = r.invoke(json!({ "content": "   " })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn unknown_type_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let err = r
            .invoke(json!({ "content": "x", "type": "weird" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn tags_must_be_strings() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let err = r
            .invoke(json!({ "content": "x", "tags": [1, 2] }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn falls_back_to_current_scope_when_arg_omitted() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let scope = ScopeState::new("personal");
        scope.set("work").unwrap();
        let r = Remember::new(Arc::clone(&s), Arc::clone(&scope));
        r.invoke(json!({ "content": "no scope passed" }))
            .await
            .unwrap();
        let hits = s
            .recall(
                "no scope passed",
                5,
                &crate::memory::semantic::RecallFilters::default(),
            )
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].item.scope, "work");
    }

    /// Some MCP clients (observed: certain Claude Code releases)
    /// double-encode array tool arguments before forwarding the
    /// `tools/call` frame, so `tags` arrives as a JSON-encoded string
    /// rather than a real array. The shared `parse_tags_arg` helper
    /// tolerates that shape; this test pins the call site to use it.
    #[tokio::test]
    async fn harness_double_encoded_tags_are_accepted() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope());
        r.invoke(json!({
            "content": "double-encoded tags should still land",
            "tags": "[\"workaround\",\"harness\"]"
        }))
        .await
        .unwrap();
        let hits = s
            .recall(
                "double-encoded tags should still land",
                5,
                &crate::memory::semantic::RecallFilters::default(),
            )
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0].item.tags,
            vec!["workaround".to_string(), "harness".to_string()]
        );
    }

    #[tokio::test]
    async fn round_trip_through_recall() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope());
        r.invoke(json!({
            "content": "the build is green",
            "type": "fact",
            "tags": ["ci"],
            "scope": "work"
        }))
        .await
        .unwrap();

        let hits = s
            .recall(
                "the build is green",
                5,
                &crate::memory::semantic::RecallFilters::default(),
            )
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].item.scope, "work");
        assert_eq!(hits[0].item.tags, vec!["ci".to_string()]);
    }
}
