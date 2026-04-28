//! Resource registry. A `Resource` is a noun the agent can read.
//!
//! Phase 1 ships only `mneme://stats`. The full v1 surface (`mneme://context`,
//! `mneme://procedural`, `mneme://session/{id}`, `mneme://recent`,
//! `mneme://scopes`) lands in later phases as the underlying memory tiers
//! come online.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;

pub mod procedural;
pub mod recent;
pub mod stats;

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Debug, Clone)]
pub struct ResourceDescriptor {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

#[derive(Debug, Clone)]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: &'static str,
    pub text: String,
}

impl ResourceContent {
    pub fn to_json(&self) -> Value {
        json!({
            "uri": self.uri,
            "mimeType": self.mime_type,
            "text": self.text,
        })
    }
}

#[async_trait]
pub trait Resource: Send + Sync {
    fn descriptor(&self) -> ResourceDescriptor;
    async fn read(&self) -> Result<ResourceContent, ResourceError>;
}

#[derive(Default)]
pub struct ResourceRegistry {
    resources: BTreeMap<&'static str, Arc<dyn Resource>>,
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// v0.1 default resource set. Phase 5 will add `mneme://context`
    /// and `mneme://session/{id}` once the orchestrator + session
    /// management land.
    pub fn defaults(
        procedural_store: Arc<ProceduralStore>,
        episodic_store: Arc<EpisodicStore>,
    ) -> Self {
        let mut r = Self::new();
        r.register(Arc::new(stats::Stats));
        r.register(Arc::new(procedural::Procedural::new(procedural_store)));
        r.register(Arc::new(recent::Recent::new(episodic_store)));
        r
    }

    pub fn register(&mut self, resource: Arc<dyn Resource>) {
        let uri = resource.descriptor().uri;
        self.resources.insert(uri, resource);
    }

    pub fn get(&self, uri: &str) -> Option<Arc<dyn Resource>> {
        self.resources.get(uri).cloned()
    }

    pub fn list(&self) -> Vec<ResourceDescriptor> {
        self.resources.values().map(|r| r.descriptor()).collect()
    }
}

pub fn descriptor_to_json(d: &ResourceDescriptor) -> Value {
    json!({
        "uri": d.uri,
        "name": d.name,
        "description": d.description,
        "mimeType": d.mime_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn fresh_registry() -> (ResourceRegistry, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let pstore = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let estore = Arc::new(EpisodicStore::new(backing));
        (ResourceRegistry::defaults(pstore, estore), tmp)
    }

    #[test]
    fn defaults_register_phase_4_resources() {
        let (r, _tmp) = fresh_registry();
        let uris: Vec<_> = r.list().iter().map(|d| d.uri).collect();
        // BTreeMap ordering: procedural, recent, stats.
        assert_eq!(
            uris,
            vec!["mneme://procedural", "mneme://recent", "mneme://stats"]
        );
    }
}
