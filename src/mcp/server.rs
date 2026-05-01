//! MCP dispatch loop.
//!
//! Reads JSON-RPC frames from a transport, routes by method, writes
//! responses back. The loop is single-tasked: handlers run in-line on
//! the same task that read the frame. This is fine for v0.1 because
//! handlers are sub-millisecond stubs. Phase 3 will spawn handler
//! tasks once we have I/O-bound work like embedding.
//!
//! Per the MCP lifecycle spec:
//!   1. client → `initialize` request
//!   2. server → `initialize` response (advertises capabilities)
//!   3. client → `notifications/initialized`
//!   4. arbitrary requests/responses until either side closes
//!
//! We do not enforce strict ordering: a request before `initialize`
//! returns `-32002` (server not initialized) so misbehaving clients
//! get a clear error rather than corrupted state.

use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::jsonrpc::{Id, Inbound, ParseError, Request, Response, error_codes, parse_inbound};
use super::resources::{ResourceError, ResourceRegistry, descriptor_to_json as resource_to_json};
use super::tools::{ToolError, ToolRegistry, descriptor_to_json as tool_to_json};
use super::transport::stdio::{FrameError, StdioTransport};
use super::{PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};
use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::working::ActiveSession;
use crate::scope::ScopeState;
use crate::storage::Storage;

// Tools consume the SemanticStore directly (Phase 3); the `Storage`
// handle on `Server` is now mostly informational, kept for future
// resources (e.g. `mneme://stats` reading the redb meta table) and
// to surface to handlers that touch storage outside the tool path.

/// MCP-specific error code. -32002 = "server not initialized" (per the
/// LSP convention MCP inherits — JSON-RPC -32000..-32099 is reserved
/// for server-defined codes).
const SERVER_NOT_INITIALIZED: i32 = -32002;

/// Cap on the `message` field included in `tool_call_failed` event
/// payloads. Bounded so a verbose error message doesn't bloat the
/// L3 hot tier; the full message is still in the JSON-RPC error
/// response the caller receives.
const FAILED_MESSAGE_CAP: usize = 500;

/// Build the L3 `tool_call` event payload for a successful invocation.
/// Per ADR-0009: extract only the per-tool value-bearing arg(s);
/// **never** the full `arguments` object. The resulting payload feeds
/// into the L3 hot tier and ages into the cold zstd archive on disk,
/// so leaking secrets through this path would be a privacy bug.
///
/// New tools added to the MCP surface MUST add an entry here in the
/// same PR. The default for unknown tools is the bare
/// `{"tool": <name>}` shape — explicit allowlist, not denylist.
fn tool_call_payload(name: &str, args: &Value) -> Value {
    let mut p = json!({ "tool": name });
    let obj = p
        .as_object_mut()
        .expect("json!({...}) yields an object literal");

    let pull = |obj: &mut serde_json::Map<String, Value>, args: &Value, key: &str| {
        if let Some(v) = args.get(key)
            && !matches!(v, Value::Null)
        {
            obj.insert(key.to_owned(), v.clone());
        }
    };

    match name {
        // L4 writers — content carries the user's actual data.
        "remember" => {
            pull(obj, args, "content");
            pull(obj, args, "kind");
            pull(obj, args, "scope");
        }
        // L0 writer — same shape as remember, no embedder involvement.
        "pin" => {
            pull(obj, args, "content");
            pull(obj, args, "scope");
        }
        // Mutators — id + new content if provided.
        "update" => {
            pull(obj, args, "id");
            pull(obj, args, "content");
            pull(obj, args, "kind");
        }
        "forget" => {
            pull(obj, args, "id");
        }
        "unpin" => {
            pull(obj, args, "id");
        }
        // Readers — record the search intent.
        "recall" => {
            pull(obj, args, "query");
            pull(obj, args, "k");
        }
        "recall_recent" => {
            pull(obj, args, "limit");
            pull(obj, args, "kind");
            pull(obj, args, "scope");
        }
        "summarize_session" => {
            pull(obj, args, "events");
            pull(obj, args, "scope");
        }
        // State change.
        "switch_scope" => {
            // Tool's arg is `scope`; expose under `new_scope` so the
            // L3 stream reads naturally ("we switched to <new>") on
            // recall_recent.
            if let Some(v) = args.get("scope") {
                obj.insert("new_scope".to_owned(), v.clone());
            }
        }
        // L3 producer (this very tool!) — capture the kind only, NOT
        // the payload. The payload is already being recorded by the
        // tool itself; double-recording would inflate the hot tier
        // with duplicate content.
        "record_event" => {
            pull(obj, args, "kind");
            pull(obj, args, "scope");
        }
        // Diagnostic / portability — no value-bearing args worth
        // mirroring. Bare `{"tool": <name>}` is enough.
        "stats" | "list_scopes" | "export" => {}
        // Unknown tool — bare payload, intentional default.
        _ => {}
    }
    p
}

/// Build the L3 `tool_call_failed` event payload. Capped message;
/// no args (privacy: failed remember/pin still received the args, we
/// don't re-emit them here).
fn tool_call_failed_payload(name: &str, error_kind: &str, message: &str) -> Value {
    let truncated: String = if message.len() > FAILED_MESSAGE_CAP {
        let mut s = message.chars().take(FAILED_MESSAGE_CAP).collect::<String>();
        s.push('…');
        s
    } else {
        message.to_owned()
    };
    json!({
        "tool": name,
        "error_kind": error_kind,
        "message": truncated,
    })
}

pub struct Server<R, W> {
    transport: StdioTransport<R, W>,
    tools: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    /// Available to handlers; Phase 2 tools don't consume it yet (per
    /// locked decision — wiring lands in Phase 3 with embeddings + HNSW).
    #[allow(dead_code)]
    storage: Arc<dyn Storage>,
    /// `Some(_)` in production (`cli::run` wires it); `None` in
    /// test fixtures that don't care about session checkpointing.
    /// Each successful `tools/call` pushes a turn here and pokes
    /// the checkpoint scheduler so the turn-count trigger sees it.
    session: Option<Arc<ActiveSession>>,
    /// Wakes after every successful `tools/call` so the scheduler
    /// can re-evaluate the turn-count threshold without waiting for
    /// the next wall-clock tick. `None` when `session` is `None`.
    checkpoint_scheduler: Option<Arc<CheckpointScheduler>>,
    /// `Some(_)` in production. Each successful `tools/call` is
    /// recorded as an `EpisodicEvent { kind: "tool_call" }` in L3
    /// hot tier. Closes the producer gap that left the L3 layer
    /// permanently empty pre-v0.2.3.
    episodic: Option<Arc<EpisodicStore>>,
    /// Tags the auto-emitted L3 event with the active default scope,
    /// matching `remember`/`pin` behaviour when the caller omits an
    /// explicit `scope` argument.
    scope_state: Option<Arc<ScopeState>>,
    initialized: AtomicBool,
}

impl<R, W> Server<R, W>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    pub fn new(
        transport: StdioTransport<R, W>,
        tools: Arc<ToolRegistry>,
        resources: Arc<ResourceRegistry>,
        storage: Arc<dyn Storage>,
    ) -> Self {
        Self {
            transport,
            tools,
            resources,
            storage,
            session: None,
            checkpoint_scheduler: None,
            episodic: None,
            scope_state: None,
            initialized: AtomicBool::new(false),
        }
    }

    /// Builder hook: attach the production runtime wiring so each
    /// successful `tools/call` (1) records a turn on the active
    /// session, (2) pokes the checkpoint scheduler, and (3) appends
    /// an L3 episodic event tagged with the current scope. `cli::run`
    /// calls this in production; stateless test fixtures skip it
    /// entirely (Server has `None` for all four slots).
    pub fn with_session(
        mut self,
        session: Arc<ActiveSession>,
        scheduler: Arc<CheckpointScheduler>,
        episodic: Arc<EpisodicStore>,
        scope_state: Arc<ScopeState>,
    ) -> Self {
        self.session = Some(session);
        self.checkpoint_scheduler = Some(scheduler);
        self.episodic = Some(episodic);
        self.scope_state = Some(scope_state);
        self
    }

    /// Run until the peer closes stdin (EOF) or sends `shutdown`.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let frame = match self.transport.read_frame().await {
                Ok(f) => f,
                Err(FrameError::Eof) => {
                    tracing::info!("peer closed stdin, shutting down");
                    return Ok(());
                }
                Err(FrameError::Oversize { cap }) => {
                    tracing::warn!(cap, "discarded oversize frame");
                    continue;
                }
                Err(FrameError::Io(e)) => return Err(e.into()),
            };

            match parse_inbound(&frame) {
                Ok(Inbound::Request(req)) => {
                    let resp = self.dispatch(req).await;
                    self.send(&resp).await?;
                }
                Ok(Inbound::Notification(n)) => {
                    if n.method == "notifications/initialized" {
                        self.initialized.store(true, Ordering::SeqCst);
                        tracing::info!("client signalled initialized");
                    } else {
                        tracing::debug!(method = %n.method, "ignoring notification");
                    }
                }
                Ok(Inbound::Response(_)) => {
                    tracing::debug!("ignoring unsolicited response (no client requests yet)");
                }
                Err(ParseError::InvalidJson(_)) => {
                    let resp = Response::error(Id::Null, error_codes::PARSE_ERROR, "parse error");
                    self.send(&resp).await?;
                }
                Err(e) => {
                    let resp =
                        Response::error(Id::Null, error_codes::INVALID_REQUEST, e.to_string());
                    self.send(&resp).await?;
                }
            }
        }
    }

    async fn send(&self, resp: &Response) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(resp)?;
        self.transport.write_frame(&bytes).await?;
        Ok(())
    }

    async fn dispatch(&self, req: Request) -> Response {
        let id = req.id.clone();
        match req.method.as_str() {
            "initialize" => self.handle_initialize(id, req.params).await,
            "ping" => Response::success(id, json!({})),
            "shutdown" => Response::success(id, json!({})),
            method if !self.initialized.load(Ordering::SeqCst) => Response::error(
                id,
                SERVER_NOT_INITIALIZED,
                format!("server not initialized; got {method}"),
            ),
            "tools/list" => self.handle_tools_list(id),
            "tools/call" => self.handle_tools_call(id, req.params).await,
            "resources/list" => self.handle_resources_list(id),
            "resources/read" => self.handle_resources_read(id, req.params).await,
            "prompts/list" => Response::success(id, json!({ "prompts": [] })),
            method => Response::error(
                id,
                error_codes::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            ),
        }
    }

    async fn handle_initialize(&self, id: Id, _params: Option<Value>) -> Response {
        // We accept whatever protocolVersion the client requested — but
        // we always advertise our own. Per spec the client decides
        // whether to proceed if mismatched.
        let result = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "listChanged": false, "subscribe": false },
                "prompts": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION
            }
        });
        Response::success(id, result)
    }

    fn handle_tools_list(&self, id: Id) -> Response {
        let tools: Vec<Value> = self.tools.list().iter().map(tool_to_json).collect();
        Response::success(id, json!({ "tools": tools }))
    }

    async fn handle_tools_call(&self, id: Id, params: Option<Value>) -> Response {
        let params = match params {
            Some(p) => p,
            None => {
                return Response::error(id, error_codes::INVALID_PARAMS, "missing params");
            }
        };
        let name = match params.get("name").and_then(Value::as_str) {
            Some(n) => n.to_string(),
            None => {
                return Response::error(id, error_codes::INVALID_PARAMS, "missing `name`");
            }
        };
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        let tool = match self.tools.get(&name) {
            Some(t) => t,
            None => {
                // Unknown tool — emit a `tool_call_failed` event
                // (failures often matter most; ADR-0009) and return
                // the JSON-RPC error.
                self.emit_tool_call_failed(
                    &name,
                    "MethodNotFound",
                    &format!("unknown tool: {name}"),
                )
                .await;
                return Response::error(
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    format!("unknown tool: {name}"),
                );
            }
        };

        match tool.invoke(args.clone()).await {
            Ok(result) => {
                // Record the turn on the active session and poke the
                // checkpoint scheduler. Two reasons to do it here, not
                // before the invoke: (1) we don't want failed calls to
                // count as turns; (2) we don't want to poke the
                // scheduler if the call ultimately errors out and
                // produces no observable effect.
                if let (Some(session), Some(sched)) =
                    (self.session.as_ref(), self.checkpoint_scheduler.as_ref())
                {
                    session.push_turn("tool", &name);
                    sched.poke();
                }
                // Auto-emit the L3 `tool_call` event with a per-tool
                // payload extracted from `args` (ADR-0009). Never the
                // full args object — privacy.
                if let (Some(episodic), Some(scope_state)) =
                    (self.episodic.as_ref(), self.scope_state.as_ref())
                {
                    let scope = scope_state.current();
                    let payload = tool_call_payload(&name, &args);
                    if let Err(e) = episodic.record_json("tool_call", &scope, &payload).await {
                        // Non-fatal: the user already got their tool result;
                        // a missed L3 event is a degraded-mode signal, not a
                        // reason to fail the request.
                        tracing::warn!(
                            error = %e,
                            tool = %name,
                            "failed to record episodic tool_call event"
                        );
                    }
                }
                Response::success(id, result.to_json())
            }
            Err(ToolError::InvalidArguments(msg)) => {
                self.emit_tool_call_failed(&name, "InvalidArguments", &msg)
                    .await;
                Response::error(id, error_codes::INVALID_PARAMS, msg)
            }
            Err(ToolError::NotFound(msg)) => {
                self.emit_tool_call_failed(&name, "NotFound", &msg).await;
                Response::error(id, error_codes::METHOD_NOT_FOUND, msg)
            }
            Err(ToolError::Internal(msg)) => {
                self.emit_tool_call_failed(&name, "Internal", &msg).await;
                Response::error(id, error_codes::INTERNAL_ERROR, msg)
            }
        }
    }

    /// Emit a `tool_call_failed` L3 event when a tool dispatch fails.
    /// Mirrors the success path's emit but skips the L1 turn / checkpoint
    /// poke (failed calls don't count as turns — same logic that
    /// protected `turns_total` pre-v0.2.4). Per ADR-0009.
    async fn emit_tool_call_failed(&self, tool_name: &str, error_kind: &str, message: &str) {
        if let (Some(episodic), Some(scope_state)) =
            (self.episodic.as_ref(), self.scope_state.as_ref())
        {
            let scope = scope_state.current();
            let payload = tool_call_failed_payload(tool_name, error_kind, message);
            if let Err(e) = episodic
                .record_json("tool_call_failed", &scope, &payload)
                .await
            {
                tracing::warn!(
                    error = %e,
                    tool = %tool_name,
                    "failed to record episodic tool_call_failed event"
                );
            }
        }
    }

    fn handle_resources_list(&self, id: Id) -> Response {
        let resources: Vec<Value> = self.resources.list().iter().map(resource_to_json).collect();
        Response::success(id, json!({ "resources": resources }))
    }

    async fn handle_resources_read(&self, id: Id, params: Option<Value>) -> Response {
        let uri = match params
            .as_ref()
            .and_then(|p| p.get("uri"))
            .and_then(Value::as_str)
        {
            Some(u) => u.to_string(),
            None => {
                return Response::error(id, error_codes::INVALID_PARAMS, "missing `uri`");
            }
        };

        let resource = match self.resources.get(&uri) {
            Some(r) => r,
            None => {
                return Response::error(
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    format!("unknown resource: {uri}"),
                );
            }
        };

        match resource.read(&uri).await {
            Ok(content) => Response::success(id, json!({ "contents": [content.to_json()] })),
            Err(ResourceError::NotFound(msg)) => {
                Response::error(id, error_codes::METHOD_NOT_FOUND, msg)
            }
            Err(ResourceError::Internal(msg)) => {
                Response::error(id, error_codes::INTERNAL_ERROR, msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::jsonrpc::Id;

    /// Drive the server with a sequence of input frames and collect the
    /// frames it writes back. Returns once stdin EOF is reached. The
    /// `TempDir` returned alongside the parsed responses keeps the
    /// SemanticStore's WAL directory alive long enough for tests that
    /// want to look at the side-effects on disk.
    async fn drive(input: &[u8]) -> Vec<Value> {
        use crate::embed::Embedder;
        use crate::embed::stub::StubEmbedder;
        use crate::memory::episodic::EpisodicStore;
        use crate::memory::procedural::ProceduralStore;
        use crate::memory::semantic::SemanticStore;
        use crate::orchestrator::{Orchestrator, TokenBudget};
        use crate::storage::archive::ColdArchive;

        let tmp = tempfile::TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = crate::storage::memory_impl::MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let orchestrator = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        ));
        let cold = ColdArchive::new(tmp.path());

        let transport = StdioTransport::new(input, Vec::<u8>::new());
        let mut server = Server::new(
            transport,
            Arc::new(ToolRegistry::defaults(
                Arc::clone(&semantic),
                Arc::clone(&procedural),
                Arc::clone(&episodic),
                Arc::clone(&storage),
                cold.clone(),
                1,
            )),
            Arc::new(ResourceRegistry::defaults(
                semantic,
                procedural,
                episodic,
                orchestrator,
                cold,
                1,
                TokenBudget::for_tests(2000),
            )),
            storage,
        );
        server.run().await.unwrap();
        let bytes = server.transport.into_writer();
        // `tmp` stays alive until end-of-scope — drop after parsing.
        let out = parse_lines(&bytes);
        drop(tmp);
        out
    }

    fn parse_lines(bytes: &[u8]) -> Vec<Value> {
        bytes
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("server emitted invalid JSON"))
            .collect()
    }

    fn req(id: i64, method: &str, params: Value) -> String {
        let mut s = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        s.push('\n');
        s
    }

    fn req_no_params(id: i64, method: &str) -> String {
        let mut s = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        }))
        .unwrap();
        s.push('\n');
        s
    }

    fn notif(method: &str) -> String {
        let mut s = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": method,
        }))
        .unwrap();
        s.push('\n');
        s
    }

    #[tokio::test]
    async fn initialize_then_tools_list() {
        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req_no_params(2, "tools/list"));

        let out = drive(input.as_bytes()).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(out[0]["result"]["serverInfo"]["name"], SERVER_NAME);
        let tools = out[1]["result"]["tools"].as_array().unwrap();
        // Phase 6 surface + switch_scope (v0.15) + record_event
        // (v0.2.4, ADR-0008): 13 tools across L0/L3/L4 + diagnostics
        // (export, forget, list_scopes, pin, recall, recall_recent,
        // record_event, remember, stats, summarize_session,
        // switch_scope, unpin, update).
        assert_eq!(tools.len(), 13);
    }

    #[tokio::test]
    async fn before_initialize_returns_error() {
        let input = req_no_params(1, "tools/list");
        let out = drive(input.as_bytes()).await;
        assert_eq!(out[0]["error"]["code"], SERVER_NOT_INITIALIZED);
    }

    #[tokio::test]
    async fn tools_call_remember_returns_ulid() {
        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req(
            2,
            "tools/call",
            json!({"name": "remember", "arguments": {"content": "hi"}}),
        ));
        let out = drive(input.as_bytes()).await;
        assert_eq!(out.len(), 2);
        let text = out[1]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("stored memory "));
    }

    #[tokio::test]
    async fn unknown_tool_returns_method_not_found() {
        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req(
            2,
            "tools/call",
            json!({"name": "nope", "arguments": {}}),
        ));
        let out = drive(input.as_bytes()).await;
        assert_eq!(out[1]["error"]["code"], error_codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn resources_list_and_read() {
        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req_no_params(2, "resources/list"));
        input.push_str(&req(3, "resources/read", json!({"uri": "mneme://stats"})));
        let out = drive(input.as_bytes()).await;
        assert_eq!(out.len(), 3);
        let resources = out[1]["result"]["resources"].as_array().unwrap();
        // Phase 5 surface: context, procedural, recent, stats.
        assert_eq!(resources.len(), 4);
        let contents = out[2]["result"]["contents"].as_array().unwrap();
        assert_eq!(contents[0]["mimeType"], "application/json");
        let body: Value = serde_json::from_str(contents[0]["text"].as_str().unwrap()).unwrap();
        // Phase 6 stats reports real counts; just assert shape here.
        assert!(body["schema_version"].is_number());
        assert!(body["memories"]["semantic"].is_number());
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error_and_continues() {
        let mut input = String::from("{ not json\n");
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        let out = drive(input.as_bytes()).await;
        assert_eq!(out[0]["error"]["code"], error_codes::PARSE_ERROR);
        assert_eq!(out[0]["id"], Value::Null);
        assert!(out[1]["result"]["protocolVersion"].is_string());
    }

    #[tokio::test]
    async fn unknown_method_after_init() {
        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req_no_params(2, "no/such/method"));
        let out = drive(input.as_bytes()).await;
        assert_eq!(out[1]["error"]["code"], error_codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn ping_works_before_init() {
        let input = req_no_params(1, "ping");
        let out = drive(input.as_bytes()).await;
        assert!(out[0]["result"].is_object());
        assert!(out[0].get("error").is_none() || out[0]["error"].is_null());
    }

    #[tokio::test]
    async fn id_is_preserved_on_error() {
        let mut input = String::new();
        input.push_str(&req_no_params(42, "tools/list")); // before init
        let out = drive(input.as_bytes()).await;
        assert_eq!(out[0]["id"], 42);
    }

    #[allow(dead_code)]
    fn _ensure_id_used() {
        let _ = Id::Number(0);
    }

    /// Closes the L3 producer gap. Until v0.2.3 the episodic hot
    /// tier had no production writer — every `EpisodicStore::record*`
    /// call lived in `#[cfg(test)]`. This test pins the auto-emit
    /// contract: every successful `tools/call` MUST land one
    /// `EpisodicEvent { kind: "tool_call" }` in the hot tier, tagged
    /// with the active scope and the called tool's name.
    #[tokio::test]
    async fn successful_tools_call_emits_episodic_event() {
        use crate::embed::Embedder;
        use crate::embed::stub::StubEmbedder;
        use crate::memory::checkpoint_scheduler::{CheckpointScheduler, CheckpointSchedulerConfig};
        use crate::memory::episodic::{EpisodicStore, RecentFilters};
        use crate::memory::procedural::ProceduralStore;
        use crate::memory::semantic::SemanticStore;
        use crate::memory::working::ActiveSession;
        use crate::orchestrator::{Orchestrator, TokenBudget};
        use crate::scope::ScopeState;
        use crate::storage::archive::ColdArchive;

        let tmp = tempfile::TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = crate::storage::memory_impl::MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let orchestrator = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        ));
        let cold = ColdArchive::new(tmp.path());
        let active_session = ActiveSession::open(tmp.path().join("sessions")).unwrap();
        let checkpoint_scheduler = CheckpointScheduler::start(
            Arc::clone(&active_session),
            CheckpointSchedulerConfig::disabled(),
        );
        let scope_state = ScopeState::new("work");

        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req(
            2,
            "tools/call",
            json!({"name": "remember", "arguments": {"content": "hi"}}),
        ));

        let transport = StdioTransport::new(input.as_bytes(), Vec::<u8>::new());
        let mut server = Server::new(
            transport,
            Arc::new(ToolRegistry::defaults(
                Arc::clone(&semantic),
                Arc::clone(&procedural),
                Arc::clone(&episodic),
                Arc::clone(&storage),
                cold.clone(),
                1,
            )),
            Arc::new(ResourceRegistry::defaults(
                semantic,
                procedural,
                Arc::clone(&episodic),
                orchestrator,
                cold,
                1,
                TokenBudget::for_tests(2000),
            )),
            storage,
        )
        .with_session(
            active_session,
            checkpoint_scheduler,
            Arc::clone(&episodic),
            Arc::clone(&scope_state),
        );
        server.run().await.unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "expected one tool_call event");
        let evt = &events[0];
        assert_eq!(evt.kind, "tool_call");
        assert_eq!(evt.scope, "work");
        let payload = evt.payload_json().unwrap();
        assert_eq!(payload["tool"], "remember");
    }

    /// Failed tool calls emit `kind="tool_call_failed"` (ADR-0009)
    /// but DO NOT push a turn to the active session — failures
    /// don't count as turns, same logic that protects `turns_total`.
    /// Pre-v0.2.4 this test asserted no emit at all; v0.2.4 changes
    /// the contract: emit with a different kind, still no L1 turn.
    #[tokio::test]
    async fn failed_tools_call_emits_tool_call_failed_only() {
        use crate::embed::Embedder;
        use crate::embed::stub::StubEmbedder;
        use crate::memory::checkpoint_scheduler::{CheckpointScheduler, CheckpointSchedulerConfig};
        use crate::memory::episodic::{EpisodicStore, RecentFilters};
        use crate::memory::procedural::ProceduralStore;
        use crate::memory::semantic::SemanticStore;
        use crate::memory::working::ActiveSession;
        use crate::orchestrator::{Orchestrator, TokenBudget};
        use crate::scope::ScopeState;
        use crate::storage::archive::ColdArchive;

        let tmp = tempfile::TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = crate::storage::memory_impl::MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let orchestrator = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        ));
        let cold = ColdArchive::new(tmp.path());
        let active_session = ActiveSession::open(tmp.path().join("sessions")).unwrap();
        let checkpoint_scheduler = CheckpointScheduler::start(
            Arc::clone(&active_session),
            CheckpointSchedulerConfig::disabled(),
        );
        let scope_state = ScopeState::new("personal");

        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req(
            2,
            "tools/call",
            json!({"name": "no_such_tool", "arguments": {}}),
        ));

        let transport = StdioTransport::new(input.as_bytes(), Vec::<u8>::new());
        let mut server = Server::new(
            transport,
            Arc::new(ToolRegistry::defaults(
                Arc::clone(&semantic),
                Arc::clone(&procedural),
                Arc::clone(&episodic),
                Arc::clone(&storage),
                cold.clone(),
                1,
            )),
            Arc::new(ResourceRegistry::defaults(
                semantic,
                procedural,
                Arc::clone(&episodic),
                orchestrator,
                cold,
                1,
                TokenBudget::for_tests(2000),
            )),
            storage,
        )
        .with_session(
            active_session,
            checkpoint_scheduler,
            Arc::clone(&episodic),
            scope_state,
        );
        server.run().await.unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        // v0.2.4: ADR-0009 turns this into a `tool_call_failed` emit
        // (was: no emit at all pre-v0.2.4). Should be exactly one
        // event, with the failed kind, payload carrying the error
        // class and the (truncated) message.
        assert_eq!(events.len(), 1, "expected one tool_call_failed event");
        let evt = &events[0];
        assert_eq!(evt.kind, "tool_call_failed");
        let payload = evt.payload_json().unwrap();
        assert_eq!(payload["tool"], "no_such_tool");
        assert_eq!(payload["error_kind"], "MethodNotFound");
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("no_such_tool")
        );
    }

    /// `tool_call` payload is enriched per-tool (ADR-0009): the
    /// value-bearing arg lands in the L3 event so `recall_recent`
    /// reconstructs intent ("we remembered <content>", "we recalled
    /// <query>"). Full args MUST NOT be mirrored — privacy.
    #[tokio::test]
    async fn successful_tools_call_payload_is_enriched_per_tool() {
        use crate::embed::Embedder;
        use crate::embed::stub::StubEmbedder;
        use crate::memory::checkpoint_scheduler::{CheckpointScheduler, CheckpointSchedulerConfig};
        use crate::memory::episodic::{EpisodicStore, RecentFilters};
        use crate::memory::procedural::ProceduralStore;
        use crate::memory::semantic::SemanticStore;
        use crate::memory::working::ActiveSession;
        use crate::orchestrator::{Orchestrator, TokenBudget};
        use crate::scope::ScopeState;
        use crate::storage::archive::ColdArchive;

        let tmp = tempfile::TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = crate::storage::memory_impl::MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let orchestrator = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        ));
        let cold = ColdArchive::new(tmp.path());
        let active_session = ActiveSession::open(tmp.path().join("sessions")).unwrap();
        let checkpoint_scheduler = CheckpointScheduler::start(
            Arc::clone(&active_session),
            CheckpointSchedulerConfig::disabled(),
        );
        let scope_state = ScopeState::new("work");

        let mut input = String::new();
        input.push_str(&req(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        input.push_str(&notif("notifications/initialized"));
        input.push_str(&req(
            2,
            "tools/call",
            json!({"name": "remember", "arguments": {
                "content": "we use redb",
                "kind": "fact",
                "scope": "work",
                "tags": ["storage"],
            }}),
        ));

        let transport = StdioTransport::new(input.as_bytes(), Vec::<u8>::new());
        let mut server = Server::new(
            transport,
            Arc::new(ToolRegistry::defaults(
                Arc::clone(&semantic),
                Arc::clone(&procedural),
                Arc::clone(&episodic),
                Arc::clone(&storage),
                cold.clone(),
                1,
            )),
            Arc::new(ResourceRegistry::defaults(
                semantic,
                procedural,
                Arc::clone(&episodic),
                orchestrator,
                cold,
                1,
                TokenBudget::for_tests(2000),
            )),
            storage,
        )
        .with_session(
            active_session,
            checkpoint_scheduler,
            Arc::clone(&episodic),
            Arc::clone(&scope_state),
        );
        server.run().await.unwrap();

        let events = episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let evt = &events[0];
        assert_eq!(evt.kind, "tool_call");
        let payload = evt.payload_json().unwrap();
        assert_eq!(payload["tool"], "remember");
        assert_eq!(payload["content"], "we use redb");
        assert_eq!(payload["kind"], "fact");
        assert_eq!(payload["scope"], "work");
        // `tags` was passed but is NOT in the per-tool extraction
        // table (ADR-0009 keeps the payload narrow); confirm it
        // didn't leak through.
        assert!(
            payload.get("tags").is_none(),
            "tags should NOT be in payload"
        );
    }

    #[test]
    fn tool_call_payload_per_tool_extraction() {
        // White-box test of the extraction table. Catches drift if
        // someone adds a tool and forgets to add an entry to the
        // match in `tool_call_payload`.

        // remember: content + kind + scope (tags excluded)
        let p = tool_call_payload(
            "remember",
            &json!({"content": "x", "kind": "fact", "scope": "work", "tags": ["a"]}),
        );
        assert_eq!(p["tool"], "remember");
        assert_eq!(p["content"], "x");
        assert_eq!(p["kind"], "fact");
        assert_eq!(p["scope"], "work");
        assert!(p.get("tags").is_none());

        // recall: query + k
        let p = tool_call_payload("recall", &json!({"query": "redb", "k": 5, "scope": "x"}));
        assert_eq!(p["query"], "redb");
        assert_eq!(p["k"], 5);
        assert!(
            p.get("scope").is_none(),
            "recall has no scope arg in extraction"
        );

        // forget: id only
        let p = tool_call_payload("forget", &json!({"id": "01ABC"}));
        assert_eq!(p["id"], "01ABC");

        // switch_scope: arg `scope` exposed as `new_scope`
        let p = tool_call_payload("switch_scope", &json!({"scope": "work"}));
        assert_eq!(p["new_scope"], "work");
        assert!(p.get("scope").is_none());

        // record_event: kind only (NOT payload — would double-record)
        let p = tool_call_payload(
            "record_event",
            &json!({"kind": "decision", "payload": {"content": "secret"}}),
        );
        assert_eq!(p["kind"], "decision");
        assert!(
            p.get("payload").is_none(),
            "record_event payload must not double-record"
        );

        // Diagnostic tools: just {tool}
        for diag in ["stats", "list_scopes", "export"] {
            let p = tool_call_payload(diag, &json!({"limit": 100}));
            assert_eq!(p["tool"], diag);
            assert!(p.get("limit").is_none(), "{diag} must not leak args");
        }

        // Unknown tool: bare default, no args leak
        let p = tool_call_payload("brand_new_tool", &json!({"secret": "xyz"}));
        assert_eq!(p["tool"], "brand_new_tool");
        assert!(
            p.get("secret").is_none(),
            "unknown tools must not leak args"
        );
    }

    #[test]
    fn tool_call_failed_payload_truncates_long_messages() {
        let long = "x".repeat(800);
        let p = tool_call_failed_payload("remember", "Internal", &long);
        assert_eq!(p["tool"], "remember");
        assert_eq!(p["error_kind"], "Internal");
        let msg = p["message"].as_str().unwrap();
        assert!(msg.len() <= FAILED_MESSAGE_CAP + 4); // +4 for the `…` UTF-8 bytes
        assert!(msg.ends_with('…'));
    }

    #[test]
    fn tool_call_failed_payload_short_message_is_verbatim() {
        let p = tool_call_failed_payload("forget", "NotFound", "no such id");
        assert_eq!(p["message"], "no such id");
    }
}
