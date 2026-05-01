//! `pin` — Phase 4. Append an entry to the procedural pinned-list at
//! `<root>/procedural/pinned.jsonl`. Pinned items appear in
//! `mneme://procedural` and bypass recency-based ranking; the agent
//! treats them as an always-on prefix to its context.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::procedural::ProceduralStore;
use crate::scope::ScopeState;

const DESCRIPTION: &str = "Promote a piece of information to procedural \
memory. Pinned items appear at the top of every recall context until \
explicitly unpinned. Use sparingly: this is the right place for \
hard preferences ('use Rust over Python'), persistent identity \
facts, and binding decisions; not for transient state.";

pub struct Pin {
    store: Arc<ProceduralStore>,
    scope_state: Arc<ScopeState>,
}

impl Pin {
    pub fn new(store: Arc<ProceduralStore>, scope_state: Arc<ScopeState>) -> Self {
        Self { store, scope_state }
    }
}

#[async_trait]
impl Tool for Pin {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "pin",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Text to pin." },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags."
                    },
                    "scope": { "type": "string", "description": "Optional scope override. Defaults to the session's current scope (set by `switch_scope`)." }
                },
                "required": ["content"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`content` is required".into()))?;
        if content.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`content` must not be empty".into(),
            ));
        }

        let tags = super::parse_tags_arg(args.get("tags"))?;

        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .map(|s| s.to_owned())
            .unwrap_or_else(|| self.scope_state.current());

        let id = self
            .store
            .pin(content.trim().to_owned(), tags, scope)
            .await
            .map_err(|e| ToolError::Internal(format!("pin failed: {e}")))?;
        Ok(ToolResult::text(format!("pinned {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_pin() -> (Pin, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let scope = ScopeState::new("personal");
        (Pin::new(store, scope), tmp)
    }

    /// Workaround for MCP clients that double-encode array tool args
    /// before forwarding the `tools/call` frame (observed: certain
    /// Claude Code releases). The shared `parse_tags_arg` helper
    /// accepts both real arrays and JSON-encoded array strings; this
    /// test pins the `pin` call site to use that helper rather than
    /// bypassing it.
    #[tokio::test]
    async fn harness_double_encoded_tags_are_accepted() {
        let (p, _tmp) = fresh_pin();
        let res = p
            .invoke(json!({
                "content": "always run cargo fmt before commit",
                "tags": "[\"binding\",\"workflow\"]"
            }))
            .await
            .unwrap();
        assert!(!res.is_error);
    }

    #[tokio::test]
    async fn array_of_strings_still_works() {
        let (p, _tmp) = fresh_pin();
        p.invoke(json!({
            "content": "real array path still works",
            "tags": ["binding", "workflow"]
        }))
        .await
        .unwrap();
    }
}
