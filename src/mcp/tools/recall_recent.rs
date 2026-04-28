//! `recall_recent` — Phase 4. Top-N most-recent episodic events,
//! optionally filtered by scope or kind. Distinct from `recall`
//! (semantic similarity); this surface is for "what just happened?"
//! questions.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::episodic::{EpisodicStore, RecentFilters};

const DESCRIPTION: &str = "Retrieve the most recent episodic events \
(tool calls, user messages, checkpoints) from this and earlier \
sessions. Use when the user asks 'what did we just do?' or you need \
to remind yourself of the immediate working context. Distinct from \
`recall`, which is for semantic match.";

const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 200;

pub struct RecallRecent {
    store: Arc<EpisodicStore>,
}

impl RecallRecent {
    pub fn new(store: Arc<EpisodicStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RecallRecent {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recall_recent",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Max events to return. Defaults to 20."
                    },
                    "scope": { "type": "string", "description": "Optional scope filter." },
                    "kind": { "type": "string", "description": "Optional event-kind filter." }
                }
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let limit = match args.get("limit") {
            None => DEFAULT_LIMIT,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                ToolError::InvalidArguments("`limit` must be a positive integer".into())
            })?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`limit` must be a positive integer".into(),
                ));
            }
        };
        if limit == 0 || limit > MAX_LIMIT {
            return Err(ToolError::InvalidArguments(format!(
                "`limit` must be between 1 and {MAX_LIMIT}"
            )));
        }

        let filters = RecentFilters {
            scope: args.get("scope").and_then(Value::as_str).map(String::from),
            kind: args.get("kind").and_then(Value::as_str).map(String::from),
        };
        let events = self
            .store
            .recall_recent(&filters, limit as usize)
            .await
            .map_err(|e| ToolError::Internal(format!("recall_recent failed: {e}")))?;

        let body: Vec<Value> = events
            .iter()
            .map(|e| {
                json!({
                    "id": e.id.to_string(),
                    "kind": e.kind,
                    "scope": e.scope,
                    "payload": e.payload,
                    "retrieval_weight": e.retrieval_weight,
                    "last_accessed": e.last_accessed.to_rfc3339(),
                    "created_at": e.created_at.to_rfc3339(),
                })
            })
            .collect();
        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise events: {e}")))?;
        Ok(ToolResult::text(text))
    }
}
