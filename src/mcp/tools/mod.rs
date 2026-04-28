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

use crate::memory::semantic::SemanticStore;

pub mod forget;
pub mod recall;
pub mod remember;

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

    /// Build the v0.1 default registry: remember, recall, forget — all
    /// backed by `store`. Phase 4 will add `pin`, `unpin`,
    /// `recall_recent`, etc. on top of the same surface.
    pub fn defaults(store: Arc<SemanticStore>) -> Self {
        let mut r = Self::new();
        r.register(Arc::new(remember::Remember::new(Arc::clone(&store))));
        r.register(Arc::new(recall::Recall::new(Arc::clone(&store))));
        r.register(Arc::new(forget::Forget::new(store)));
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
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    /// Bootstraps a `SemanticStore` over `MemoryStorage` + the stub
    /// embedder so tests of the registry plumbing don't need a model
    /// download. The TempDir is returned to the caller because dropping
    /// it would yank the WAL directory out from under the writer thread.
    fn fresh_registry() -> (ToolRegistry, TempDir) {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let store = SemanticStore::open_disabled(tmp.path(), storage, embedder).unwrap();
        (ToolRegistry::defaults(store), tmp)
    }

    #[test]
    fn defaults_register_three_tools() {
        let (r, _tmp) = fresh_registry();
        let names: Vec<_> = r.list().iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["forget", "recall", "remember"]); // BTreeMap order
    }

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let (r, _tmp) = fresh_registry();
        assert!(r.get("nope").is_none());
    }
}
