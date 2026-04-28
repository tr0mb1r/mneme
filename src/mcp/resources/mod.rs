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
use crate::memory::semantic::SemanticStore;
use crate::orchestrator::{Orchestrator, TokenBudget};
use crate::storage::archive::ColdArchive;

pub mod context;
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

    /// v0.1 default resource set. `mneme://session/{id}` is still
    /// deferred — sessions live in `memory::working` but their
    /// per-id resource surface is not wired yet.
    pub fn defaults(
        semantic_store: Arc<SemanticStore>,
        procedural_store: Arc<ProceduralStore>,
        episodic_store: Arc<EpisodicStore>,
        orchestrator: Arc<Orchestrator>,
        cold: ColdArchive,
        schema_version: u32,
        budget: TokenBudget,
    ) -> Self {
        let mut r = Self::new();
        r.register(Arc::new(stats::Stats::new(
            semantic_store,
            Arc::clone(&procedural_store),
            Arc::clone(&episodic_store),
            cold,
            schema_version,
        )));
        r.register(Arc::new(procedural::Procedural::new(Arc::clone(
            &procedural_store,
        ))));
        r.register(Arc::new(recent::Recent::new(Arc::clone(&episodic_store))));
        r.register(Arc::new(context::Context::new(orchestrator, budget)));
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
        use crate::embed::Embedder;
        use crate::embed::stub::StubEmbedder;
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let pstore = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let estore = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let orch = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&pstore),
            Arc::clone(&estore),
        ));
        let cold = ColdArchive::new(tmp.path());
        (
            ResourceRegistry::defaults(
                semantic,
                pstore,
                estore,
                orch,
                cold,
                1,
                TokenBudget::for_tests(2000),
            ),
            tmp,
        )
    }

    #[test]
    fn defaults_register_phase_5_resources() {
        let (r, _tmp) = fresh_registry();
        let uris: Vec<_> = r.list().iter().map(|d| d.uri).collect();
        // BTreeMap ordering.
        assert_eq!(
            uris,
            vec![
                "mneme://context",
                "mneme://procedural",
                "mneme://recent",
                "mneme://stats",
            ]
        );
    }
}
