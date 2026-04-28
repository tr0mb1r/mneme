//! `mneme://recent` — last 20 episodic events as a JSON array.
//! The host LLM reads this to ground the agent in the immediate
//! working context. Distinct from `mneme://context` (Phase 5), which
//! will assemble a structured context blob across all memory layers.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::memory::episodic::{EpisodicStore, RecentFilters};

const RECENT_LIMIT: usize = 20;

pub struct Recent {
    store: Arc<EpisodicStore>,
}

impl Recent {
    pub fn new(store: Arc<EpisodicStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Resource for Recent {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: "mneme://recent",
            name: "recent",
            description: "The 20 most recent episodic events from this and prior sessions.",
            mime_type: "application/json",
        }
    }

    async fn read(&self) -> Result<ResourceContent, ResourceError> {
        let events = self
            .store
            .recall_recent(&RecentFilters::default(), RECENT_LIMIT)
            .await
            .map_err(|e| ResourceError::Internal(format!("recall_recent: {e}")))?;
        let body: Vec<_> = events
            .into_iter()
            .map(|e| {
                json!({
                    "id": e.id.to_string(),
                    "kind": e.kind,
                    "scope": e.scope,
                    "payload": e.payload,
                    "tags": e.tags,
                    "last_accessed": e.last_accessed.to_rfc3339(),
                    "created_at": e.created_at.to_rfc3339(),
                })
            })
            .collect();
        let text = serde_json::to_string(&body)
            .map_err(|e| ResourceError::Internal(format!("serialise: {e}")))?;
        Ok(ResourceContent {
            uri: self.descriptor().uri.into(),
            mime_type: "application/json",
            text,
        })
    }
}
