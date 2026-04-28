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
use crate::storage::Storage;

// Tools consume the SemanticStore directly (Phase 3); the `Storage`
// handle on `Server` is now mostly informational, kept for future
// resources (e.g. `mneme://stats` reading the redb meta table) and
// to surface to handlers that touch storage outside the tool path.

/// MCP-specific error code. -32002 = "server not initialized" (per the
/// LSP convention MCP inherits — JSON-RPC -32000..-32099 is reserved
/// for server-defined codes).
const SERVER_NOT_INITIALIZED: i32 = -32002;

pub struct Server<R, W> {
    transport: StdioTransport<R, W>,
    tools: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    /// Available to handlers; Phase 2 tools don't consume it yet (per
    /// locked decision — wiring lands in Phase 3 with embeddings + HNSW).
    #[allow(dead_code)]
    storage: Arc<dyn Storage>,
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
            initialized: AtomicBool::new(false),
        }
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
                return Response::error(
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    format!("unknown tool: {name}"),
                );
            }
        };

        match tool.invoke(args).await {
            Ok(result) => Response::success(id, result.to_json()),
            Err(ToolError::InvalidArguments(msg)) => {
                Response::error(id, error_codes::INVALID_PARAMS, msg)
            }
            Err(ToolError::NotFound(msg)) => {
                Response::error(id, error_codes::METHOD_NOT_FOUND, msg)
            }
            Err(ToolError::Internal(msg)) => Response::error(id, error_codes::INTERNAL_ERROR, msg),
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

        match resource.read().await {
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
        // Phase 4 surface: forget, pin, recall, recall_recent,
        // remember, summarize_session, unpin.
        assert_eq!(tools.len(), 7);
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
}
