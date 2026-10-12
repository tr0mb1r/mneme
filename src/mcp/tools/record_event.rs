//! `record_event` — agent-driven L3 producer.
//!
//! The unified MCP tool ratified by ADR-0008. Lets the agent record a
//! structured event into mneme's L3 episodic hot tier with a free-form
//! `kind` string and a JSON payload. For canonical message kinds
//! (`user_message`, `assistant_message`), the server also mirrors a
//! matching turn into the L1 working session so `mneme://context`
//! L1 fold-in surfaces real conversation content rather than just
//! tool names.
//!
//! # Kind taxonomy
//!
//! `kind` is a free-form `String` (matches `EpisodicEvent::kind` —
//! ADR-0011 freezes the schema for v1.0). Canonical kinds are
//! documented in the tool description and the architecture doc §3.3.1;
//! agents can invent new kinds without a schema migration.
//!
//! # Privacy
//!
//! `payload` is opaque to mneme — agents are responsible for not
//! shipping credentials or secrets. The L3 hot tier ages into the
//! cold zstd archive on disk (180 days by default), so the same
//! discipline applies as `remember` for L4: only persist what's
//! actually meant to live.
//!
//! # No embedder
//!
//! Per ADR-0007, L3 events are not embedded. `record_event` writes
//! straight through `EpisodicStore::record_full` — no HNSW touch,
//! no embedder call, no synchronous vector compute.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::secret_guard::{self, Guard};
use super::size_tier::{self, DEFAULT_MAX_CHARS, Tier};
use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::config::SecretsMode;
use crate::memory::episodic::{DEFAULT_RETRIEVAL_WEIGHT, EpisodicStore};
use crate::memory::working::ActiveSession;
use crate::scope::ScopeState;

const DESCRIPTION: &str = "\
Record a structured event into L3 episodic memory. The agent calls \
this when something meaningful happens that should be retrievable \
later by recency, kind, or scope.

Canonical kinds:
- `user_message` / `assistant_message` — one event per conversation \
turn. The server also pushes a matching turn to the L1 working \
session, so `mneme://context` surfaces real conversation content. \
`payload` should include `{\"content\": \"<text>\"}`.
- `decision` — a choice was made. Include the choice and reasoning.
- `problem` / `resolution` — an issue was identified / solved. The \
resolution should reference the problem event by id in payload.
- `milestone` — something significant was completed.
- `preference` — the user expressed a way they want things done.
- `pivot` — a direction was changed.
- `observation` — something noticed worth recording.
- `summary` — after `summarize_session` returns its prompt, the \
agent fills it via the host LLM and records the digest here. \
`payload` typically includes `{\"text\": \"<summary>\", \"covers\": \
[\"<event_id>\", ...]}`.

Use `remember` (NOT this tool) for currently-true facts that need \
similarity retrieval. Use `pin` (NOT this tool) for always-on rules. \
Tool-call activity is auto-emitted by the server — do not duplicate.

`kind` is free-form — agents can invent new kinds. `payload` is \
required and must carry content, e.g. `{\"content\": \"<what \
happened>\"}`; put all text under `payload` (there is no top-level \
`text` or `content` argument). An absent, null, empty, blank or \
bare boolean/number payload is rejected. mneme stores the payload verbatim. `scope` \
defaults to the active scope; `retrieval_weight` defaults to 1.0 \
(range [0.0, 1.0]).";

/// Text-bearing payload fields: an object holding only these, all
/// blank, carries nothing worth storing.
const TEXT_FIELDS: &[&str] = &["content", "text"];

/// True when `payload` holds no content: absent/`null`, a bare boolean
/// or number, a blank string, an empty array or object, or an object
/// whose only keys are [`TEXT_FIELDS`] and all of them are `null` or
/// blank.
fn payload_is_empty(payload: &Value) -> bool {
    match payload {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => {
            o.is_empty()
                || o.iter().all(|(k, v)| {
                    TEXT_FIELDS.contains(&k.as_str())
                        && match v {
                            Value::Null => true,
                            Value::String(s) => s.trim().is_empty(),
                            _ => false,
                        }
                })
        }
    }
}

/// Kinds that trigger L1 mirroring (server pushes a matching turn).
const MESSAGE_KINDS: &[&str] = &["user_message", "assistant_message"];

pub struct RecordEvent {
    episodic: Arc<EpisodicStore>,
    scope_state: Arc<ScopeState>,
    active_session: Option<Arc<ActiveSession>>,
    /// Hard ceiling on the serialised payload's character length;
    /// writes above this are rejected with `memory_too_large`
    /// (release-planning v2.1 §5.4, applied to L3 per SEC-002 to
    /// prevent unbounded episodic exhaustion). Configured via
    /// `[budgets] max_remember_chars`.
    max_chars: usize,
    /// `[guardrails] secrets`.
    secrets: SecretsMode,
}

impl RecordEvent {
    pub fn new(episodic: Arc<EpisodicStore>, scope_state: Arc<ScopeState>) -> Self {
        Self {
            episodic,
            scope_state,
            active_session: None,
            max_chars: DEFAULT_MAX_CHARS,
            secrets: SecretsMode::default(),
        }
    }

    /// Builder hook — production wires the active session here so
    /// message-kind events also push a turn to L1. Tests that don't
    /// care about L1 mirror can skip this.
    pub fn with_active_session(mut self, session: Arc<ActiveSession>) -> Self {
        self.active_session = Some(session);
        self
    }

    /// Override the over-limit ceiling. Mirrors
    /// `Remember::with_max_chars` / `Update::with_max_chars`. The
    /// 500 / 2,000-character advisory and warning bounds are fixed
    /// (per §5.3).
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
impl Tool for RecordEvent {
    /// An agent that passes `text=` / `content=` at the top level used
    /// to get `payload: null` stored silently; the registry's unknown-
    /// argument check now rejects it with this pointer.
    fn unknown_args_hint(&self) -> Option<&'static str> {
        Some("Put event text under payload: {\"content\": \"<what happened>\"}")
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "record_event",
            title: "Record an event",
            annotations: ToolAnnotations::additive(),
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Event kind. Canonical kinds: user_message, assistant_message, decision, problem, resolution, milestone, preference, pivot, observation, summary. Free-form — agents can invent new kinds.",
                        "minLength": 1,
                    },
                    "payload": {
                        "description": "Event content, required and non-empty. JSON object preferred, e.g. {\"content\": \"<what happened>\"} (kind-specific shape); a non-blank string is also accepted. Absent, null, empty or blank payloads are rejected. Stored verbatim.",
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for filtering.",
                    },
                    "scope": {
                        "type": "string",
                        "description": "Optional scope. Defaults to the active scope (set by `switch_scope`).",
                    },
                    "retrieval_weight": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Per-event ranking weight. Defaults to 1.0; lower for chatty/low-signal events to push them behind their peers.",
                    },
                },
                "required": ["kind", "payload"],
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        // ---------- kind ----------
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`kind` is required".into()))?;
        if kind.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`kind` must not be empty".into(),
            ));
        }

        // ---------- payload ----------
        // Any JSON value that carries content is accepted and stored
        // verbatim as a string per the EpisodicEvent schema. Empty
        // shapes are rejected below, right before the size gate.
        let payload_value = args.get("payload").cloned().unwrap_or(Value::Null);
        let payload_str = serde_json::to_string(&payload_value)
            .map_err(|e| ToolError::Internal(format!("encode payload: {e}")))?;

        // ---------- empty-payload gate ----------
        if payload_is_empty(&payload_value) {
            return Err(ToolError::InvalidArguments(
                "`payload` is required and must carry content; pass \
                 {\"content\": \"<what happened>\"}"
                    .into(),
            ));
        }

        // ---------- SEC-002 size gate ----------
        // Same classifier `remember` / `update` use, applied to the
        // serialised payload string. Prevents unbounded L3 episodic
        // exhaustion by an agent writing oversized payloads through
        // `record_event` (the audit pin SEC-002 captured the gap).
        let payload_len = size_tier::count_chars(&payload_str);
        let tier = size_tier::classify(payload_len, self.max_chars);
        if tier == Tier::OverLimit {
            let (text, meta) = size_tier::rejection(payload_len, self.max_chars);
            tracing::warn!(
                tool = "record_event",
                kind = kind,
                payload_chars = payload_len,
                max_chars = self.max_chars,
                "rejected: payload over size limit"
            );
            return Ok(ToolResult::text(text).with_error().with_meta(meta));
        }

        // ---------- secret guard ----------
        // After the empty-payload and size gates, so a payload that is
        // about to be rejected anyway is never regex-scanned.
        let secret_advisory = match secret_guard::guard_value(self.secrets, &payload_value)? {
            Guard::Reject(r) => return Ok(r),
            g => g.into_advisory(),
        };

        // ---------- scope (fallback to ScopeState::current) ----------
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| self.scope_state.current());

        // ---------- tags ----------
        let tags = super::parse_tags_arg(args.get("tags"))?;

        // ---------- retrieval_weight ----------
        let retrieval_weight = match args.get("retrieval_weight") {
            None | Some(Value::Null) => DEFAULT_RETRIEVAL_WEIGHT,
            Some(Value::Number(n)) => n
                .as_f64()
                .ok_or_else(|| {
                    ToolError::InvalidArguments("`retrieval_weight` must be a finite number".into())
                })?
                .clamp(f32::MIN as f64, f32::MAX as f64)
                as f32,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`retrieval_weight` must be a number in [0.0, 1.0]".into(),
                ));
            }
        };
        if !(0.0..=1.0).contains(&retrieval_weight) || retrieval_weight.is_nan() {
            return Err(ToolError::InvalidArguments(format!(
                "`retrieval_weight` must be in [0.0, 1.0]; got {retrieval_weight}"
            )));
        }

        // ---------- write to L3 ----------
        let id = self
            .episodic
            .record_full(kind, &scope, &payload_str, tags, retrieval_weight)
            .await
            .map_err(|e| ToolError::Internal(format!("record event: {e}")))?;

        // ---------- L1 mirror for message kinds ----------
        if MESSAGE_KINDS.contains(&kind)
            && let Some(session) = self.active_session.as_ref()
        {
            let role = if kind == "user_message" {
                "user"
            } else {
                "assistant"
            };
            let content = payload_value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Skip empty content — pushing an empty turn is noise.
            // The L3 event still records the kind and (empty) payload.
            if !content.is_empty() {
                session.push_turn(role, content);
            }
        }

        if tier == Tier::Warning {
            tracing::info!(
                tool = "record_event",
                kind = kind,
                payload_chars = payload_len,
                limit = self.max_chars,
                event_id = %id,
                "record_event: large payload stored (warning tier)"
            );
        }

        let mut result = ToolResult::text(format!("recorded event {id}"));
        if let Some(meta) = size_tier::success_meta(tier, payload_len, self.max_chars) {
            result = result.with_meta(meta);
        }
        Ok(secret_guard::attach(result, secret_advisory, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::episodic::RecentFilters;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use serde_json::json;

    fn fixture_no_session() -> RecordEvent {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(storage));
        let scope = ScopeState::new("personal");
        RecordEvent::new(episodic, scope)
    }

    fn fixture_with_session(
        sessions_dir: std::path::PathBuf,
    ) -> (RecordEvent, Arc<EpisodicStore>, Arc<ActiveSession>) {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(storage));
        let scope = ScopeState::new("work");
        let session = ActiveSession::open(sessions_dir).unwrap();
        let tool = RecordEvent::new(Arc::clone(&episodic), scope)
            .with_active_session(Arc::clone(&session));
        (tool, episodic, session)
    }

    /// Run `args` through the tool and assert it is rejected as
    /// `InvalidArguments` naming `payload`, with L3 untouched.
    async fn assert_payload_rejected(args: Value) {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(storage));
        let t = RecordEvent::new(Arc::clone(&episodic), ScopeState::new("personal"));
        let err = t.invoke(args.clone()).await.unwrap_err();
        match err {
            ToolError::InvalidArguments(m) => {
                assert!(m.contains("payload"), "message should name payload: {m}");
                assert!(
                    m.contains("content"),
                    "message should say what to pass: {m}"
                );
            }
            other => panic!("expected InvalidArguments for {args}, got {other:?}"),
        }
        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert!(events.is_empty(), "rejected call must not write: {args}");
    }

    #[tokio::test]
    async fn absent_payload_is_rejected() {
        assert_payload_rejected(json!({"kind": "milestone"})).await;
    }

    #[tokio::test]
    async fn null_payload_is_rejected() {
        assert_payload_rejected(json!({"kind": "milestone", "payload": null})).await;
    }

    #[tokio::test]
    async fn blank_string_payload_is_rejected() {
        assert_payload_rejected(json!({"kind": "milestone", "payload": ""})).await;
        assert_payload_rejected(json!({"kind": "milestone", "payload": " \n\t "})).await;
    }

    #[tokio::test]
    async fn empty_object_and_array_payloads_are_rejected() {
        assert_payload_rejected(json!({"kind": "milestone", "payload": {}})).await;
        assert_payload_rejected(json!({"kind": "milestone", "payload": []})).await;
    }

    #[tokio::test]
    async fn bare_bool_and_number_payloads_are_rejected() {
        for p in [json!(true), json!(false), json!(0), json!(42), json!(1.5)] {
            assert_payload_rejected(json!({"kind": "milestone", "payload": p})).await;
        }
    }

    #[tokio::test]
    async fn bool_and_number_inside_an_object_still_count_as_content() {
        let t = fixture_no_session();
        for p in [json!({"ok": true}), json!({"count": 3})] {
            let res = t
                .invoke(json!({"kind": "milestone", "payload": p}))
                .await
                .unwrap();
            assert!(!res.is_error);
        }
    }

    #[tokio::test]
    async fn blank_text_fields_only_payload_is_rejected() {
        for p in [
            json!({"content": ""}),
            json!({"text": "   "}),
            json!({"content": "", "text": " "}),
            json!({"content": null}),
        ] {
            assert_payload_rejected(json!({"kind": "milestone", "payload": p})).await;
        }
    }

    #[tokio::test]
    async fn blank_text_field_beside_other_content_is_accepted() {
        let t = fixture_no_session();
        let res = t
            .invoke(json!({
                "kind": "milestone",
                "payload": {"content": "", "covers": ["01ABC"]},
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
    }

    #[tokio::test]
    async fn non_blank_string_payload_is_accepted() {
        let t = fixture_no_session();
        let res = t
            .invoke(json!({"kind": "milestone", "payload": "shipped v1"}))
            .await
            .unwrap();
        assert!(!res.is_error);
    }

    #[tokio::test]
    async fn missing_kind_is_rejected() {
        let t = fixture_no_session();
        let err = t.invoke(json!({"payload": {"x": 1}})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn empty_kind_is_rejected() {
        let t = fixture_no_session();
        let err = t
            .invoke(json!({"kind": "   ", "payload": {"content": "x"}}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn out_of_range_weight_is_rejected() {
        let t = fixture_no_session();
        let err = t
            .invoke(
                json!({"kind": "decision", "payload": {"content": "x"}, "retrieval_weight": 1.5}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = t
            .invoke(
                json!({"kind": "decision", "payload": {"content": "x"}, "retrieval_weight": -0.1}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn tags_must_be_string_array() {
        let t = fixture_no_session();
        let err = t
            .invoke(json!({"kind": "observation", "payload": {"content": "x"}, "tags": [1, 2]}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn writes_event_to_l3_with_defaults() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let scope = ScopeState::new("personal");
        let t = RecordEvent::new(Arc::clone(&episodic), scope);

        t.invoke(json!({
            "kind": "decision",
            "payload": {"content": "use redb", "reasoning": "stable"},
        }))
        .await
        .unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind, "decision");
        assert_eq!(e.scope, "personal"); // fell back to ScopeState
        assert!((e.retrieval_weight - 1.0).abs() < 1e-6);
        assert!(e.tags.is_empty());
        let p = e.payload_json().unwrap();
        assert_eq!(p["content"], "use redb");
        assert_eq!(p["reasoning"], "stable");
    }

    #[tokio::test]
    async fn explicit_scope_and_tags_round_trip() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let scope = ScopeState::new("personal"); // session default
        let t = RecordEvent::new(Arc::clone(&episodic), scope);

        t.invoke(json!({
            "kind": "milestone",
            "payload": {"text": "v0.2.4 shipped"},
            "scope": "work",
            "tags": ["release", "v0.2.4"],
            "retrieval_weight": 0.9,
        }))
        .await
        .unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.scope, "work");
        assert_eq!(e.tags, vec!["release".to_string(), "v0.2.4".to_string()]);
        assert!((e.retrieval_weight - 0.9).abs() < 1e-4);
    }

    /// Some MCP clients double-encode array tool args before
    /// forwarding the `tools/call` frame, so `tags` arrives as a
    /// JSON-encoded string. The shared `parse_tags_arg` helper
    /// tolerates that shape; this test pins the `record_event` call
    /// site to delegate to the helper rather than hard-rejecting
    /// non-array values.
    #[tokio::test]
    async fn harness_double_encoded_tags_are_accepted() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let scope = ScopeState::new("personal");
        let t = RecordEvent::new(Arc::clone(&episodic), scope);

        t.invoke(json!({
            "kind": "observation",
            "payload": {"content": "double-encoded tags"},
            "tags": "[\"workaround\",\"harness\"]",
        }))
        .await
        .unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].tags,
            vec!["workaround".to_string(), "harness".to_string()]
        );
    }

    #[tokio::test]
    async fn user_message_mirrors_to_l1() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tool, episodic, session) = fixture_with_session(tmp.path().join("sessions"));

        tool.invoke(json!({
            "kind": "user_message",
            "payload": {"content": "let's switch to Postgres"},
        }))
        .await
        .unwrap();

        // L3 event landed
        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "user_message");
        assert_eq!(events[0].scope, "work"); // ScopeState default

        // L1 turn pushed
        assert_eq!(session.turns_total(), 1);
    }

    #[tokio::test]
    async fn assistant_message_mirrors_to_l1() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tool, _episodic, session) = fixture_with_session(tmp.path().join("sessions"));

        tool.invoke(json!({
            "kind": "assistant_message",
            "payload": {"content": "ok, draft the migration"},
        }))
        .await
        .unwrap();

        assert_eq!(session.turns_total(), 1);
    }

    #[tokio::test]
    async fn non_message_kind_does_not_push_l1_turn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tool, _episodic, session) = fixture_with_session(tmp.path().join("sessions"));

        tool.invoke(json!({
            "kind": "decision",
            "payload": {"content": "use redb"},
        }))
        .await
        .unwrap();

        // L1 NOT touched for non-message kinds — that's L3's job alone
        assert_eq!(session.turns_total(), 0);
    }

    #[tokio::test]
    async fn message_kind_without_content_is_l3_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tool, episodic, session) = fixture_with_session(tmp.path().join("sessions"));

        // Payload without `content` — L3 still records, L1 mirror
        // skipped (no content to push, would be noise).
        tool.invoke(json!({
            "kind": "user_message",
            "payload": {"text": "x"},
        }))
        .await
        .unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(session.turns_total(), 0);
    }

    /// SEC-002: payload under 500 chars after JSON-encoding lands in
    /// the Normal tier — no `_meta` annotation, no `is_error`.
    #[tokio::test]
    async fn normal_tier_attaches_no_meta() {
        let t = fixture_no_session();
        let res = t
            .invoke(json!({
                "kind": "observation",
                "payload": {"content": "small payload"},
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
        assert!(res.meta.is_none(), "Normal tier must not annotate meta");
    }

    /// SEC-002: payload in [500, 2_000) chars carries
    /// `length_advisory` meta but is stored.
    #[tokio::test]
    async fn advisory_tier_attaches_length_advisory_meta() {
        let t = fixture_no_session();
        let content = "a".repeat(700);
        let res = t
            .invoke(json!({
                "kind": "observation",
                "payload": {"content": content},
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected length_advisory meta");
        assert!(meta.get("length_advisory").is_some());
        assert!(meta.get("length_warning").is_none());
        assert_eq!(meta["length_advisory"]["limit"], 10_000);
    }

    /// SEC-002: payload in [2_000, 10_000] chars carries
    /// `length_warning` meta but is stored.
    #[tokio::test]
    async fn warning_tier_attaches_length_warning_meta() {
        let t = fixture_no_session();
        let content = "w".repeat(5_000);
        let res = t
            .invoke(json!({
                "kind": "observation",
                "payload": {"content": content},
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected length_warning meta");
        assert!(meta.get("length_warning").is_some());
        assert!(meta.get("length_advisory").is_none());
    }

    /// SEC-002: payload over `max_chars` is rejected with structured
    /// `memory_too_large` meta and `is_error`. Storage is NOT
    /// touched (the rejection happens before `record_full`).
    #[tokio::test]
    async fn over_limit_rejects_with_memory_too_large_and_skips_write() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let scope = ScopeState::new("personal");
        let t = RecordEvent::new(Arc::clone(&episodic), scope);
        let content = "x".repeat(15_000);
        let res = t
            .invoke(json!({
                "kind": "observation",
                "payload": {"content": content},
            }))
            .await
            .unwrap();
        assert!(res.is_error, "over-limit payload must mark is_error");
        let meta = res.meta.expect("expected error meta");
        assert_eq!(meta["error"]["code"], "memory_too_large");
        assert_eq!(meta["error"]["limit"], 10_000);
        // L3 must not have grown — the rejection precedes `record_full`.
        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert!(
            events.is_empty(),
            "rejected payload must not have been written to L3"
        );
    }

    /// SEC-002: custom ceiling propagates through `with_max_chars`.
    #[tokio::test]
    async fn with_max_chars_overrides_default_ceiling() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(storage));
        let scope = ScopeState::new("personal");
        let t = RecordEvent::new(episodic, scope).with_max_chars(100);
        let content = "z".repeat(200);
        let res = t
            .invoke(json!({
                "kind": "observation",
                "payload": {"content": content},
            }))
            .await
            .unwrap();
        assert!(res.is_error);
        let meta = res.meta.unwrap();
        assert_eq!(meta["error"]["limit"], 100);
    }

    #[tokio::test]
    async fn descriptor_lists_canonical_kinds() {
        let t = fixture_no_session();
        let d = t.descriptor();
        assert_eq!(d.name, "record_event");
        assert!(d.description.contains("user_message"));
        assert!(d.description.contains("decision"));
        assert!(d.description.contains("summary"));
        // Schema requires kind, has retrieval_weight bounds.
        assert_eq!(d.input_schema["required"][0], "kind");
        assert_eq!(d.input_schema["required"][1], "payload");
        assert!(
            !d.input_schema["properties"]["payload"]["description"]
                .as_str()
                .unwrap()
                .contains("null also accepted")
        );
        assert_eq!(
            d.input_schema["properties"]["retrieval_weight"]["minimum"],
            0.0
        );
        assert_eq!(
            d.input_schema["properties"]["retrieval_weight"]["maximum"],
            1.0
        );
    }

    /// A credential-shaped string. Assembled at runtime so this file
    /// does not itself trip repository secret scanners.
    fn aws_key() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }

    fn fixture_with_mode(mode: SecretsMode) -> (RecordEvent, Arc<EpisodicStore>) {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(storage));
        let tool = RecordEvent::new(Arc::clone(&episodic), ScopeState::new("personal"))
            .with_secret_guard(mode);
        (tool, episodic)
    }

    async fn stored_events(episodic: &EpisodicStore) -> usize {
        episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn secret_in_reject_mode_records_nothing() {
        let (t, episodic) = fixture_with_mode(SecretsMode::Reject);
        let key = aws_key();
        let res = t
            .invoke(json!({
                "kind": "milestone",
                "payload": { "content": "rotated", "detail": { "key": key.clone() } },
            }))
            .await
            .unwrap();
        assert!(res.is_error);
        let meta = res.meta.as_ref().expect("error meta");
        assert_eq!(meta["error"]["code"], "secret_detected");
        assert_eq!(meta["error"]["path"], "/detail/key");
        assert!(!format!("{:?}{}", res.content, meta).contains(&key));
        assert_eq!(stored_events(&episodic).await, 0);
    }

    #[tokio::test]
    async fn secret_in_warn_mode_is_recorded_with_advisory() {
        let (t, episodic) = fixture_with_mode(SecretsMode::Warn);
        let res = t
            .invoke(json!({
                "kind": "milestone",
                "payload": { "content": format!("key {}", aws_key()) },
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected a secret advisory");
        assert_eq!(meta["secret_advisory"]["pattern"], "aws_access_key_id");
        assert_eq!(meta["secret_advisory"]["path"], "/content");
        assert_eq!(stored_events(&episodic).await, 1);
        let crate::mcp::tools::ContentBlock::Text(text) = &res.content[0];
        assert!(text.starts_with("recorded event "), "{text}");
        assert!(
            text.contains(
                "warning: content matched credential pattern aws_access_key_id \
                 at char 4 of /content;"
            ),
            "{text}"
        );
        assert!(text.contains("if this is a real secret"), "{text}");
        assert!(!text.contains(&aws_key()));
    }

    /// An oversized payload is refused for its size before the secret
    /// guard scans it.
    #[tokio::test]
    async fn oversized_payload_is_rejected_for_size_not_scanned_for_secrets() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let episodic = Arc::new(EpisodicStore::new(storage));
        let t = RecordEvent::new(Arc::clone(&episodic), ScopeState::new("personal"))
            .with_max_chars(100)
            .with_secret_guard(SecretsMode::Reject);
        let res = t
            .invoke(json!({
                "kind": "milestone",
                "payload": { "content": format!("{} {}", "x".repeat(200), aws_key()) },
            }))
            .await
            .unwrap();
        assert!(res.is_error);
        assert_eq!(res.meta.unwrap()["error"]["code"], "memory_too_large");
        assert_eq!(stored_events(&episodic).await, 0);
    }

    #[tokio::test]
    async fn secret_in_off_mode_is_recorded_without_advisory() {
        let (t, episodic) = fixture_with_mode(SecretsMode::Off);
        let res = t
            .invoke(json!({
                "kind": "milestone",
                "payload": { "content": format!("key {}", aws_key()) },
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let has_adv = res
            .meta
            .as_ref()
            .map(|m| m.get("secret_advisory").is_some())
            .unwrap_or(false);
        assert!(!has_adv, "off mode must not scan: {:?}", res.meta);
        assert_eq!(stored_events(&episodic).await, 1);
    }
}
