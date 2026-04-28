//! `pin` — Phase 4. Append an entry to the procedural pinned-list at
//! `<root>/procedural/pinned.jsonl`. Pinned items appear in
//! `mneme://procedural` and bypass recency-based ranking; the agent
//! treats them as an always-on prefix to its context.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::procedural::ProceduralStore;

const DESCRIPTION: &str = "Promote a piece of information to procedural \
memory. Pinned items appear at the top of every recall context until \
explicitly unpinned. Use sparingly: this is the right place for \
hard preferences ('use Rust over Python'), persistent identity \
facts, and binding decisions; not for transient state.";

/// Default scope, mirrors `remember`.
const DEFAULT_SCOPE: &str = "personal";

pub struct Pin {
    store: Arc<ProceduralStore>,
}

impl Pin {
    pub fn new(store: Arc<ProceduralStore>) -> Self {
        Self { store }
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
                    "scope": { "type": "string", "description": "Optional scope override." }
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

        let tags: Vec<String> = match args.get("tags") {
            None => Vec::new(),
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_owned())
                        .ok_or_else(|| ToolError::InvalidArguments("`tags` must be strings".into()))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`tags` must be an array of strings".into(),
                ));
            }
        };

        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_SCOPE)
            .to_owned();

        let id = self
            .store
            .pin(content.trim().to_owned(), tags, scope)
            .await
            .map_err(|e| ToolError::Internal(format!("pin failed: {e}")))?;
        Ok(ToolResult::text(format!("pinned {id}")))
    }
}
