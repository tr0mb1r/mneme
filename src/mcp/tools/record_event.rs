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

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
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
JSON; mneme stores it verbatim. `scope` defaults to the active \
scope; `retrieval_weight` defaults to 1.0 (range [0.0, 1.0]).";

/// Kinds that trigger L1 mirroring (server pushes a matching turn).
const MESSAGE_KINDS: &[&str] = &["user_message", "assistant_message"];

pub struct RecordEvent {
    episodic: Arc<EpisodicStore>,
    scope_state: Arc<ScopeState>,
    active_session: Option<Arc<ActiveSession>>,
}

impl RecordEvent {
    pub fn new(episodic: Arc<EpisodicStore>, scope_state: Arc<ScopeState>) -> Self {
        Self {
            episodic,
            scope_state,
            active_session: None,
        }
    }

    /// Builder hook — production wires the active session here so
    /// message-kind events also push a turn to L1. Tests that don't
    /// care about L1 mirror can skip this.
    pub fn with_active_session(mut self, session: Arc<ActiveSession>) -> Self {
        self.active_session = Some(session);
        self
    }
}

#[async_trait]
impl Tool for RecordEvent {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "record_event",
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
                        "description": "Event content. JSON object preferred (kind-specific shape); strings and null also accepted. Stored verbatim.",
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
                "required": ["kind"],
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
        // Accept any JSON value (object, string, null). Stored
        // verbatim as a string per the EpisodicEvent schema.
        let payload_value = args.get("payload").cloned().unwrap_or(Value::Null);
        let payload_str = serde_json::to_string(&payload_value)
            .map_err(|e| ToolError::Internal(format!("encode payload: {e}")))?;

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

        Ok(ToolResult::text(format!("recorded event {id}")))
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
            .invoke(json!({"kind": "   ", "payload": {}}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn out_of_range_weight_is_rejected() {
        let t = fixture_no_session();
        let err = t
            .invoke(json!({"kind": "decision", "retrieval_weight": 1.5}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = t
            .invoke(json!({"kind": "decision", "retrieval_weight": -0.1}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn tags_must_be_string_array() {
        let t = fixture_no_session();
        let err = t
            .invoke(json!({"kind": "observation", "tags": [1, 2]}))
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

        // Empty payload — L3 still records, L1 mirror skipped (no
        // content to push, would be noise).
        tool.invoke(json!({
            "kind": "user_message",
            "payload": {},
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
        assert_eq!(
            d.input_schema["properties"]["retrieval_weight"]["minimum"],
            0.0
        );
        assert_eq!(
            d.input_schema["properties"]["retrieval_weight"]["maximum"],
            1.0
        );
    }
}
