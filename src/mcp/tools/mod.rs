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
pub mod secret_guard;
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

    /// Extra sentence appended to the "unexpected argument(s)" error
    /// when a caller passes a key the input schema does not declare.
    /// Use it to say where a commonly guessed parameter belongs.
    fn unknown_args_hint(&self) -> Option<&'static str> {
        None
    }
}

/// Insertion-ordered registry. We use BTreeMap<&'static str, ...>
/// keyed by tool name so `tools/list` is deterministic across runs —
/// helpful for diffing test output.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, RegisteredTool>,
}

/// A tool plus what the `tools/call` hot path needs, captured once at
/// registration so it never rebuilds the descriptor (and its JSON input
/// schema) per call: the `readOnlyHint` and the top-level argument names.
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    read_only: bool,
    /// Top-level keys of `inputSchema.properties`. `None` when the
    /// schema declares no `properties` or explicitly permits
    /// `additionalProperties`; unknown arguments are then not rejected.
    allowed_args: Option<Vec<String>>,
    hint: Option<&'static str>,
}

/// The argument names a schema declares, or `None` when it does not
/// close the set (no `properties`, or `additionalProperties` other than
/// `false`).
fn declared_args(schema: &Value) -> Option<Vec<String>> {
    if schema
        .get("additionalProperties")
        .is_some_and(|v| *v != Value::Bool(false))
    {
        return None;
    }
    let props = schema.get("properties")?.as_object()?;
    Some(props.keys().cloned().collect())
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
            crate::config::SecretsMode::default(),
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
    /// L1 surface. `secrets` is `[guardrails] secrets`: what the write
    /// tools do with credential-shaped content (see [`secret_guard`]).
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
        secrets: crate::config::SecretsMode,
    ) -> Self {
        let mut r = Self::new();
        // L4 — semantic memory.
        r.register(Arc::new(
            remember::Remember::new(Arc::clone(&semantic), Arc::clone(&scope_state))
                .with_max_chars(max_remember_chars)
                .with_secret_guard(secrets),
        ));
        r.register(Arc::new(recall::Recall::new(Arc::clone(&semantic))));
        r.register(Arc::new(forget::Forget::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        )));
        r.register(Arc::new(
            update::Update::new(Arc::clone(&semantic))
                .with_max_chars(max_remember_chars)
                .with_secret_guard(secrets),
        ));
        // L0 — procedural memory.
        r.register(Arc::new(
            pin::Pin::new(Arc::clone(&procedural), Arc::clone(&scope_state))
                .with_secret_guard(secrets),
        ));
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
                .with_max_chars(max_remember_chars)
                .with_secret_guard(secrets);
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
        let descriptor = tool.descriptor();
        self.tools.insert(
            descriptor.name,
            RegisteredTool {
                read_only: descriptor.annotations.read_only,
                allowed_args: declared_args(&descriptor.input_schema),
                hint: tool.unknown_args_hint(),
                tool,
            },
        );
    }

    /// Reject `args` keys the tool's input schema does not declare
    /// (`InvalidArguments` naming them). Uses the key set cached at
    /// registration, so a clean call allocates nothing. Unknown tools
    /// and tools whose schema does not close the set pass.
    pub fn check_args(&self, name: &str, args: &Value) -> Result<(), ToolError> {
        match self.tools.get(name) {
            Some(RegisteredTool {
                allowed_args: Some(allowed),
                hint,
                ..
            }) => reject_unknown_args(args, allowed, *hint),
            _ => Ok(()),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(|t| Arc::clone(&t.tool))
    }

    /// The tool's `readOnlyHint`, cached at registration. `None` for an
    /// unknown tool. Does not call [`Tool::descriptor`].
    pub fn is_read_only(&self, name: &str) -> Option<bool> {
        self.tools.get(name).map(|t| t.read_only)
    }

    pub fn list(&self) -> Vec<ToolDescriptor> {
        self.tools.values().map(|t| t.tool.descriptor()).collect()
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

/// Reject arguments the tool does not declare.
///
/// MCP hosts forward whatever keys the model produced; without this an
/// agent that guesses a parameter name (for example `text=` on
/// `record_event`) has the key silently dropped and the call "succeeds"
/// with nothing stored. Names every unexpected key, in sorted order, and
/// appends `hint` (if any) so the error says what to do instead.
/// Non-object `args` are ignored here; each tool reports its own
/// missing-argument errors.
fn reject_unknown_args<S: AsRef<str>>(
    args: &Value,
    allowed: &[S],
    hint: Option<&str>,
) -> Result<(), ToolError> {
    let Some(obj) = args.as_object() else {
        return Ok(());
    };
    let mut unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.iter().any(|a| a.as_ref() == *k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    let names = unknown
        .iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut msg = format!(
        "unexpected argument(s) {names}; accepted arguments: {}",
        allowed
            .iter()
            .map(|k| format!("`{}`", k.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(h) = hint {
        msg.push_str(". ");
        msg.push_str(h);
    }
    Err(ToolError::InvalidArguments(msg))
}

/// Parse `supersedes`: one ULID string or an array of them, deduplicated.
pub(crate) fn parse_supersedes(v: Option<&Value>) -> Result<Vec<crate::ids::MemoryId>, ToolError> {
    let raw: Vec<&str> = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("`supersedes` entries must be strings".into())
                })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(ToolError::InvalidArguments(
                "`supersedes` must be a memory id or an array of ids".into(),
            ));
        }
    };
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let id = ulid::Ulid::from_string(s.trim())
            .map(crate::ids::MemoryId)
            .map_err(|_| {
                ToolError::InvalidArguments(format!("`supersedes`: `{s}` is not a memory id"))
            })?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
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

    #[test]
    fn registry_caches_read_only_flag() {
        let (r, _tmp) = fresh_registry();
        assert_eq!(r.is_read_only("recall"), Some(true));
        assert_eq!(r.is_read_only("forget"), Some(false));
        assert_eq!(r.is_read_only("remember"), Some(false));
        assert_eq!(r.is_read_only("no_such_tool"), None);
    }

    /// Run `tools/call` through the real dispatcher; returns the
    /// JSON-RPC error message when the call was rejected.
    async fn call_error(r: &ToolRegistry, name: &str, arguments: Value) -> Option<String> {
        use crate::config::AutoLogToolCalls;
        use crate::mcp::dispatch::handle_tools_call;
        use crate::mcp::jsonrpc::{Id, error_codes};
        let resp = handle_tools_call(
            r,
            None,
            None,
            None,
            None,
            AutoLogToolCalls::Off,
            Id::Number(1),
            Some(json!({"name": name, "arguments": arguments})),
        )
        .await;
        resp.error.map(|e| {
            assert_eq!(e.code, error_codes::INVALID_PARAMS, "{name}");
            e.message
        })
    }

    /// Every shipped tool closes its argument set, so the generic check
    /// covers it; a tool without `properties` would silently skip it.
    #[test]
    fn every_default_tool_has_a_closed_argument_set() {
        let (r, _tmp) = fresh_registry();
        for d in r.list() {
            let cached = r.tools.get(d.name).and_then(|t| t.allowed_args.as_ref());
            let declared: Vec<&String> = d.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("`{}` declares no properties", d.name))
                .keys()
                .collect();
            let mut cached: Vec<&String> = cached
                .unwrap_or_else(|| panic!("`{}` has no cached argument set", d.name))
                .iter()
                .collect();
            cached.sort_unstable();
            assert_eq!(cached, declared, "{}", d.name);
        }
    }

    #[tokio::test]
    async fn unknown_arguments_are_rejected_on_write_tools() {
        let (r, _tmp) = fresh_registry();
        let cases = [
            ("pin", json!({"content": "rule", "bogus": 1})),
            ("update", json!({"id": "x", "content": "c", "bogus": 1})),
            ("remember", json!({"content": "fact", "bogus": 1})),
            (
                "record_event",
                json!({"kind": "k", "payload": {"content": "x"}, "bogus": 1}),
            ),
            ("stats", json!({"bogus": 1})),
        ];
        for (tool, args) in cases {
            let msg = call_error(&r, tool, args)
                .await
                .unwrap_or_else(|| panic!("{tool} accepted an unknown argument"));
            assert!(msg.contains("`bogus`"), "{tool}: {msg}");
            assert!(msg.contains("accepted arguments"), "{tool}: {msg}");
        }
    }

    #[tokio::test]
    async fn record_event_unknown_text_key_gets_the_payload_hint() {
        let (r, _tmp) = fresh_registry();
        for key in ["text", "content"] {
            let msg = call_error(&r, "record_event", json!({"kind": "milestone", key: "x"}))
                .await
                .expect("rejected");
            assert!(msg.contains(&format!("`{key}`")), "{msg}");
            assert!(msg.contains("payload: {\"content\""), "{msg}");
        }
        // Other tools get no record_event hint.
        let msg = call_error(&r, "pin", json!({"content": "r", "text": "x"}))
            .await
            .expect("rejected");
        assert!(!msg.contains("payload"), "{msg}");
    }

    #[tokio::test]
    async fn declared_arguments_pass_the_check() {
        let (r, _tmp) = fresh_registry();
        assert!(call_error(&r, "stats", json!({})).await.is_none());
        assert!(
            call_error(
                &r,
                "pin",
                json!({"content": "rule", "tags": ["a"], "scope": "s"})
            )
            .await
            .is_none()
        );
    }

    struct OpenSchemaTool;

    #[async_trait]
    impl Tool for OpenSchemaTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "open",
                title: "Open",
                description: "test tool",
                input_schema: json!({
                    "type": "object",
                    "properties": {"a": {"type": "string"}},
                    "additionalProperties": true,
                }),
                annotations: ToolAnnotations::read_only(),
            }
        }
        async fn invoke(&self, _args: Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    #[tokio::test]
    async fn schema_permitting_additional_properties_skips_the_check() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(OpenSchemaTool));
        assert!(r.check_args("open", &json!({"anything": 1})).is_ok());
        assert!(r.check_args("no_such_tool", &json!({"x": 1})).is_ok());
    }

    /// A tool that counts `descriptor()` calls.
    struct CountingTool {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolDescriptor {
                name: "counting",
                title: "Counting",
                description: "test tool",
                input_schema: json!({"type": "object"}),
                annotations: ToolAnnotations::read_only(),
            }
        }
        async fn invoke(&self, _args: Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    /// The `writes` auto-log decision on `tools/call` must not rebuild
    /// the descriptor: the flag is read once, at registration.
    #[tokio::test]
    async fn tools_call_does_not_rebuild_descriptor() {
        use crate::config::AutoLogToolCalls;
        use crate::mcp::dispatch::handle_tools_call;
        use crate::mcp::jsonrpc::Id;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let mut r = ToolRegistry::new();
        r.register(Arc::new(CountingTool {
            calls: Arc::clone(&calls),
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "register reads it once");
        assert_eq!(r.is_read_only("counting"), Some(true));
        for n in 0..3 {
            let resp = handle_tools_call(
                &r,
                None,
                None,
                None,
                None,
                AutoLogToolCalls::Writes,
                Id::Number(n),
                Some(json!({"name": "counting", "arguments": {}})),
            )
            .await;
            assert!(resp.error.is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "hot path must not rebuild");
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
    fn reject_unknown_args_names_every_stray_key() {
        let args = json!({"kind": "x", "text": "a", "content": "b"});
        let err = reject_unknown_args(&args, &["kind", "payload"], Some("use payload"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`content`") && err.contains("`text`"), "{err}");
        assert!(err.contains("use payload"), "{err}");
    }

    #[test]
    fn reject_unknown_args_accepts_declared_and_non_objects() {
        assert!(reject_unknown_args(&json!({"kind": "x"}), &["kind"], None).is_ok());
        assert!(reject_unknown_args(&json!({}), &["kind"], None).is_ok());
        assert!(reject_unknown_args(&Value::Null, &["kind"], None).is_ok());
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

    /// `[guardrails] secrets` must reach all four write tools through
    /// `defaults_with_schedulers`. Each tool defaults to `reject`, so
    /// if a `.with_secret_guard(secrets)` call is dropped from the
    /// wiring, that tool refuses the credential and this test fails.
    #[tokio::test]
    async fn secrets_mode_is_threaded_to_every_write_tool() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let seeded = semantic
            .remember(
                "original note",
                crate::memory::semantic::MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        let build = |mode| {
            ToolRegistry::defaults_with_schedulers(
                Arc::clone(&semantic),
                Arc::clone(&procedural),
                Arc::clone(&episodic),
                Arc::clone(&backing),
                ColdArchive::new(tmp.path()),
                1,
                None,
                None,
                ScopeState::new("personal"),
                None,
                size_tier::DEFAULT_MAX_CHARS,
                mode,
            )
        };
        // Assembled at runtime so this file does not trip secret scanners.
        let key = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let calls = [
            ("remember", json!({ "content": format!("key {key}") })),
            (
                "update",
                json!({ "id": seeded.to_string(), "content": format!("key {key}") }),
            ),
            ("pin", json!({ "content": format!("key {key}") })),
            (
                "record_event",
                json!({ "kind": "milestone", "payload": { "content": format!("key {key}") } }),
            ),
        ];

        let off = build(crate::config::SecretsMode::Off);
        for (name, args) in &calls {
            let res = off.get(name).unwrap().invoke(args.clone()).await.unwrap();
            assert!(!res.is_error, "{name} refused a secret with secrets = off");
            let advised = res
                .meta
                .as_ref()
                .is_some_and(|m| m.get("secret_advisory").is_some());
            assert!(!advised, "{name} scanned with secrets = off");
        }

        // The same calls are refused when the mode is `reject`, so the
        // pass above is down to the mode and not to a missing guard.
        let reject = build(crate::config::SecretsMode::Reject);
        for (name, args) in &calls {
            let res = reject
                .get(name)
                .unwrap()
                .invoke(args.clone())
                .await
                .unwrap();
            assert!(
                res.is_error,
                "{name} accepted a secret with secrets = reject"
            );
            assert_eq!(res.meta.unwrap()["error"]["code"], "secret_detected");
        }
    }
}
