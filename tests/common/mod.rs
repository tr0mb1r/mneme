//! Shared fixtures for integration tests: the synthetic corpus and an
//! in-process harness that wires the stores, registries and MCP
//! sessions the way `mneme daemon` / `mneme serve` do.
//!
//! In-process on purpose. The structural checks run on every PR; they
//! should not depend on spawning the binary, on free ports, or on which
//! worktree last linked `target/debug/mneme`.

#![allow(dead_code)]

pub mod corpus;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tempfile::TempDir;

use mneme::config::{BudgetsConfig, Config, SecretsMode};
use mneme::embed::Embedder;
use mneme::embed::stub::StubEmbedder;
use mneme::mcp::dispatch::DispatchPolicy;
use mneme::mcp::resources::ResourceRegistry;
use mneme::mcp::server::McpSession;
use mneme::mcp::tools::ToolRegistry;
use mneme::mcp::transport::http::SessionFactory;
use mneme::memory::checkpoint_scheduler::{CheckpointSchedulerConfig, WorkingSessionFactory};
use mneme::memory::episodic::EpisodicStore;
use mneme::memory::procedural::ProceduralStore;
use mneme::memory::semantic::{SemanticStore, SnapshotConfig};
use mneme::memory::working::ActiveSession;
use mneme::orchestrator::{Orchestrator, TokenBudget};
use mneme::scope::ScopeState;
use mneme::storage::Storage;
use mneme::storage::archive::ColdArchive;
use mneme::storage::memory_impl::MemoryStorage;

/// The scope a fresh session starts in, as `[scopes] default`.
pub const DEFAULT_SCOPE: &str = "global";

/// One mneme "server": the shared stores, without a transport.
pub struct Harness {
    pub tmp: TempDir,
    pub storage: Arc<dyn Storage>,
    pub semantic: Arc<SemanticStore>,
    pub procedural: Arc<ProceduralStore>,
    pub episodic: Arc<EpisodicStore>,
    pub cold: ColdArchive,
    pub config: Config,
    orchestrator: Arc<Orchestrator>,
    working: WorkingSessionFactory,
}

impl Harness {
    /// Empty stores over in-memory storage and the stub embedder, with
    /// the default configuration.
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new());
        let semantic = SemanticStore::open(
            tmp.path(),
            Arc::clone(&storage),
            embedder,
            SnapshotConfig::disabled(),
        )
        .expect("open semantic store");
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).expect("open procedural"));
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
        let orchestrator = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        ));
        let working = WorkingSessionFactory::new(
            tmp.path().join("sessions"),
            None,
            CheckpointSchedulerConfig::from_config(&config.checkpoints),
        );
        Self {
            cold: ColdArchive::new(tmp.path()),
            tmp,
            storage,
            semantic,
            procedural,
            episodic,
            config,
            orchestrator,
            working,
        }
    }

    /// Seed the synthetic corpus into this harness's stores.
    pub async fn seed(&self) -> corpus::Corpus {
        corpus::seed(&self.semantic, &self.procedural, &self.episodic)
            .await
            .expect("seed corpus")
    }

    fn budget(&self) -> TokenBudget {
        TokenBudget::from_config(&BudgetsConfig {
            ..self.config.budgets.clone()
        })
    }

    /// A fully wired MCP session plus the working session it owns:
    /// what the daemon builds per connection and the HTTP listener per
    /// `Mcp-Session-Id`.
    fn build_session(&self) -> (McpSession, Arc<ActiveSession>) {
        let (working, scheduler) = self.working.open().expect("open working session");
        let scope = ScopeState::new(DEFAULT_SCOPE);
        let tools = Arc::new(ToolRegistry::defaults_with_schedulers(
            Arc::clone(&self.semantic),
            Arc::clone(&self.procedural),
            Arc::clone(&self.episodic),
            Arc::clone(&self.storage),
            self.cold.clone(),
            1,
            None,
            Some(Arc::clone(&scheduler)),
            Arc::clone(&scope),
            Some(Arc::clone(&working)),
            self.config.budgets.max_remember_chars,
            SecretsMode::default(),
        ));
        let orchestrator = Arc::new(
            (*self.orchestrator)
                .clone()
                .with_active_session(Arc::clone(&working)),
        );
        let resources = Arc::new(ResourceRegistry::defaults_with_schedulers(
            Arc::clone(&self.semantic),
            Arc::clone(&self.procedural),
            Arc::clone(&self.episodic),
            orchestrator,
            self.cold.clone(),
            1,
            self.budget(),
            None,
            Some(Arc::clone(&scheduler)),
            Some(Arc::clone(&working)),
            Some(self.tmp.path().join("sessions")),
            Some(Arc::clone(&scope)),
            Some((
                Arc::clone(&self.storage),
                self.config.budgets.max_remember_chars,
            )),
        ));
        let session = McpSession::new(tools, resources, Arc::clone(&self.storage))
            .with_session(
                Arc::clone(&working),
                scheduler,
                Arc::clone(&self.episodic),
                scope,
            )
            .with_dispatch_policy(DispatchPolicy::from_config(&self.config))
            .with_owned_working_session(true);
        (session, working)
    }

    /// Connect a client: a new MCP session, already initialized.
    pub async fn connect(&self) -> Client {
        let (session, working) = self.build_session();
        let client = Client {
            session,
            working,
            next_id: AtomicU64::new(1),
        };
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "structural", "version": "0"}
                }),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "mneme", "{init}");
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(
            client
                .session
                .handle_frame(note.to_string().as_bytes())
                .await
                .is_none()
        );
        client
    }

    /// The session factory the HTTP transport takes, backed by this
    /// harness's stores.
    pub fn http_session_factory(self: &Arc<Self>) -> SessionFactory {
        let harness = Arc::clone(self);
        Arc::new(move || harness.build_session().0)
    }

    /// Kinds of every event in the hot and warm tiers.
    pub async fn event_kinds(&self) -> Vec<String> {
        let mut events = self.episodic.list_all().await.expect("list hot");
        events.extend(self.episodic.list_warm().await.expect("list warm"));
        events.into_iter().map(|e| e.kind).collect()
    }
}

/// One MCP client talking to a [`Harness`] session, frame by frame.
pub struct Client {
    session: McpSession,
    /// This client's L1 working session.
    pub working: Arc<ActiveSession>,
    next_id: AtomicU64,
}

impl Client {
    /// Send one JSON-RPC request; returns the whole response.
    pub async fn request(&self, method: &str, params: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self
            .session
            .handle_frame(frame.to_string().as_bytes())
            .await
            .expect("a request gets a response");
        let v = serde_json::to_value(response).expect("response serialises");
        assert_eq!(v["id"], id, "{v}");
        v
    }

    /// `tools/call`; returns the whole response, error or not.
    pub async fn call(&self, tool: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": args}))
            .await
    }

    /// `tools/call` that must succeed; returns the `result` object.
    pub async fn ok(&self, tool: &str, args: Value) -> Value {
        let v = self.call(tool, args.clone()).await;
        assert!(v.get("error").is_none(), "{tool}({args}) failed: {v}");
        assert_eq!(v["result"]["isError"], false, "{tool}({args}): {v}");
        v["result"].clone()
    }

    /// The reply text of a successful `tools/call`.
    pub async fn text(&self, tool: &str, args: Value) -> String {
        self.ok(tool, args).await["content"][0]["text"]
            .as_str()
            .expect("text block")
            .to_owned()
    }

    /// A successful `tools/call` whose reply text is JSON.
    pub async fn json(&self, tool: &str, args: Value) -> Value {
        let text = self.text(tool, args).await;
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{tool} reply is not JSON ({e}): {text}"))
    }

    /// `resources/read`; returns the resource body as text.
    pub async fn read_text(&self, uri: &str) -> String {
        let v = self.request("resources/read", json!({"uri": uri})).await;
        assert!(v.get("error").is_none(), "read {uri} failed: {v}");
        v["result"]["contents"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("read {uri}: no text in {v}"))
            .to_owned()
    }

    /// `resources/read` of a JSON resource.
    pub async fn read(&self, uri: &str) -> Value {
        let text = self.read_text(uri).await;
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{uri} is not JSON ({e}): {text}"))
    }

    /// `working.turns_total` as `stats` reports it for this session.
    pub async fn turns_total(&self) -> u64 {
        self.json("stats", json!({})).await["working"]["turns_total"]
            .as_u64()
            .expect("working.turns_total")
    }

    /// End the session the way a disconnect does.
    pub async fn close(self) {
        self.session.close().await;
    }
}

/// `chars / 4`, the token estimate the orchestrator budgets with.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Panics if `haystack` holds any 20-character run of `content`.
pub fn assert_no_trace(haystack: &str, content: &str) {
    let chars: Vec<char> = content.chars().collect();
    assert!(chars.len() >= 20, "content too short to window");
    for window in chars.windows(20) {
        let needle: String = window.iter().collect();
        assert!(
            !haystack.contains(&needle),
            "`{needle}` survives in {haystack}"
        );
    }
}
