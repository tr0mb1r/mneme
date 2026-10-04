//! MCP Streamable HTTP transport (protocol revision `2025-06-18`).
//!
//! Lets an agent that can't spawn a local subprocess — typically one
//! running in another container on the same Docker / Compose network —
//! talk to mneme over TCP. `mneme serve` runs this as its only
//! transport; `mneme daemon` runs it next to the Unix socket when
//! `[http] enabled = true`.
//!
//! Endpoints:
//!
//! - `POST /mcp` — one JSON-RPC message per request body. Requests get
//!   `200 application/json` carrying the response; notifications and
//!   client responses get `202 Accepted` with no body. mneme never
//!   sends server→client requests, so it never needs to upgrade a POST
//!   to an SSE stream.
//! - `GET /mcp` — `405`. The spec lets a server decline the standalone
//!   SSE stream, and mneme has nothing to push.
//! - `DELETE /mcp` — ends the session named by `Mcp-Session-Id`.
//! - `GET /healthz` — unauthenticated liveness probe for container
//!   health checks. Reveals nothing beyond "the process is up".
//!
//! Sessions: an `initialize` request mints a fresh [`McpSession`]
//! (with its own scope cell, same SEC-001 isolation as the daemon's
//! per-connection registries) and returns its id in the
//! `Mcp-Session-Id` response header. Every later request must echo
//! it; an unknown id gets `404`, which tells a spec-compliant client to
//! re-initialize. Sessions idle for longer than the TTL are swept, and
//! the table is capped so a misbehaving client can't grow it unbounded.
//!
//! Security:
//!
//! - Every `/mcp` request must carry `Authorization: Bearer <token>`.
//!   The token comes from `$MNEME_HTTP_TOKEN`, a file named by
//!   `$MNEME_HTTP_TOKEN_FILE` / `[http] token_file` (Docker secrets),
//!   or the daemon's own `<root>/run/auth.token`. File-backed tokens
//!   are re-read per request so `mneme auth rotate` applies without a
//!   restart. Comparison is constant-time.
//! - Requests carrying an `Origin` header are rejected unless the
//!   origin is allow-listed (DNS-rebinding defence the spec requires).
//!   Non-browser MCP clients don't send `Origin`, so the default empty
//!   list blocks browsers without affecting agents.
//! - Bodies are capped at the same 16 MiB as a stdio frame.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{get, post};
use base64::Engine as _;
use rand::RngCore;
use serde_json::Value;

use super::stdio::MAX_FRAME_BYTES;
use crate::mcp::jsonrpc::{Id, Response, error_codes};
use crate::mcp::server::McpSession;

/// Path of the MCP endpoint.
pub const MCP_PATH: &str = "/mcp";
/// Path of the unauthenticated liveness probe.
pub const HEALTH_PATH: &str = "/healthz";
/// Session header name. HTTP header names are case-insensitive; the
/// spec spells it `Mcp-Session-Id`.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// Default cap on concurrent sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 256;
/// Default idle lifetime of a session. Long, because agents such as
/// Hermes keep one MCP connection open for the life of a gateway
/// process; a swept session costs the client a re-initialize.
pub const DEFAULT_SESSION_IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the sweeper looks for idle sessions.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Where the bearer token comes from.
#[derive(Debug, Clone)]
pub enum TokenSource {
    /// A fixed value, e.g. from `$MNEME_HTTP_TOKEN`.
    Static(String),
    /// A file re-read on every request, so rotating it takes effect
    /// without restarting mneme.
    File(PathBuf),
}

impl TokenSource {
    /// Current token, surrounding whitespace trimmed (Docker secrets
    /// and `echo > file` both leave a trailing newline).
    fn load(&self) -> std::io::Result<String> {
        match self {
            Self::Static(s) => Ok(s.trim().to_owned()),
            Self::File(p) => Ok(std::fs::read_to_string(p)?.trim().to_owned()),
        }
    }

    /// Human-readable origin for boot logs. Never includes the value.
    pub fn describe(&self) -> String {
        match self {
            Self::Static(_) => "$MNEME_HTTP_TOKEN".to_owned(),
            Self::File(p) => p.display().to_string(),
        }
    }
}

/// Everything the HTTP listener needs besides the session factory.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub bind: SocketAddr,
    pub token: TokenSource,
    /// Exact `Origin` values to accept; `"*"` accepts any.
    pub allowed_origins: Vec<String>,
    pub max_sessions: usize,
    pub session_idle_ttl: Duration,
}

/// Builds a fresh, fully wired [`McpSession`] for each `initialize`.
pub type SessionFactory = Arc<dyn Fn() -> McpSession + Send + Sync>;

struct Entry {
    session: Arc<McpSession>,
    last_seen: Instant,
}

struct AppState {
    factory: SessionFactory,
    sessions: Mutex<HashMap<String, Entry>>,
    token: TokenSource,
    allowed_origins: Vec<String>,
    max_sessions: usize,
    session_idle_ttl: Duration,
}

/// Bind `opts.bind` and serve until `shutdown` resolves.
pub async fn serve<F>(
    opts: HttpOptions,
    factory: SessionFactory,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = bind(&opts).await?;
    serve_on(listener, opts, factory, shutdown).await
}

/// Bind the listening socket without serving yet, so a caller that runs
/// HTTP next to another transport can fail fast on a taken port.
pub async fn bind(opts: &HttpOptions) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(opts.bind).await.map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("could not bind HTTP listener on {}: {e}", opts.bind),
        )
    })
}

/// Serve on an already-bound listener until `shutdown` resolves.
pub async fn serve_on<F>(
    listener: tokio::net::TcpListener,
    opts: HttpOptions,
    factory: SessionFactory,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let local = listener.local_addr()?;
    let state = new_state(&opts, factory);
    tracing::info!(
        bind = %local,
        token_source = %opts.token.describe(),
        "transport=http: listening on http://{local}{MCP_PATH}"
    );
    let sweeper = tokio::spawn(sweep_loop(Arc::clone(&state)));
    let result = axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown)
        .await;
    sweeper.abort();
    tracing::info!("transport=http: listener stopped");
    result
}

/// The router on its own, for in-process tests.
pub fn router(opts: &HttpOptions, factory: SessionFactory) -> Router {
    build_router(new_state(opts, factory))
}

fn new_state(opts: &HttpOptions, factory: SessionFactory) -> Arc<AppState> {
    Arc::new(AppState {
        factory,
        sessions: Mutex::new(HashMap::new()),
        token: opts.token.clone(),
        allowed_origins: opts.allowed_origins.clone(),
        max_sessions: opts.max_sessions.max(1),
        session_idle_ttl: opts.session_idle_ttl,
    })
}

fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(MCP_PATH, post(post_mcp).get(get_mcp).delete(delete_mcp))
        .route(HEALTH_PATH, get(health))
        .layer(DefaultBodyLimit::max(MAX_FRAME_BYTES))
        .with_state(state)
}

async fn sweep_loop(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        tick.tick().await;
        let evicted = state.evict_idle();
        if evicted > 0 {
            tracing::info!(evicted, "transport=http: swept idle sessions");
        }
    }
}

impl AppState {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A panic while holding this lock can only come from a bug in
        // the few lines below; the map itself is still consistent.
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn evict_idle(&self) -> usize {
        let ttl = self.session_idle_ttl;
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, e| e.last_seen.elapsed() < ttl);
        before - map.len()
    }

    fn check_origin(&self, headers: &HeaderMap) -> Result<(), Reject> {
        let Some(origin) = headers.get(header::ORIGIN) else {
            return Ok(());
        };
        let origin = origin.to_str().unwrap_or("");
        if self.allowed_origins.iter().any(|o| o == "*" || o == origin) {
            return Ok(());
        }
        tracing::warn!(
            origin,
            "transport=http: rejected request from disallowed Origin"
        );
        Err(Reject(StatusCode::FORBIDDEN, "origin not allowed"))
    }

    fn check_auth(&self, headers: &HeaderMap) -> Result<(), Reject> {
        let expected = match self.token.load() {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => {
                tracing::error!(
                    source = %self.token.describe(),
                    "transport=http: auth token is empty; refusing every request"
                );
                return Err(Reject(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server auth token is not configured",
                ));
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    source = %self.token.describe(),
                    "transport=http: failed to read auth token"
                );
                return Err(Reject(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server auth token is unreadable",
                ));
            }
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, rest) = v.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
            });
        match presented {
            Some(p) if crate::daemon::auth::tokens_match(p.as_bytes(), expected.as_bytes()) => {
                Ok(())
            }
            _ => {
                tracing::warn!(
                    "transport=http: rejected request with missing or wrong bearer token"
                );
                Err(Reject(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token",
                ))
            }
        }
    }

    fn create_session(&self) -> Result<(String, Arc<McpSession>), Reject> {
        let ttl = self.session_idle_ttl;
        let mut map = self.lock();
        map.retain(|_, e| e.last_seen.elapsed() < ttl);
        if map.len() >= self.max_sessions {
            tracing::warn!(
                max = self.max_sessions,
                "transport=http: session table full; rejecting initialize"
            );
            return Err(Reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many open MCP sessions; close one with DELETE /mcp",
            ));
        }
        let id = new_session_id();
        let session = Arc::new((self.factory)());
        map.insert(
            id.clone(),
            Entry {
                session: Arc::clone(&session),
                last_seen: Instant::now(),
            },
        );
        tracing::info!(active = map.len(), "transport=http: session opened");
        Ok((id, session))
    }

    fn lookup(&self, headers: &HeaderMap) -> Result<Arc<McpSession>, Reject> {
        let id = session_id(headers)?;
        let mut map = self.lock();
        match map.get_mut(id) {
            Some(e) if e.last_seen.elapsed() < self.session_idle_ttl => {
                e.last_seen = Instant::now();
                Ok(Arc::clone(&e.session))
            }
            Some(_) => {
                map.remove(id);
                Err(Reject(
                    StatusCode::NOT_FOUND,
                    "session expired; re-initialize",
                ))
            }
            None => Err(Reject(
                StatusCode::NOT_FOUND,
                "unknown session; re-initialize",
            )),
        }
    }
}

async fn post_mcp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    if let Err(r) = state.check_origin(&headers) {
        return r.into_response();
    }
    if let Err(r) = state.check_auth(&headers) {
        return r.into_response();
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return rpc(
                StatusCode::BAD_REQUEST,
                &Response::error(Id::Null, error_codes::PARSE_ERROR, "parse error"),
            );
        }
    };
    if value.is_array() {
        // MCP 2025-06-18 removed JSON-RPC batching.
        return rpc(
            StatusCode::BAD_REQUEST,
            &Response::error(
                Id::Null,
                error_codes::INVALID_REQUEST,
                "JSON-RPC batches are not supported; send one message per request",
            ),
        );
    }

    let is_initialize = value.get("method").and_then(Value::as_str) == Some("initialize")
        && value.get("id").is_some();
    if is_initialize {
        let (id, session) = match state.create_session() {
            Ok(x) => x,
            Err(r) => return r.into_response(),
        };
        let mut resp = match session.handle_frame(&body).await {
            Some(r) => rpc(StatusCode::OK, &r),
            None => StatusCode::ACCEPTED.into_response(),
        };
        if let Ok(v) = HeaderValue::from_str(&id) {
            resp.headers_mut().insert(SESSION_HEADER, v);
        }
        return resp;
    }

    let session = match state.lookup(&headers) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match session.handle_frame(&body).await {
        Some(r) => rpc(StatusCode::OK, &r),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn get_mcp(State(state): State<Arc<AppState>>, headers: HeaderMap) -> HttpResponse {
    if let Err(r) = state.check_origin(&headers) {
        return r.into_response();
    }
    if let Err(r) = state.check_auth(&headers) {
        return r.into_response();
    }
    let mut resp = plain(
        StatusCode::METHOD_NOT_ALLOWED,
        "mneme does not offer a server-to-client SSE stream",
    );
    resp.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    resp
}

async fn delete_mcp(State(state): State<Arc<AppState>>, headers: HeaderMap) -> HttpResponse {
    if let Err(r) = state.check_origin(&headers) {
        return r.into_response();
    }
    if let Err(r) = state.check_auth(&headers) {
        return r.into_response();
    }
    let id = match session_id(&headers) {
        Ok(id) => id,
        Err(r) => return r.into_response(),
    };
    let mut map = state.lock();
    if map.remove(id).is_some() {
        tracing::info!(
            active = map.len(),
            "transport=http: session closed by client"
        );
        StatusCode::NO_CONTENT.into_response()
    } else {
        plain(StatusCode::NOT_FOUND, "unknown session")
    }
}

async fn health() -> &'static str {
    "ok"
}

fn session_id(headers: &HeaderMap) -> Result<&str, Reject> {
    headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or(Reject(
            StatusCode::BAD_REQUEST,
            "missing Mcp-Session-Id header; send initialize first",
        ))
}

/// 32 random bytes, URL-safe base64 — visible ASCII as the spec asks.
fn new_session_id() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn rpc(status: StatusCode, resp: &Response) -> HttpResponse {
    match serde_json::to_vec(resp) {
        Ok(body) => (
            status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            body,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "transport=http: failed to serialise response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn plain(status: StatusCode, msg: &'static str) -> HttpResponse {
    (status, msg).into_response()
}

/// Early exit from a request check: a status plus a fixed message.
/// Kept as a small value rather than a built `Response` so `Result<_,
/// Reject>` stays cheap to return (clippy::result_large_err).
struct Reject(StatusCode, &'static str);

impl IntoResponse for Reject {
    fn into_response(self) -> HttpResponse {
        let mut resp = plain(self.0, self.1);
        if self.0 == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"mneme\""),
            );
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::resources::ResourceRegistry;
    use crate::mcp::tools::ToolRegistry;
    use crate::storage::Storage;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use serde_json::json;
    use tower::ServiceExt;

    const TOKEN: &str = "test-token-value";

    struct Fixture {
        app: Router,
        _tmp: tempfile::TempDir,
    }

    fn options(token: TokenSource) -> HttpOptions {
        HttpOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token,
            allowed_origins: vec!["http://allowed.example".into()],
            max_sessions: 4,
            session_idle_ttl: DEFAULT_SESSION_IDLE_TTL,
        }
    }

    fn fixture_with(opts: HttpOptions) -> Fixture {
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
        let tools = Arc::new(ToolRegistry::defaults(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            Arc::clone(&storage),
            cold.clone(),
            1,
        ));
        let resources = Arc::new(ResourceRegistry::defaults(
            semantic,
            procedural,
            episodic,
            orchestrator,
            cold,
            1,
            TokenBudget::for_tests(2000),
        ));
        let factory: SessionFactory = Arc::new(move || {
            McpSession::new(
                Arc::clone(&tools),
                Arc::clone(&resources),
                Arc::clone(&storage),
            )
        });
        Fixture {
            app: router(&opts, factory),
            _tmp: tmp,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(options(TokenSource::Static(TOKEN.into())))
    }

    fn post(body: &Value, session: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method(Method::POST)
            .uri(MCP_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
        if let Some(s) = session {
            b = b.header(SESSION_HEADER, s);
        }
        b.body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap()
    }

    fn initialize() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        })
    }

    async fn send(app: &Router, req: Request<Body>) -> HttpResponse {
        app.clone().oneshot(req).await.unwrap()
    }

    async fn body_json(resp: HttpResponse) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// initialize + notifications/initialized; returns the session id.
    async fn open_session(app: &Router) -> String {
        let resp = send(app, post(&initialize(), None)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let id = resp
            .headers()
            .get(SESSION_HEADER)
            .expect("initialize must return Mcp-Session-Id")
            .to_str()
            .unwrap()
            .to_owned();
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let resp = send(app, post(&note, Some(&id))).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        id
    }

    #[tokio::test]
    async fn health_needs_no_auth() {
        let f = fixture();
        let req = Request::get(HEALTH_PATH).body(Body::empty()).unwrap();
        let resp = send(&f.app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_or_wrong_token_is_unauthorized() {
        let f = fixture();
        let mut req = post(&initialize(), None);
        req.headers_mut().remove(header::AUTHORIZATION);
        let resp = send(&f.app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key(header::WWW_AUTHENTICATE));

        let mut req = post(&initialize(), None);
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer nope"),
        );
        let resp = send(&f.app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_scheme_is_case_insensitive() {
        let f = fixture();
        let mut req = post(&initialize(), None);
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("bearer {TOKEN}")).unwrap(),
        );
        assert_eq!(send(&f.app, req).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn disallowed_origin_is_forbidden_and_allowed_origin_passes() {
        let f = fixture();
        let mut req = post(&initialize(), None);
        req.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert_eq!(send(&f.app, req).await.status(), StatusCode::FORBIDDEN);

        let mut req = post(&initialize(), None);
        req.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("http://allowed.example"),
        );
        assert_eq!(send(&f.app, req).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn initialize_then_tools_list_round_trips() {
        let f = fixture();
        let id = open_session(&f.app).await;
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let resp = send(&f.app, post(&list, Some(&id))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let v = body_json(resp).await;
        assert_eq!(v["id"], 2);
        let tools = v["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "remember"));
    }

    #[tokio::test]
    async fn initialize_reports_the_protocol_version() {
        let f = fixture();
        let resp = send(&f.app, post(&initialize(), None)).await;
        let v = body_json(resp).await;
        assert_eq!(v["result"]["protocolVersion"], crate::mcp::PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "mneme");
    }

    #[tokio::test]
    async fn sessions_are_isolated() {
        let f = fixture();
        let a = open_session(&f.app).await;
        // A second session that never sent `initialized` must not
        // inherit the first one's latch.
        let resp = send(&f.app, post(&initialize(), None)).await;
        let b = resp.headers()[SESSION_HEADER].to_str().unwrap().to_owned();
        assert_ne!(a, b);
        let list = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"});
        let v = body_json(send(&f.app, post(&list, Some(&b))).await).await;
        assert_eq!(v["error"]["code"], -32002, "uninitialized session: {v}");
        let v = body_json(send(&f.app, post(&list, Some(&a))).await).await;
        assert!(v["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn missing_session_header_is_bad_request() {
        let f = fixture();
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let resp = send(&f.app, post(&list, None)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_session_is_not_found() {
        let f = fixture();
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let resp = send(&f.app, post(&list, Some("does-not-exist"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_ends_the_session() {
        let f = fixture();
        let id = open_session(&f.app).await;
        let del = || {
            Request::builder()
                .method(Method::DELETE)
                .uri(MCP_PATH)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(SESSION_HEADER, &id)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(send(&f.app, del()).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(send(&f.app, del()).await.status(), StatusCode::NOT_FOUND);
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let resp = send(&f.app, post(&list, Some(&id))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_is_method_not_allowed() {
        let f = fixture();
        let req = Request::get(MCP_PATH)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let resp = send(&f.app, req).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(resp.headers()[header::ALLOW], "POST, DELETE");
    }

    #[tokio::test]
    async fn batches_and_bad_json_are_rejected() {
        let f = fixture();
        let batch = json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]);
        let resp = send(&f.app, post(&batch, None)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"]["code"], -32600);

        let mut req = post(&json!({}), None);
        *req.body_mut() = Body::from("{not json");
        let resp = send(&f.app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn session_table_is_capped() {
        let f = fixture();
        for _ in 0..4 {
            let resp = send(&f.app, post(&initialize(), None)).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let resp = send(&f.app, post(&initialize(), None)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn idle_sessions_expire() {
        let mut opts = options(TokenSource::Static(TOKEN.into()));
        opts.session_idle_ttl = Duration::from_millis(1);
        let f = fixture_with(opts);
        let resp = send(&f.app, post(&initialize(), None)).await;
        let id = resp.headers()[SESSION_HEADER].to_str().unwrap().to_owned();
        std::thread::sleep(Duration::from_millis(5));
        let ping = json!({"jsonrpc": "2.0", "id": 2, "method": "ping"});
        let resp = send(&f.app, post(&ping, Some(&id))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn file_token_is_reread_per_request() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
        let f = fixture_with(options(TokenSource::File(path.clone())));
        // Trailing newline in the file is ignored.
        assert_eq!(
            send(&f.app, post(&initialize(), None)).await.status(),
            StatusCode::OK
        );
        std::fs::write(&path, "rotated").unwrap();
        assert_eq!(
            send(&f.app, post(&initialize(), None)).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn empty_token_refuses_everything() {
        let f = fixture_with(options(TokenSource::Static("  ".into())));
        let mut req = post(&initialize(), None);
        req.headers_mut()
            .insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        let resp = send(&f.app, req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn serve_binds_and_shuts_down() {
        // Exercise `serve` end-to-end over a real socket. No
        // `initialize` is sent, so the factory is never called.
        let factory: SessionFactory = Arc::new(|| unreachable!("no session is opened"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut opts = options(TokenSource::Static(TOKEN.into()));
        opts.bind = addr;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(serve(opts, factory, async {
            let _ = rx.await;
        }));
        // Poll /healthz over raw TCP until the listener is up.
        let mut ok = false;
        for _ in 0..50 {
            if let Ok(mut s) = tokio::net::TcpStream::connect(addr).await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                let mut buf = String::new();
                s.read_to_string(&mut buf).await.unwrap();
                ok = buf.starts_with("HTTP/1.1 200");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "healthz did not answer 200");
        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }
}
