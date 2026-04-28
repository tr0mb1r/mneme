//! `mneme://context` — Phase 5. The auto-context the agent reads on
//! every turn: pinned procedural anchors + recent episodic events,
//! packed inside the configured token budget. No semantic layer is
//! folded in: a static resource has no query, so L4 stays empty by
//! design. Tools that need query-driven context call `recall`
//! directly.
//!
//! The output is JSON with three sections + a `total_tokens` count;
//! the agent's host renders it however it likes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::orchestrator::{Orchestrator, TokenBudget};

pub struct Context {
    orchestrator: Arc<Orchestrator>,
    budget: TokenBudget,
}

impl Context {
    pub fn new(orchestrator: Arc<Orchestrator>, budget: TokenBudget) -> Self {
        Self {
            orchestrator,
            budget,
        }
    }
}

#[async_trait]
impl Resource for Context {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: "mneme://context",
            name: "context",
            description: "Auto-assembled context block: pinned procedural items + recent \
                          episodic events, packed inside the configured token budget.",
            mime_type: "application/json",
        }
    }

    async fn read(&self) -> Result<ResourceContent, ResourceError> {
        let ctx = self
            .orchestrator
            .build_context(None, None, self.budget)
            .await
            .map_err(|e| ResourceError::Internal(format!("build_context: {e}")))?;

        let body = json!({
            "procedural": ctx.procedural.iter().map(|p| json!({
                "id": p.id.to_string(),
                "content": p.content,
                "tags": p.tags,
                "scope": p.scope,
                "created_at": p.created_at.to_rfc3339(),
            })).collect::<Vec<_>>(),
            "episodic": ctx.episodic.iter().map(|e| json!({
                "id": e.id.to_string(),
                "kind": e.kind,
                "scope": e.scope,
                "payload": e.payload,
                "last_accessed": e.last_accessed.to_rfc3339(),
                "created_at": e.created_at.to_rfc3339(),
            })).collect::<Vec<_>>(),
            "semantic": [],
            "total_tokens": ctx.total_tokens,
            "max_tokens": self.budget.max_tokens,
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ResourceError::Internal(format!("serialise: {e}")))?;
        Ok(ResourceContent {
            uri: self.descriptor().uri.into(),
            mime_type: "application/json",
            text,
        })
    }
}
