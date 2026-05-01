//! Tool registry. A `Tool` is a verb the agent can call.
//!
//! Phase 1 ships three stubs (`remember`, `recall`, `forget`) that don't
//! yet touch storage — they exercise the MCP protocol surface so we can
//! prove conformance before wiring the persistence layers in Phase 2/3.
//!
//! Tool descriptions are deliberately written from the agent's point of
//! view (spec §6.1: "the LLM reads the description to decide when to
//! invoke"). Phase 1 already commits to the production wording.

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

/// A tool's machine-readable signature. Mirrors the MCP `tools/list`
/// entry: name, human description, and a JSON Schema for `arguments`.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// A successful tool invocation produces one or more `ContentBlock`s.
/// MCP supports text, image, and resource-link blocks; v0.1 only
/// emits text.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
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
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "content": self.content.iter().map(ContentBlock::to_json).collect::<Vec<_>>(),
            "isError": self.is_error,
        })
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
    ) -> Self {
        let mut r = Self::new();
        // L4 — semantic memory.
        r.register(Arc::new(remember::Remember::new(
            Arc::clone(&semantic),
            Arc::clone(&scope_state),
        )));
        r.register(Arc::new(recall::Recall::new(Arc::clone(&semantic))));
        r.register(Arc::new(forget::Forget::new(Arc::clone(&semantic))));
        r.register(Arc::new(update::Update::new(Arc::clone(&semantic))));
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
            record_event::RecordEvent::new(Arc::clone(&episodic), Arc::clone(&scope_state));
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
        "description": d.description,
        "inputSchema": d.input_schema,
    })
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
}
