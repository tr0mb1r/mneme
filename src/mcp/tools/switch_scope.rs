//! `switch_scope` — Phase 6 §6 v0.15. Set the active default scope
//! for the rest of the `mneme run` lifetime.
//!
//! Mneme tools all accept an optional `scope` argument. Without
//! `switch_scope` the default is hardcoded to `[scopes] default`
//! (typically `"personal"`); after a successful `switch_scope`
//! call, write tools (`remember`, `pin`) without an explicit `scope`
//! land in the new scope. Filter tools (`recall`, `recall_recent`,
//! `forget`, etc.) are unaffected — they still default to "match
//! every scope" when the arg is omitted; pass `scope` explicitly to
//! filter.
//!
//! Process-lifetime only: the next `mneme run` resets to the config
//! default. See `crate::scope::ScopeState` for the rationale.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::scope::ScopeState;

const DESCRIPTION: &str = "Set the session's default scope. After this \
call, write tools (`remember`, `pin`) that omit the `scope` argument \
land in the new scope; filter tools (`recall`, `recall_recent`, \
`forget`, ...) are unaffected — they still match every scope when \
`scope` is omitted, so pass `scope` explicitly to filter. The change \
lasts until the server restarts.";

pub struct SwitchScope {
    scope_state: Arc<ScopeState>,
}

impl SwitchScope {
    pub fn new(scope_state: Arc<ScopeState>) -> Self {
        Self { scope_state }
    }
}

#[async_trait]
impl Tool for SwitchScope {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "switch_scope",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "description": "New default scope (e.g. \"work\", \"personal\", \"client-x\")."
                    }
                },
                "required": ["scope"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`scope` is required".into()))?;

        let previous = self.scope_state.current();
        self.scope_state
            .set(scope)
            .map_err(|e| ToolError::InvalidArguments(e.to_owned()))?;
        let new = self.scope_state.current();

        Ok(ToolResult::text(format!(
            "scope changed: {previous} -> {new}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn changes_scope_and_returns_confirmation() {
        let state = ScopeState::new("personal");
        let t = SwitchScope::new(Arc::clone(&state));
        let res = t.invoke(json!({ "scope": "work" })).await.unwrap();
        let crate::mcp::tools::ContentBlock::Text(text) = &res.content[0];
        assert!(text.contains("personal -> work"));
        assert_eq!(state.current(), "work");
    }

    #[tokio::test]
    async fn missing_scope_is_invalid() {
        let state = ScopeState::new("personal");
        let t = SwitchScope::new(state);
        let err = t.invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn empty_scope_is_invalid() {
        let state = ScopeState::new("personal");
        let t = SwitchScope::new(Arc::clone(&state));
        let err = t.invoke(json!({ "scope": "   " })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        // State unchanged on rejection.
        assert_eq!(state.current(), "personal");
    }

    #[tokio::test]
    async fn whitespace_in_scope_is_trimmed() {
        let state = ScopeState::new("personal");
        let t = SwitchScope::new(Arc::clone(&state));
        t.invoke(json!({ "scope": "  client-x  " })).await.unwrap();
        assert_eq!(state.current(), "client-x");
    }
}
