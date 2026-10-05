//! `remember` — Phase 3. Persists a memory through `SemanticStore`,
//! returning the assigned [`MemoryId`] on success.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::size_tier::{self, DEFAULT_MAX_CHARS, Tier};
use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::memory::semantic::{
    MemoryKind, NEAR_DUPLICATE_SIMILARITY, NearDuplicate, SemanticStore,
};
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
REPLACING A FACT: when this memory replaces an older one (a new \
balance, a changed decision, a corrected detail), pass its id in \
`supersedes`. The old memory stays readable by id but drops out of \
`recall` and auto-context, so stale and current versions don't compete. \
When a close existing memory is found, the reply says so (and gives its \
id) so you can decide.\n\
\n\
DO NOT use for: transient information from tool outputs (those are \
captured automatically), or contents of source code files (those are \
read live from disk).";

/// How much of an existing memory's content to quote in the reply
/// text when pointing the agent at it.
const QUOTE_CHARS: usize = 160;

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
            title: "Remember a fact",
            annotations: ToolAnnotations::additive(),
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
                    "pinned": { "type": "boolean", "description": "Promote to procedural memory." },
                    "supersedes": {
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ],
                        "description": "Id (or ids) of memories this one replaces. They stay \
            readable by id but drop out of recall and auto-context."
                    }
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

        // `pinned` is recognised by the schema but not wired to the
        // procedural layer. Warn loudly rather than silently dropping it,
        // so a caller who set it learns the flag does nothing. Tracked in
        // book/src/roadmap.md; call `pin` for an L0 rule.
        if args.get("pinned").and_then(Value::as_bool) == Some(true) {
            tracing::warn!(
                "remember: `pinned=true` ignored — call the `pin` tool to add an L0 rule"
            );
        }

        // Validate `supersedes` before writing anything, so a typo'd id
        // doesn't leave a new memory stored next to the one it was
        // meant to retire.
        let supersedes = super::parse_supersedes(args.get("supersedes"))?;
        for old in &supersedes {
            let exists = self
                .store
                .get(*old)
                .await
                .map_err(|e| ToolError::Internal(format!("remember failed: {e}")))?
                .is_some();
            if !exists {
                return Err(ToolError::InvalidArguments(format!(
                    "`supersedes`: no memory with id {old}"
                )));
            }
        }

        let (id, duplicate) = self
            .store
            .remember_checked(content, kind, tags, scope, true)
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

        for old in &supersedes {
            self.store
                .supersede(*old, id)
                .await
                .map_err(|e| ToolError::Internal(format!("supersede {old} failed: {e}")))?;
        }

        let mut text = format!("stored memory {id}");
        if !supersedes.is_empty() {
            let ids: Vec<String> = supersedes.iter().map(ToString::to_string).collect();
            text.push_str(&format!(" (supersedes {})", ids.join(", ")));
        }

        // Merge the size advisory (if any) and the similarity advisory
        // (if any) into a single `_meta` object. They are independent —
        // a long restatement trips both.
        let mut meta = size_tier::success_meta(tier, len, self.max_chars)
            .and_then(|m| m.as_object().cloned())
            .unwrap_or_default();
        // A neighbour the caller just superseded is the expected
        // outcome, not something to warn about.
        if let Some(near) = duplicate.filter(|d| !supersedes.contains(&d.id)) {
            let (key, advisory, note) = similarity_advisory(&near);
            tracing::info!(
                memory_id = %id,
                existing = %near.id,
                similarity = near.similarity,
                kind = key,
                "remember: stored close to an existing memory"
            );
            meta.insert(key.into(), advisory);
            // Also in the text: some MCP hosts never show `_meta` to the
            // model, and an advisory the agent can't see is useless.
            text.push('\n');
            text.push_str(&note);
        }
        if !supersedes.is_empty() {
            meta.insert(
                "supersedes".into(),
                json!(
                    supersedes
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                ),
            );
        }

        let mut result = ToolResult::text(text);
        if !meta.is_empty() {
            result = result.with_meta(Value::Object(meta));
        }
        Ok(result)
    }
}

/// Build the `_meta` entry and the note appended to the reply text for
/// the nearest existing memory.
///
/// Similarity can't tell a restatement from an updated value (both
/// land around 0.82–0.96 under BGE-M3; see
/// [`crate::memory::semantic::RELATED_SIMILARITY`]), so the guidance is
/// the same either way: supersede, forget the new one, or ignore. Only
/// the key differs: `duplicate_advisory` (≥ [`NEAR_DUPLICATE_SIMILARITY`],
/// its pre-1.5 name and shape) or `related_memory` below that.
fn similarity_advisory(near: &NearDuplicate) -> (&'static str, Value, String) {
    let quoted = quote(&near.content);
    let (key, relation) = if near.similarity >= NEAR_DUPLICATE_SIMILARITY {
        ("duplicate_advisory", "near-duplicate of")
    } else {
        ("related_memory", "close to")
    };
    let message = format!(
        "The new memory is {relation} memory {} (similarity {:.2}). Both are stored. \
         If the new one is an updated version (new value, changed decision), \
         retire the old one: call `update` on the new id with `supersedes: \"{}\"` \
         (or pass `supersedes` to `remember` up front next time). If it only \
         restates the old one, `forget` the new id. If both are true and \
         distinct, ignore this.",
        near.id, near.similarity, near.id
    );
    let note = format!(
        "note: {relation} memory {} (similarity {:.2}): \"{quoted}\". If this \
         replaces it, call `update` on the new id with `supersedes: \"{}\"`; if it \
         only restates it, `forget` the new id.",
        near.id, near.similarity, near.id
    );
    (
        key,
        json!({
            "existing_id": near.id.to_string(),
            "existing_content": near.content,
            "existing_scope": near.scope,
            "similarity": near.similarity,
            "message": message,
        }),
        note,
    )
}

fn quote(content: &str) -> String {
    let one_line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= QUOTE_CHARS {
        one_line
    } else {
        let mut cut: String = one_line.chars().take(QUOTE_CHARS).collect();
        cut.push('…');
        cut
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

    /// Storing the same fact twice must still succeed, but the second
    /// call carries a `duplicate_advisory` so the agent can choose to
    /// `update` instead of growing the corpus forever.
    #[tokio::test]
    async fn second_identical_write_carries_a_duplicate_advisory() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());

        let first = r
            .invoke(json!({ "content": "CI runs on ubuntu-latest only" }))
            .await
            .unwrap();
        assert!(first.meta.is_none(), "first write should carry no advisory");

        let second = r
            .invoke(json!({ "content": "CI runs on ubuntu-latest only" }))
            .await
            .unwrap();
        assert!(!second.is_error, "the duplicate must still be stored");
        let meta = second.meta.expect("expected a duplicate advisory");
        let adv = &meta["duplicate_advisory"];
        assert!(adv.is_object(), "meta was {meta}");
        assert_eq!(adv["existing_content"], "CI runs on ubuntu-latest only");
        assert!(adv["existing_id"].is_string());
        assert!(adv["similarity"].as_f64().unwrap() >= 0.95);
    }

    #[tokio::test]
    async fn unrelated_writes_carry_no_duplicate_advisory() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        r.invoke(json!({ "content": "CI runs on ubuntu-latest only" }))
            .await
            .unwrap();
        let res = r
            .invoke(json!({ "content": "the office wifi password rotates monthly" }))
            .await
            .unwrap();
        let has_dup = res
            .meta
            .as_ref()
            .map(|m| m.get("duplicate_advisory").is_some())
            .unwrap_or(false);
        assert!(!has_dup, "unrelated content flagged: {:?}", res.meta);
    }

    /// A long restatement trips both advisories; they must coexist in
    /// one `_meta` object rather than one clobbering the other.
    #[tokio::test]
    async fn size_and_duplicate_advisories_coexist() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        // 600 chars lands in the advisory tier (500-2,000).
        let long = "x".repeat(600);
        r.invoke(json!({ "content": long })).await.unwrap();
        let second = r.invoke(json!({ "content": long })).await.unwrap();
        let meta = second.meta.expect("expected both advisories");
        assert!(
            meta.get("length_advisory").is_some(),
            "size advisory lost: {meta}"
        );
        assert!(
            meta.get("duplicate_advisory").is_some(),
            "duplicate advisory lost: {meta}"
        );
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

    fn text_of(res: &ToolResult) -> String {
        match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        }
    }

    fn id_of(res: &ToolResult) -> crate::ids::MemoryId {
        let text = text_of(res);
        let raw = text.split_whitespace().nth(2).unwrap();
        crate::ids::MemoryId(Ulid::from_string(raw).unwrap())
    }

    #[tokio::test]
    async fn supersedes_retires_the_old_memory() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let old = id_of(
            &r.invoke(json!({ "content": "balance is 5200" }))
                .await
                .unwrap(),
        );
        let res = r
            .invoke(json!({ "content": "balance is 4800", "supersedes": old.to_string() }))
            .await
            .unwrap();
        let new = id_of(&res);
        assert!(text_of(&res).contains(&format!("(supersedes {old})")));
        assert_eq!(res.meta.as_ref().unwrap()["supersedes"][0], old.to_string());
        // The neighbour it just retired must not be reported back.
        let meta = res.meta.unwrap();
        assert!(meta.get("duplicate_advisory").is_none(), "{meta}");
        assert!(meta.get("related_memory").is_none(), "{meta}");
        assert_eq!(store.superseded_by(old).await.unwrap(), Some(new));
    }

    #[tokio::test]
    async fn supersedes_accepts_an_array() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let a = id_of(&r.invoke(json!({ "content": "alpha" })).await.unwrap());
        let b = id_of(&r.invoke(json!({ "content": "bravo" })).await.unwrap());
        let res = r
            .invoke(json!({ "content": "merged", "supersedes": [a.to_string(), b.to_string()] }))
            .await
            .unwrap();
        let new = id_of(&res);
        assert_eq!(store.superseded_by(a).await.unwrap(), Some(new));
        assert_eq!(store.superseded_by(b).await.unwrap(), Some(new));
    }

    #[tokio::test]
    async fn bad_supersedes_is_rejected_before_anything_is_stored() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        for bad in [
            json!("not-a-ulid"),
            json!(42),
            json!([1]),
            json!(Ulid::new().to_string()),
        ] {
            let err = r
                .invoke(json!({ "content": "x", "supersedes": bad }))
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidArguments(_)), "{bad}");
        }
        assert_eq!(store.len(), 0);
    }

    #[tokio::test]
    async fn duplicate_note_is_also_in_the_reply_text() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let first = id_of(
            &r.invoke(json!({ "content": "CI runs on ubuntu" }))
                .await
                .unwrap(),
        );
        let res = r
            .invoke(json!({ "content": "CI runs on ubuntu" }))
            .await
            .unwrap();
        let text = text_of(&res);
        assert!(text.starts_with("stored memory "), "{text}");
        assert!(
            text.contains(&format!("near-duplicate of memory {first}")),
            "{text}"
        );
        assert!(text.contains("\"CI runs on ubuntu\""), "{text}");
    }

    #[test]
    fn advisory_key_depends_on_similarity() {
        let near = |similarity| NearDuplicate {
            id: crate::ids::MemoryId::new(),
            similarity,
            content: "D account balance is 5,200 EUR".into(),
            scope: "global".into(),
        };
        let (k, v, note) = similarity_advisory(&near(0.97));
        assert_eq!(k, "duplicate_advisory");
        assert!(v["message"].as_str().unwrap().contains("supersedes"));
        assert!(note.contains("near-duplicate of"));
        let (k, v, note) = similarity_advisory(&near(0.86));
        assert_eq!(k, "related_memory");
        assert_eq!(v["existing_content"], "D account balance is 5,200 EUR");
        assert!(note.contains("close to memory"), "{note}");
        assert!(note.contains("`update`"), "{note}");
    }

    #[test]
    fn long_quotes_are_truncated() {
        let q = quote(&"word ".repeat(100));
        assert!(q.chars().count() <= QUOTE_CHARS + 1);
        assert!(q.ends_with('…'));
        assert_eq!(quote("a\n  b"), "a b");
    }
}
