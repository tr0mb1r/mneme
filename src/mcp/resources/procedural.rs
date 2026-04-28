//! `mneme://procedural` — current procedural pinned-list as JSON.
//! Hosts read this to surface the always-on context block; the
//! source of truth is `<root>/procedural/pinned.jsonl`, which is
//! human-editable.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::memory::procedural::ProceduralStore;

pub struct Procedural {
    store: Arc<ProceduralStore>,
}

impl Procedural {
    pub fn new(store: Arc<ProceduralStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Resource for Procedural {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: "mneme://procedural",
            name: "procedural",
            description: "Always-on pinned items: preferences, identity facts, binding decisions.",
            mime_type: "application/json",
        }
    }

    async fn read(&self) -> Result<ResourceContent, ResourceError> {
        let items = self
            .store
            .list(None)
            .map_err(|e| ResourceError::Internal(format!("procedural list: {e}")))?;
        let body: Vec<_> = items
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.id.to_string(),
                    "content": p.content,
                    "tags": p.tags,
                    "scope": p.scope,
                    "created_at": p.created_at.to_rfc3339(),
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
