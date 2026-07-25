//! Tool registry. A `Tool` is a verb the agent can call.
//!
//! Thirteen tools ship, spanning every memory layer: `remember` /
//! `recall` / `update` / `forget` (L4 semantic), `pin` / `unpin` (L0
//! procedural), `record_event` / `recall_recent` / `summarize_session`
//! (L3 episodic), `switch_scope` (session state), and `stats` /
//! `list_scopes` / `export` (diagnostics). `book/src/mcp-surface.md` is
//! the authoritative inventory.
//!
//! Tool descriptions are deliberately written from the agent's point of
//! view (spec §6.1: "the LLM reads the description to decide when to
//! invoke"), and each descriptor carries [`ToolAnnotations`] so a host
//! can tell a read from a delete without parsing prose.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::consolidation_scheduler::ConsolidationScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
use crate::memory::working::ActiveSession;
use crate::scope::ScopeState;
use crate::storage::Storage;
use crate::storage::archive::ColdArchive;

pub mod export;
pub mod forget;
pub mod list_scopes;
pub mod pin;
pub mod recall;
pub mod recall_recent;
pub mod record_event;
pub mod remember;
pub mod size_tier;
pub mod stats;
pub mod summarize_session;
pub mod switch_scope;
pub mod unpin;
pub mod update;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Behaviour hints emitted as MCP's `tools/list` → `annotations`.
///
/// Hosts use these to decide what needs a confirmation prompt. Without
/// them a host cannot tell `forget` (deletes a memory) from `stats`
/// (reads counters), so it must either prompt for everything or prompt
/// for nothing.
///
/// The spec's defaults are unhelpfully permissive — `destructiveHint`
/// defaults to `true` and `openWorldHint` to `true` — so every field is
/// emitted explicitly rather than relying on omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAnnotations {
    /// The tool does not modify any stored state.
    pub read_only: bool,
    /// The tool may destroy or overwrite existing data. Meaningful only
    /// when `read_only` is false.
    pub destructive: bool,
    /// Calling the tool twice with the same arguments leaves the same
    /// end state as calling it once. Meaningful only when `read_only`
    /// is false.
    pub idempotent: bool,
}

impl ToolAnnotations {
    /// Reads only. Trivially idempotent, never destructive.
    pub const fn read_only() -> Self {
        Self {
            read_only: true,
            destructive: false,
            idempotent: true,
        }
    }

    /// Creates new data on every call (`remember`, `pin`,
    /// `record_event`). Never overwrites, so not destructive — but not
    /// idempotent either, because a second call mints a second row
    /// under a fresh ULID.
    pub const fn additive() -> Self {
        Self {
            read_only: false,
            destructive: false,
            idempotent: false,
        }
    }

    /// Removes or overwrites existing data (`forget`, `unpin`,
    /// `update`). Idempotent: re-applying lands in the same state.
    pub const fn destructive() -> Self {
        Self {
            read_only: false,
            destructive: true,
            idempotent: true,
        }
    }

    /// Changes session state without touching stored memories
    /// (`switch_scope`). Converges on repeat.
    pub const fn session_state() -> Self {
        Self {
            read_only: false,
            destructive: false,
            idempotent: true,
        }
    }

    fn to_json(self, title: &str) -> Value {
        json!({
            "title": title,
            "readOnlyHint": self.read_only,
            "destructiveHint": self.destructive,
            "idempotentHint": self.idempotent,
            // Mneme is a local-first store and no tool path makes a
            // network call (cardinal rule: the server never talks to a
            // model), so the world it touches is closed by definition.
            "openWorldHint": false,
        })
    }
}

/// A tool's machine-readable signature. Mirrors the MCP `tools/list`
/// entry: name, human-readable title, description, a JSON Schema for
/// `arguments`, and behaviour annotations.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    /// Human-readable display name, for UIs that would otherwise show
    /// the raw snake_case `name`.
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub annotations: ToolAnnotations,
}

/// A tool invocation produces one or more `ContentBlock`s. MCP
/// supports text, image, and resource-link blocks; v0.1 only emits
/// text. `meta` carries structured tool-defined annotations (size
/// advisories, etc.) emitted as the MCP `_meta` field on the
/// tools/call result.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
}

impl ContentBlock {
    pub fn to_json(&self) -> Value {
        match self {
            ContentBlock::Text(t) => json!({ "type": "text", "text": t }),
        }
    }
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(s.into())],
            is_error: false,
            meta: None,
        }
    }

    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn with_error(mut self) -> Self {
        self.is_error = true;
        self
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "content": self.content.iter().map(ContentBlock::to_json).collect::<Vec<_>>(),
            "isError": self.is_error,
        });
        if let Some(m) = &self.meta {
            v["_meta"] = m.clone();
        }
        v
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError>;
}

/// Insertion-ordered registry. We use BTreeMap<&'static str, ...>
/// keyed by tool name so `tools/list` is deterministic across runs —
/// helpful for diffing test output.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the v0.1 default registry. Backed by all three memory
    /// stores so every tool the agent can call against L0/L3/L4 is
    /// wired up at once. The Phase 6 diagnostic surface (`stats`,
    /// `list_scopes`, `export`) takes the same handles plus the
    /// underlying [`Storage`] (for `b"mem:"` prefix scans) and the
    /// cold-tier [`ColdArchive`].
    ///
    /// Uses `size_tier::DEFAULT_MAX_CHARS` for `remember` / `update`
    /// content size enforcement. Production callers thread the
    /// configured ceiling via [`defaults_with_schedulers`].
    #[allow(clippy::too_many_arguments)]
    pub fn defaults(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
        storage: Arc<dyn Storage>,
        cold: ColdArchive,
        schema_version: u32,
    ) -> Self {
        Self::defaults_with_schedulers(
            semantic,
            procedural,
            episodic,
            storage,
            cold,
            schema_version,
            None,
            None,
            ScopeState::new("personal"),
            None,
            size_tier::DEFAULT_MAX_CHARS,
        )
    }

    /// Like [`defaults`](Self::defaults) but also attaches the L3
    /// consolidation scheduler and the L1 checkpoint scheduler so
    /// the `stats` tool reports their observability counters.
    /// `scope_state` is the per-process default-scope cell that
    /// `switch_scope` mutates and `remember` / `pin` consult on
    /// argument fall-back. `active_session` is the L1 working session
    /// that `record_event` mirrors message-kind events into (per
    /// ADR-0008); pass `None` in test fixtures that don't need the
    /// L1 surface.
    #[allow(clippy::too_many_arguments)]
    pub fn defaults_with_schedulers(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
        storage: Arc<dyn Storage>,
        cold: ColdArchive,
        schema_version: u32,
        consolidation: Option<Arc<ConsolidationScheduler>>,
        checkpoints: Option<Arc<CheckpointScheduler>>,
        scope_state: Arc<ScopeState>,
        active_session: Option<Arc<ActiveSession>>,
        max_remember_chars: usize,
    ) -> Self {
        let mut r = Self::new();
        // L4 — semantic memory.
        r.register(Arc::new(
            remember::Remember::new(Arc::clone(&semantic), Arc::clone(&scope_state))
                .with_max_chars(max_remember_chars),
        ));
        r.register(Arc::new(recall::Recall::new(Arc::clone(&semantic))));
        r.register(Arc::new(forget::Forget::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        )));
        r.register(Arc::new(
            update::Update::new(Arc::clone(&semantic)).with_max_chars(max_remember_chars),
        ));
        // L0 — procedural memory.
        r.register(Arc::new(pin::Pin::new(
            Arc::clone(&procedural),
            Arc::clone(&scope_state),
        )));
        r.register(Arc::new(unpin::Unpin::new(Arc::clone(&procedural))));
        // L3 — episodic memory.
        r.register(Arc::new(recall_recent::RecallRecent::new(Arc::clone(
            &episodic,
        ))));
        r.register(Arc::new(summarize_session::SummarizeSession::new(
            Arc::clone(&episodic),
        )));
        // record_event — agent-driven L3 producer (ADR-0008). For
        // message-kind events (user_message / assistant_message) the
        // tool also pushes a turn to the active session, so L1
        // captures conversation content rather than just tool names.
        let mut record_event_tool =
            record_event::RecordEvent::new(Arc::clone(&episodic), Arc::clone(&scope_state))
                .with_max_chars(max_remember_chars);
        if let Some(ref session) = active_session {
            record_event_tool = record_event_tool.with_active_session(Arc::clone(session));
        }
        r.register(Arc::new(record_event_tool));
        // Session state: switch_scope tool.
        r.register(Arc::new(switch_scope::SwitchScope::new(Arc::clone(
            &scope_state,
        ))));
        // Phase 6 diagnostics + portability.
        let mut stats_tool = stats::Stats::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            cold,
            schema_version,
        );
        if let Some(sched) = consolidation {
            stats_tool = stats_tool.with_consolidation(sched);
        }
        if let Some(sched) = checkpoints {
            stats_tool = stats_tool.with_checkpoints(sched);
        }
        stats_tool = stats_tool.with_scope_state(Arc::clone(&scope_state));
        r.register(Arc::new(stats_tool));
        r.register(Arc::new(list_scopes::ListScopes::new(
            semantic,
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            Arc::clone(&storage),
        )));
        r.register(Arc::new(export::Export::new(procedural, episodic, storage)));
        r
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.descriptor().name;
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn list(&self) -> Vec<ToolDescriptor> {
        self.tools.values().map(|t| t.descriptor()).collect()
    }
}

pub fn descriptor_to_json(d: &ToolDescriptor) -> Value {
    json!({
        "name": d.name,
        "title": d.title,
        "description": d.description,
        "inputSchema": d.input_schema,
        "annotations": d.annotations.to_json(d.title),
    })
}

/// Parse a `tags` argument shared by `remember`, `pin`, and `record_event`.
///
/// Accepts three shapes:
/// 1. Absent or `null` — empty list.
/// 2. JSON array of strings — taken as-is.
/// 3. JSON-encoded string whose contents parse to an array of strings —
///    tolerated as a workaround for MCP clients that double-encode
///    array-typed tool arguments before forwarding the `tools/call`
///    frame. Observed in some Claude Code releases against the
///    `tags` parameter; without this fallback every tagged write
///    from those clients fails with `-32602` even though the JSON
///    Schema is correct. The strict JSON-array path remains the
///    documented contract — the schema is unchanged — but a
///    stringified array is accepted with no behavioural difference.
///
/// Anything else (number, object, array of non-strings, malformed
/// JSON-encoded string) returns `InvalidArguments`. Callers report
/// the same `tags` error message regardless of source so existing
/// regression tests keep matching.
pub(crate) fn parse_tags_arg(arg: Option<&Value>) -> Result<Vec<String>, ToolError> {
    match arg {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_owned())
                    .ok_or_else(|| ToolError::InvalidArguments("`tags` must be strings".into()))
            })
            .collect(),
        Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s).map_err(|_| {
            ToolError::InvalidArguments(
                "`tags` must be an array of strings (got a string that did not parse as a JSON array of strings; some MCP clients double-encode array args)".into(),
            )
        }),
        Some(_) => Err(ToolError::InvalidArguments(
            "`tags` must be an array of strings".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    /// Bootstraps the three memory stores over `MemoryStorage` + the
    /// stub embedder so tests of the registry plumbing don't need a
    /// model download or a real redb file. The `TempDir` is returned
    /// to the caller because dropping it would yank the WAL +
    /// procedural files out from under the stores.
    fn fresh_registry() -> (ToolRegistry, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let cold = ColdArchive::new(tmp.path());
        (
            ToolRegistry::defaults(semantic, procedural, episodic, backing, cold, 1),
            tmp,
        )
    }

    #[test]
    fn defaults_register_phase_6_tools() {
        let (r, _tmp) = fresh_registry();
        let names: Vec<_> = r.list().iter().map(|d| d.name).collect();
        // BTreeMap ordering across L0/L3/L4 + Phase 6 diagnostics
        // + switch_scope (v0.15) + record_event (v0.2.4, ADR-0008).
        assert_eq!(
            names,
            vec![
                "export",
                "forget",
                "list_scopes",
                "pin",
                "recall",
                "recall_recent",
                "record_event",
                "remember",
                "stats",
                "summarize_session",
                "switch_scope",
                "unpin",
                "update",
            ]
        );
    }

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let (r, _tmp) = fresh_registry();
        assert!(r.get("nope").is_none());
    }

    /// Every tool must declare its behaviour, and the classification
    /// has to be right — a host that trusts `readOnlyHint` on a tool
    /// that deletes memories would skip the confirmation prompt.
    #[test]
    fn every_tool_declares_correct_annotations() {
        let (r, _tmp) = fresh_registry();
        let expected: &[(&str, ToolAnnotations)] = &[
            ("export", ToolAnnotations::read_only()),
            ("forget", ToolAnnotations::destructive()),
            ("list_scopes", ToolAnnotations::read_only()),
            ("pin", ToolAnnotations::additive()),
            ("recall", ToolAnnotations::read_only()),
            ("recall_recent", ToolAnnotations::read_only()),
            ("record_event", ToolAnnotations::additive()),
            ("remember", ToolAnnotations::additive()),
            ("stats", ToolAnnotations::read_only()),
            ("summarize_session", ToolAnnotations::read_only()),
            ("switch_scope", ToolAnnotations::session_state()),
            ("unpin", ToolAnnotations::destructive()),
            ("update", ToolAnnotations::destructive()),
        ];
        for d in r.list() {
            let want = expected
                .iter()
                .find(|(n, _)| *n == d.name)
                .map(|(_, a)| *a)
                .unwrap_or_else(|| panic!("tool `{}` has no expected annotation", d.name));
            assert_eq!(d.annotations, want, "wrong annotations for `{}`", d.name);
            assert!(!d.title.is_empty(), "tool `{}` has no title", d.name);
        }
        assert_eq!(r.list().len(), expected.len());
    }

    /// The wire form must carry all four hints explicitly — the spec's
    /// defaults for the omitted ones are the wrong way round for us.
    #[test]
    fn annotations_serialise_all_four_hints() {
        let (r, _tmp) = fresh_registry();
        let forget = r.get("forget").unwrap();
        let json = descriptor_to_json(&forget.descriptor());
        let ann = &json["annotations"];
        assert_eq!(ann["readOnlyHint"], false);
        assert_eq!(ann["destructiveHint"], true);
        assert_eq!(ann["idempotentHint"], true);
        assert_eq!(ann["openWorldHint"], false);
        assert_eq!(json["title"], "Forget a memory");

        let stats = r.get("stats").unwrap();
        let json = descriptor_to_json(&stats.descriptor());
        assert_eq!(json["annotations"]["readOnlyHint"], true);
        assert_eq!(json["annotations"]["destructiveHint"], false);
    }

    #[test]
    fn parse_tags_accepts_missing_or_null() {
        assert_eq!(parse_tags_arg(None).unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_tags_arg(Some(&Value::Null)).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn parse_tags_accepts_array_of_strings() {
        let v = json!(["a", "b"]);
        assert_eq!(parse_tags_arg(Some(&v)).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn parse_tags_rejects_array_of_numbers() {
        let v = json!([1, 2]);
        let err = parse_tags_arg(Some(&v)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    /// Workaround test: some MCP clients double-encode array tool
    /// arguments before forwarding the `tools/call` frame. We accept
    /// the stringified-array form when it parses back to a `Vec<String>`,
    /// so a buggy client doesn't lock the user out of `tags` writes.
    #[test]
    fn parse_tags_accepts_stringified_array_workaround() {
        let v = json!("[\"a\",\"b\"]");
        assert_eq!(parse_tags_arg(Some(&v)).unwrap(), vec!["a", "b"]);
        // Empty stringified array also accepted.
        let empty = json!("[]");
        assert_eq!(parse_tags_arg(Some(&empty)).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn parse_tags_rejects_unparseable_string() {
        let v = json!("not an array");
        let err = parse_tags_arg(Some(&v)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn parse_tags_rejects_stringified_non_string_array() {
        let v = json!("[1, 2]");
        let err = parse_tags_arg(Some(&v)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn parse_tags_rejects_non_array_non_string() {
        let v = json!(42);
        let err = parse_tags_arg(Some(&v)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
