//! `pin` — Phase 4. Append an entry to the procedural pinned-list at
//! `<root>/procedural/pinned.jsonl`. Pinned items appear in
//! `mneme://procedural` and bypass recency-based ranking; the agent
//! treats them as an always-on prefix to its context.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::secret_guard::{self, Guard};
use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::config::SecretsMode;
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
    /// `[guardrails] secrets`.
    secrets: SecretsMode,
}

impl Pin {
    pub fn new(store: Arc<ProceduralStore>, scope_state: Arc<ScopeState>) -> Self {
        Self {
            store,
            scope_state,
            secrets: SecretsMode::default(),
        }
    }

    /// Set `[guardrails] secrets`. Defaults to `reject`, so fixtures
    /// that never call this still get the guard.
    pub fn with_secret_guard(mut self, mode: SecretsMode) -> Self {
        self.secrets = mode;
        self
    }
}

#[async_trait]
impl Tool for Pin {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "pin",
            title: "Pin a rule",
            annotations: ToolAnnotations::additive(),
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
        let secret_advisory = match secret_guard::guard_text(self.secrets, content)? {
            Guard::Reject(r) => return Ok(r),
            g => g.into_advisory(),
        };

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
        Ok(secret_guard::attach(
            ToolResult::text(format!("pinned {id}")),
            secret_advisory,
            id,
        ))
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

    /// A credential-shaped string. Assembled at runtime so this file
    /// does not itself trip repository secret scanners.
    fn aws_key() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }

    fn pin_with_mode(mode: SecretsMode) -> (Pin, Arc<ProceduralStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let pin = Pin::new(Arc::clone(&store), ScopeState::new("personal")).with_secret_guard(mode);
        (pin, store, tmp)
    }

    #[tokio::test]
    async fn secret_in_reject_mode_pins_nothing() {
        let (p, store, _tmp) = pin_with_mode(SecretsMode::Reject);
        let key = aws_key();
        let res = p
            .invoke(json!({ "content": format!("always use key {key}") }))
            .await
            .unwrap();
        assert!(res.is_error);
        let meta = res.meta.as_ref().expect("error meta");
        assert_eq!(meta["error"]["code"], "secret_detected");
        assert!(!format!("{:?}{}", res.content, meta).contains(&key));
        assert!(store.list(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn secret_in_warn_mode_is_pinned_with_advisory() {
        let (p, store, _tmp) = pin_with_mode(SecretsMode::Warn);
        let res = p
            .invoke(json!({ "content": format!("always use key {}", aws_key()) }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let meta = res.meta.expect("expected a secret advisory");
        assert_eq!(meta["secret_advisory"]["pattern"], "aws_access_key_id");
        let pinned = store.list(None).unwrap();
        assert_eq!(pinned.len(), 1);
        let crate::mcp::tools::ContentBlock::Text(text) = &res.content[0];
        assert!(
            text.contains(&format!(
                "warning: content matched credential pattern aws_access_key_id at char 15; \
                 forget {} if this is a real secret",
                pinned[0].id
            )),
            "{text}"
        );
        assert!(!text.contains(&aws_key()));
    }

    #[tokio::test]
    async fn secret_in_off_mode_is_pinned_without_advisory() {
        let (p, store, _tmp) = pin_with_mode(SecretsMode::Off);
        let res = p
            .invoke(json!({ "content": format!("always use key {}", aws_key()) }))
            .await
            .unwrap();
        assert!(!res.is_error);
        let has_adv = res
            .meta
            .as_ref()
            .map(|m| m.get("secret_advisory").is_some())
            .unwrap_or(false);
        assert!(!has_adv, "off mode must not scan: {:?}", res.meta);
        assert_eq!(store.list(None).unwrap().len(), 1);
    }
}
