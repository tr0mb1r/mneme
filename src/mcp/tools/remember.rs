//! `remember` — Phase 3. Persists a memory through `SemanticStore`,
//! returning the assigned [`MemoryId`] on success.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::size_tier::{self, DEFAULT_MAX_CHARS, Tier};
use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::semantic::{MemoryKind, SemanticStore};
use crate::scope::ScopeState;

const DESCRIPTION: &str = "Store a piece of information for future recall. \
Use when the user shares a fact, makes a decision, or expresses a \
preference that should persist across sessions.\n\
\n\
SIZE: Target under 500 characters. Mneme stores concise facts, not \
source material.\n\
- Good: \"user prefers tabs over spaces\" (32 chars).\n\
- Bad: pasting a 4,000-char Slack thread. Instead, extract the insight: \
\"team agreed 2026-04-29 to migrate auth to Auth0 by Q3\".\n\
- 500-2,000 chars: accepted; the response carries a `length_advisory` \
field suggesting future memories be more concise.\n\
- 2,000-10,000 chars: accepted; the response carries a stronger \
`length_warning` field.\n\
- Over 10,000 chars: rejected with a structured error. Extract a key \
insight or store a brief summary plus a source reference instead.\n\
\n\
DO NOT use for: transient information from tool outputs (those are \
captured automatically), or contents of source code files (those are \
read live from disk).";

pub struct Remember {
    store: Arc<SemanticStore>,
    /// Active default scope. Tools fall back to
    /// `scope_state.current()` when the caller omits the `scope`
    /// argument; `switch_scope` mutates this. Initialised from
    /// `[scopes] default` in `config.toml` at boot.
    scope_state: Arc<ScopeState>,
    /// Hard ceiling on content length; writes above this are
    /// rejected with `memory_too_large` (release-planning v2.1 §5.4).
    /// Configured via `[budgets] max_remember_chars`.
    max_chars: usize,
}

impl Remember {
    pub fn new(store: Arc<SemanticStore>, scope_state: Arc<ScopeState>) -> Self {
        Self {
            store,
            scope_state,
            max_chars: DEFAULT_MAX_CHARS,
        }
    }

    /// Override the over-limit ceiling. The 500/2,000-character
    /// advisory and warning bounds are fixed (per §5.3).
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
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

        // Size-tier check happens BEFORE embedding so we don't waste
        // a forward pass on content we're about to reject (§5.5).
        let len = size_tier::count_chars(content);
        let tier = size_tier::classify(len, self.max_chars);
        if tier == Tier::OverLimit {
            let (text, meta) = size_tier::rejection(len, self.max_chars);
            tracing::warn!(
                tool = "remember",
                content_chars = len,
                max_chars = self.max_chars,
                "rejected: content over size limit"
            );
            return Ok(ToolResult::text(text).with_error().with_meta(meta));
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

        if tier == Tier::Warning {
            tracing::info!(
                content_len = len,
                limit = self.max_chars,
                memory_id = %id,
                "remember: large memory stored (warning tier)"
            );
        }

        let mut result = ToolResult::text(format!("stored memory {id}"));
        if let Some(meta) = size_tier::success_meta(tier, len, self.max_chars) {
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

    /// Content under 500 chars: stored, no `_meta` annotation.
    #[tokio::test]
    async fn small_content_has_no_size_meta() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let res = r.invoke(json!({ "content": "short fact" })).await.unwrap();
        assert!(!res.is_error);
        assert!(res.meta.is_none(), "small content must not carry size meta");
    }

    /// Content in [500, 2_000): stored with `length_advisory` meta.
    #[tokio::test]
    async fn advisory_tier_attaches_length_advisory_meta() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let payload = "a".repeat(700);
        let res = r.invoke(json!({ "content": payload })).await.unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected length_advisory meta");
        assert!(meta.get("length_advisory").is_some());
        assert!(meta.get("length_warning").is_none());
        assert_eq!(meta["length_advisory"]["content_length"], 700);
        assert_eq!(meta["length_advisory"]["limit"], 10_000);
    }

    /// Content in [2_000, 10_000]: stored with `length_warning` meta.
    #[tokio::test]
    async fn warning_tier_attaches_length_warning_meta() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope());
        let payload = "x".repeat(5_000);
        let res = r.invoke(json!({ "content": payload })).await.unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected length_warning meta");
        assert!(meta.get("length_warning").is_some());
        assert!(meta.get("length_advisory").is_none());
        assert_eq!(meta["length_warning"]["content_length"], 5_000);
    }

    /// Content over the configured ceiling: rejected with structured
    /// `memory_too_large` error meta. Storage is NOT touched.
    #[tokio::test]
    async fn over_limit_rejects_with_memory_too_large() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope());
        let payload = "y".repeat(15_000);
        let res = r.invoke(json!({ "content": &payload })).await.unwrap();
        assert!(res.is_error, "over-limit content must mark is_error");
        let meta = res.meta.expect("expected error meta");
        assert_eq!(meta["error"]["code"], "memory_too_large");
        assert_eq!(meta["error"]["content_length"], 15_000);
        assert_eq!(meta["error"]["limit"], 10_000);
        // The text content also surfaces the rejection so dumb
        // clients that ignore _meta still see it.
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        assert!(text.contains("exceeds 10000"));
        // And the store was never written to — no recall hit.
        let hits = s
            .recall(
                &payload[..50],
                5,
                &crate::memory::semantic::RecallFilters::default(),
            )
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "rejected content must not have been embedded/stored"
        );
    }

    /// Custom ceiling propagates through `with_max_chars`.
    #[tokio::test]
    async fn with_max_chars_overrides_default_ceiling() {
        let tmp = TempDir::new().unwrap();
        let r = Remember::new(store(&tmp), make_scope()).with_max_chars(100);
        let payload = "z".repeat(200);
        let res = r.invoke(json!({ "content": payload })).await.unwrap();
        assert!(res.is_error);
        let meta = res.meta.unwrap();
        assert_eq!(meta["error"]["limit"], 100);
        assert_eq!(meta["error"]["content_length"], 200);
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
