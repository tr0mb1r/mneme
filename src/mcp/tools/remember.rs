//! `remember` — Phase 3. Persists a memory through `SemanticStore`,
//! returning the assigned [`MemoryId`] on success.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::secret_guard::{self, Guard};
use super::size_tier::{self, DEFAULT_MAX_CHARS, Tier};
use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::config::SecretsMode;
use crate::ids::MemoryId;
use crate::memory::semantic::{
    MemoryKind, NEAR_DUPLICATE_SIMILARITY, NearDuplicate, RememberOutcome, SemanticStore,
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
RETRIES: storing content identical to an existing current memory in the \
same scope (ignoring leading/trailing and repeated whitespace) writes \
nothing; the reply says `already stored as <id>` with `deduplicated: true`, \
so a retried call is safe. The first memory's type and tags stand. Not \
applied when `supersedes` is passed.\n\
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
    /// `[guardrails] secrets`.
    secrets: SecretsMode,
}

impl Remember {
    pub fn new(store: Arc<SemanticStore>, scope_state: Arc<ScopeState>) -> Self {
        Self {
            store,
            scope_state,
            max_chars: DEFAULT_MAX_CHARS,
            secrets: SecretsMode::default(),
        }
    }

    /// Override the over-limit ceiling. The 500/2,000-character
    /// advisory and warning bounds are fixed (per §5.3).
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
    }

    /// Set `[guardrails] secrets`. Defaults to `reject`, so fixtures
    /// that never call this still get the guard.
    pub fn with_secret_guard(mut self, mode: SecretsMode) -> Self {
        self.secrets = mode;
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

        // After the size gate, so oversized content is rejected for its
        // size without being regex-scanned first.
        let secret_advisory = match secret_guard::guard_text(self.secrets, content)? {
            Guard::Reject(r) => return Ok(r),
            g => g.into_advisory(),
        };

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

        // With `supersedes` the new memory is meant to sit next to a close
        // neighbour until the link is written, so dedup is skipped there.
        let (id, duplicate) = if supersedes.is_empty() {
            // Only needed to tell the caller their `type`/`tags` were not
            // applied; cloned up front because the store takes ownership.
            let passed_tags = tags.clone();
            let passed_kind = args.get("type").is_some().then_some(kind);
            match self
                .store
                .remember_or_dedupe(content, kind, tags, scope)
                .await
                .map_err(|e| ToolError::Internal(format!("remember failed: {e}")))?
            {
                RememberOutcome::Stored { id, near } => (id, near),
                RememberOutcome::Existing(id) => {
                    tracing::info!(
                        memory_id = %id,
                        "remember: identical content already stored, nothing written"
                    );
                    let kept = self
                        .differs_from_stored(id, passed_kind, &passed_tags)
                        .await;
                    let text = if kept {
                        format!("already stored as {id}; existing type/tags kept")
                    } else {
                        format!("already stored as {id}")
                    };
                    let result = ToolResult::text(text)
                        .with_meta(json!({ "deduplicated": true, "id": id.to_string() }));
                    return Ok(secret_guard::attach(result, secret_advisory, id));
                }
            }
        } else {
            self.store
                .remember_checked(content, kind, tags, scope, true)
                .await
                .map_err(|e| ToolError::Internal(format!("remember failed: {e}")))?
        };

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
        Ok(secret_guard::attach(result, secret_advisory, id))
    }
}

impl Remember {
    /// Whether the caller passed a `type` or non-empty `tags` that differ
    /// from the memory a dedup hit resolved to. A lookup failure is not
    /// worth failing a successful dedup for, so it reads as "no".
    async fn differs_from_stored(
        &self,
        id: MemoryId,
        passed_kind: Option<MemoryKind>,
        passed_tags: &[String],
    ) -> bool {
        if passed_kind.is_none() && passed_tags.is_empty() {
            return false;
        }
        let Some(stored) = self.store.get(id).await.ok().flatten() else {
            return false;
        };
        let kind_differs = passed_kind.is_some_and(|k| k != stored.kind);
        let tags_differ = !passed_tags.is_empty() && {
            let mut a = passed_tags.to_vec();
            let mut b = stored.tags;
            a.sort();
            a.dedup();
            b.sort();
            b.dedup();
            a != b
        };
        kind_differs || tags_differ
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

    /// Storing a near-identical fact must still succeed, but the second
    /// call carries a `duplicate_advisory` so the agent can choose to
    /// `update` instead of growing the corpus forever. (Byte-identical
    /// content is short-circuited instead; see the `dedup` tests.)
    #[tokio::test]
    async fn second_near_identical_write_carries_a_duplicate_advisory() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());

        let first = r
            .invoke(json!({ "content": "CI runs on ubuntu-latest only" }))
            .await
            .unwrap();
        assert!(first.meta.is_none(), "first write should carry no advisory");

        let second = r
            .invoke(json!({ "content": "CI runs on ubuntu-latest only." }))
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
        let second = r
            .invoke(json!({ "content": format!("{long}.") }))
            .await
            .unwrap();
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
            .invoke(json!({ "content": "CI runs on ubuntu." }))
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

    fn id_from_dedup_text(res: &ToolResult) -> String {
        text_of(res)
            .strip_prefix("already stored as ")
            .expect("expected `already stored as <id>`")
            .to_owned()
    }

    #[tokio::test]
    async fn dedup_identical_write_returns_the_existing_id() {
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
        assert!(!res.is_error);
        assert_eq!(id_from_dedup_text(&res), first.to_string());
        let meta = res.meta.expect("expected dedup meta");
        assert_eq!(meta["deduplicated"], true);
        assert_eq!(meta["id"], first.to_string());
        assert_eq!(store.len(), 1, "a deduplicated write must not add a row");
    }

    #[tokio::test]
    async fn dedup_matches_whitespace_variants() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let first = id_of(
            &r.invoke(json!({ "content": "deploys  go out\non Fridays" }))
                .await
                .unwrap(),
        );
        let res = r
            .invoke(json!({ "content": "  deploys go out on Fridays \n" }))
            .await
            .unwrap();
        assert_eq!(id_from_dedup_text(&res), first.to_string());
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn dedup_is_per_scope() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let work = id_of(
            &r.invoke(json!({ "content": "same fact", "scope": "work" }))
                .await
                .unwrap(),
        );
        let res = r
            .invoke(json!({ "content": "same fact", "scope": "home" }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("stored memory "), "{res:?}");
        assert_ne!(id_of(&res), work);
        assert_eq!(store.len(), 2);
    }

    #[tokio::test]
    async fn dedup_reply_says_when_type_or_tags_were_not_applied() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let first = id_of(
            &r.invoke(json!({ "content": "ship on Fridays", "tags": ["ops"] }))
                .await
                .unwrap(),
        );
        let kept = format!("already stored as {first}; existing type/tags kept");

        let res = r
            .invoke(json!({ "content": "ship on Fridays", "type": "decision" }))
            .await
            .unwrap();
        assert_eq!(text_of(&res), kept, "different type");

        let res = r
            .invoke(json!({ "content": "ship on Fridays", "tags": ["ops", "release"] }))
            .await
            .unwrap();
        assert_eq!(text_of(&res), kept, "different tags");
        assert_eq!(res.meta.unwrap()["deduplicated"], true);

        // Same type and tags (any order), no type/tags, and the default
        // type spelled out all keep the plain reply.
        for args in [
            json!({ "content": "ship on Fridays", "type": "fact", "tags": ["ops"] }),
            json!({ "content": "ship on Fridays" }),
            json!({ "content": "ship on Fridays", "tags": [] }),
        ] {
            let res = r.invoke(args.clone()).await.unwrap();
            assert_eq!(
                text_of(&res),
                format!("already stored as {first}"),
                "{args}"
            );
        }
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn dedup_reply_keeps_the_secret_advisory() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r =
            Remember::new(Arc::clone(&store), make_scope()).with_secret_guard(SecretsMode::Warn);
        let content = format!("the deploy key is {}", aws_key());
        let first = id_of(&r.invoke(json!({ "content": content })).await.unwrap());
        let res = r.invoke(json!({ "content": content })).await.unwrap();
        let text = text_of(&res);
        assert!(
            text.starts_with(&format!("already stored as {first}")),
            "unexpected dedup text: {text}"
        );
        // The warn-mode advisory also reaches the text, as on the stored
        // path, because some hosts never show `_meta` to the model.
        assert!(
            text.contains("credential pattern aws_access_key_id")
                && text.contains(&first.to_string()),
            "advisory missing from the dedup text: {text}"
        );
        let meta = res.meta.expect("expected dedup meta");
        assert_eq!(meta["deduplicated"], true);
        assert_eq!(
            meta["secret_advisory"]["pattern"], "aws_access_key_id",
            "advisory lost on the dedup path: {meta}"
        );
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn dedup_ignores_a_superseded_twin() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let old = id_of(
            &r.invoke(json!({ "content": "balance is 5200" }))
                .await
                .unwrap(),
        );
        r.invoke(json!({ "content": "balance is 4800", "supersedes": old.to_string() }))
            .await
            .unwrap();
        let res = r
            .invoke(json!({ "content": "balance is 5200" }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("stored memory "), "{res:?}");
        assert_ne!(id_of(&res), old);
        assert_eq!(store.len(), 3);
    }

    #[tokio::test]
    async fn dedup_is_skipped_when_superseding() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let r = Remember::new(Arc::clone(&store), make_scope());
        let old = id_of(
            &r.invoke(json!({ "content": "balance is 5200" }))
                .await
                .unwrap(),
        );
        let res = r
            .invoke(json!({ "content": "balance is 5200", "supersedes": old.to_string() }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("stored memory "), "{res:?}");
        assert_eq!(store.superseded_by(old).await.unwrap(), Some(id_of(&res)));
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

    /// A credential-shaped string. Assembled at runtime so this file
    /// does not itself trip repository secret scanners.
    fn aws_key() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }

    #[tokio::test]
    async fn secret_in_reject_mode_stores_nothing() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope()).with_secret_guard(SecretsMode::Reject);
        let key = aws_key();
        let res = r
            .invoke(json!({ "content": format!("the deploy key is {key}") }))
            .await
            .unwrap();
        assert!(res.is_error);
        let meta = res.meta.as_ref().expect("error meta");
        assert_eq!(meta["error"]["code"], "secret_detected");
        assert_eq!(meta["error"]["pattern"], "aws_access_key_id");
        assert!(!format!("{:?}{}", res.content, meta).contains(&key));
        assert_eq!(s.len(), 0, "a rejected write must not store anything");
    }

    #[tokio::test]
    async fn secret_in_warn_mode_is_stored_with_advisory() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope()).with_secret_guard(SecretsMode::Warn);
        let res = r
            .invoke(json!({ "content": format!("the deploy key is {}", aws_key()) }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected a secret advisory");
        assert_eq!(meta["secret_advisory"]["pattern"], "aws_access_key_id");
        assert_eq!(meta["secret_advisory"]["high_confidence"], true);
        assert_eq!(s.len(), 1);
    }

    /// Some hosts never show `_meta`, so the advisory is in the text too,
    /// naming the stored id but never echoing the match.
    #[tokio::test]
    async fn warn_mode_advisory_is_also_in_the_reply_text() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope()).with_secret_guard(SecretsMode::Warn);
        let key = aws_key();
        let res = r
            .invoke(json!({ "content": format!("the deploy key is {key}") }))
            .await
            .unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let id = text
            .strip_prefix("stored memory ")
            .and_then(|t| t.split_whitespace().next())
            .expect("stored id")
            .to_owned();
        assert!(
            text.contains(&format!(
                "warning: content matched credential pattern aws_access_key_id at char 18; \
                 forget {id} if this is a real secret"
            )),
            "{text}"
        );
        assert!(!text.contains(&key));
    }

    /// Oversized content is refused for its size before the secret
    /// guard ever scans it.
    #[tokio::test]
    async fn oversized_content_is_rejected_for_size_not_scanned_for_secrets() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope())
            .with_max_chars(100)
            .with_secret_guard(SecretsMode::Reject);
        let res = r
            .invoke(json!({ "content": format!("{} {}", "x".repeat(200), aws_key()) }))
            .await
            .unwrap();
        assert!(res.is_error);
        assert_eq!(res.meta.unwrap()["error"]["code"], "memory_too_large");
        assert_eq!(s.len(), 0);
    }

    /// A long note that also holds a credential trips both advisories;
    /// they must coexist in one `_meta` object.
    #[tokio::test]
    async fn secret_advisory_coexists_with_length_advisory() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope()).with_secret_guard(SecretsMode::Warn);
        // 600+ chars lands in the advisory tier (500-2,000).
        let content = format!("{} {}", "x".repeat(600), aws_key());
        let res = r.invoke(json!({ "content": content })).await.unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected both advisories");
        assert!(
            meta.get("length_advisory").is_some(),
            "size advisory lost: {meta}"
        );
        assert!(
            meta.get("secret_advisory").is_some(),
            "secret advisory lost: {meta}"
        );
        assert_eq!(s.len(), 1);
    }

    #[tokio::test]
    async fn secret_in_off_mode_is_stored_without_advisory() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let r = Remember::new(Arc::clone(&s), make_scope()).with_secret_guard(SecretsMode::Off);
        let res = r
            .invoke(json!({ "content": format!("the deploy key is {}", aws_key()) }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let has_adv = res
            .meta
            .as_ref()
            .map(|m| m.get("secret_advisory").is_some())
            .unwrap_or(false);
        assert!(!has_adv, "off mode must not scan: {:?}", res.meta);
        assert_eq!(s.len(), 1);
    }
}
