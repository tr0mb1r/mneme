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

use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;

pub mod forget;
pub mod pin;
pub mod recall;
pub mod recall_recent;
pub mod remember;
pub mod summarize_session;
pub mod unpin;

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
    /// wired up at once.
    pub fn defaults(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
    ) -> Self {
        let mut r = Self::new();
        // L4 — semantic memory.
        r.register(Arc::new(remember::Remember::new(Arc::clone(&semantic))));
        r.register(Arc::new(recall::Recall::new(Arc::clone(&semantic))));
        r.register(Arc::new(forget::Forget::new(semantic)));
        // L0 — procedural memory.
        r.register(Arc::new(pin::Pin::new(Arc::clone(&procedural))));
        r.register(Arc::new(unpin::Unpin::new(procedural)));
        // L3 — episodic memory.
        r.register(Arc::new(recall_recent::RecallRecent::new(Arc::clone(
            &episodic,
        ))));
        r.register(Arc::new(summarize_session::SummarizeSession::new(episodic)));
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
        let episodic = Arc::new(EpisodicStore::new(backing));
        (ToolRegistry::defaults(semantic, procedural, episodic), tmp)
    }

    #[test]
    fn defaults_register_phase_4_tools() {
        let (r, _tmp) = fresh_registry();
        let names: Vec<_> = r.list().iter().map(|d| d.name).collect();
        // BTreeMap ordering: forget, pin, recall, recall_recent,
        // remember, summarize_session, unpin.
        assert_eq!(
            names,
            vec![
                "forget",
                "pin",
                "recall",
                "recall_recent",
                "remember",
                "summarize_session",
                "unpin",
            ]
        );
    }

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let (r, _tmp) = fresh_registry();
        assert!(r.get("nope").is_none());
    }
}
