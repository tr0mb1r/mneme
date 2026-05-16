//! `summarize_session` — Phase 4. Returns a prompt template the host
//! LLM can fill in with a session summary. Mneme deliberately does
//! NOT call an LLM itself (per spec §3 #5: "no model dependencies in
//! the binary"); we just stage the events and the framing prompt.
//!
//! The agent receives the prompt as the tool's text result, runs it
//! through whatever LLM the host already has wired up, and (in the
//! future) calls `remember` with the resulting summary so it lands
//! back in semantic memory.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::episodic::{EpisodicStore, RecentFilters};

const DESCRIPTION: &str = "Build a prompt template the host LLM can \
use to summarize the most recent session activity. Returns text the \
agent should pass to its own LLM completion path; mneme does not \
call any model itself. Useful for end-of-turn condensation: feed the \
result to your model, then `remember` the summary it produces.";

/// Default number of recent events folded into the summary prompt.
/// Picked to fit comfortably in a 4K-token context with room for
/// the framing.
const DEFAULT_EVENTS: u64 = 30;
const MAX_EVENTS: u64 = 200;

pub struct SummarizeSession {
    store: Arc<EpisodicStore>,
}

impl SummarizeSession {
    pub fn new(store: Arc<EpisodicStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SummarizeSession {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "summarize_session",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "events": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_EVENTS,
                        "description": "How many recent events to include. Defaults to 30."
                    },
                    "scope": { "type": "string", "description": "Optional scope filter." }
                }
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let events_n = match args.get("events") {
            None => DEFAULT_EVENTS,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                ToolError::InvalidArguments("`events` must be a positive integer".into())
            })?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`events` must be a positive integer".into(),
                ));
            }
        };
        if events_n == 0 || events_n > MAX_EVENTS {
            return Err(ToolError::InvalidArguments(format!(
                "`events` must be between 1 and {MAX_EVENTS}"
            )));
        }

        let filters = RecentFilters {
            scope: args.get("scope").and_then(Value::as_str).map(String::from),
            kind: None,
        };
        let events = self
            .store
            .recall_recent(&filters, events_n as usize)
            .await
            .map_err(|e| ToolError::Internal(format!("recall_recent failed: {e}")))?;

        let mut prompt = String::with_capacity(events.len() * 96 + 256);
        prompt.push_str(
            "You are summarizing a session. Below is a list of recent events in \
             reverse-chronological order (newest first). Produce a concise summary \
             (≤5 bullets) that captures: the user's goal, key decisions, \
             outstanding actions, and any unresolved questions. Avoid restating \
             trivia.\n\n",
        );
        if events.is_empty() {
            prompt.push_str("(no recent events)\n");
        } else {
            prompt.push_str("EVENTS:\n");
            for e in &events {
                prompt.push_str(&format!(
                    "- [{}] {} ({}): {}\n",
                    e.created_at.to_rfc3339(),
                    e.kind,
                    e.scope,
                    e.payload
                ));
            }
        }
        prompt.push_str("\nRespond with the summary only. Do not include the events verbatim.");

        Ok(ToolResult::text(prompt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::ContentBlock;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn store() -> (Arc<EpisodicStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let store = Arc::new(EpisodicStore::new(backing));
        (store, tmp)
    }

    fn text(res: ToolResult) -> String {
        match &res.content[0] {
            ContentBlock::Text(t) => t.clone(),
        }
    }

    #[tokio::test]
    async fn default_events_count_is_30() {
        let (s, _tmp) = store();
        let tool = SummarizeSession::new(s);
        let res = tool.invoke(json!({})).await.unwrap();
        let prompt = text(res);
        assert!(prompt.starts_with("You are summarizing a session."));
    }

    #[tokio::test]
    async fn custom_events_count_is_accepted() {
        let (s, _tmp) = store();
        let tool = SummarizeSession::new(s);
        let res = tool.invoke(json!({"events": 5})).await.unwrap();
        let prompt = text(res);
        assert!(prompt.starts_with("You are summarizing a session."));
    }

    #[tokio::test]
    async fn scope_filter_propagates() {
        let (s, _tmp) = store();
        s.record("k1", "work", "\"a\"").await.unwrap();
        s.record("k2", "personal", "\"b\"").await.unwrap();
        let tool = SummarizeSession::new(s);
        let res = tool.invoke(json!({"scope": "work"})).await.unwrap();
        let prompt = text(res);
        assert!(prompt.contains("k1"));
        assert!(!prompt.contains("k2"));
    }

    #[tokio::test]
    async fn empty_store_returns_no_recent_events() {
        let (s, _tmp) = store();
        let tool = SummarizeSession::new(s);
        let res = tool.invoke(json!({})).await.unwrap();
        let prompt = text(res);
        assert!(prompt.contains("(no recent events)"));
    }

    #[tokio::test]
    async fn events_zero_returns_invalid_arguments() {
        let (s, _tmp) = store();
        let tool = SummarizeSession::new(s);
        let err = tool.invoke(json!({"events": 0})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn events_over_max_returns_invalid_arguments() {
        let (s, _tmp) = store();
        let tool = SummarizeSession::new(s);
        let err = tool.invoke(json!({"events": 201})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invalid_events_type_returns_invalid_arguments() {
        let (s, _tmp) = store();
        let tool = SummarizeSession::new(s);
        let err = tool
            .invoke(json!({"events": "not-a-number"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
