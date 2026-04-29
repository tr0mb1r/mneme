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

use std::path::PathBuf;

use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::consolidation_scheduler::ConsolidationScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
use crate::memory::working::ActiveSession;
use crate::orchestrator::{Orchestrator, TokenBudget};
use crate::scope::ScopeState;
use crate::storage::archive::ColdArchive;

pub mod context;
pub mod procedural;
pub mod recent;
pub mod session;
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
    /// Read the resource. The `uri` parameter is the exact URI the
    /// client requested — for fixed-URI resources (e.g.
    /// `mneme://stats`) it equals `descriptor().uri`; for template
    /// resources (e.g. `mneme://session/{id}`) it carries the
    /// substituted form (`mneme://session/01H...`). Implementations
    /// that don't care can ignore the parameter.
    async fn read(&self, uri: &str) -> Result<ResourceContent, ResourceError>;
}

/// Registry that supports both fixed and *template* URIs. Fixed URIs
/// (`mneme://stats`, `mneme://procedural`, etc.) match by equality
/// in the BTreeMap. Template URIs are stored as a prefix string —
/// any incoming `read` URI starting with that prefix routes to the
/// template's handler. This is the simplest URI-template scheme that
/// covers the v1.0 surface (just `mneme://session/{id}`); a real
/// RFC 6570 parser would be overkill until we add more templates.
#[derive(Default)]
pub struct ResourceRegistry {
    resources: BTreeMap<&'static str, Arc<dyn Resource>>,
    templates: Vec<(String, Arc<dyn Resource>)>,
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
        Self::defaults_with_schedulers(
            semantic_store,
            procedural_store,
            episodic_store,
            orchestrator,
            cold,
            schema_version,
            budget,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// Like [`defaults`](Self::defaults) but also attaches the L3
    /// consolidation scheduler, the L1 checkpoint scheduler, the
    /// active session, the sessions directory, and the scope state
    /// so the per-session resource (`mneme://session/{id}`) and the
    /// observability counters on `mneme://stats` (including the
    /// `working.current_scope` field) are wired. Callers that don't
    /// run the schedulers (tests, CLI helpers) keep using
    /// `defaults`.
    #[allow(clippy::too_many_arguments)]
    pub fn defaults_with_schedulers(
        semantic_store: Arc<SemanticStore>,
        procedural_store: Arc<ProceduralStore>,
        episodic_store: Arc<EpisodicStore>,
        orchestrator: Arc<Orchestrator>,
        cold: ColdArchive,
        schema_version: u32,
        budget: TokenBudget,
        consolidation: Option<Arc<ConsolidationScheduler>>,
        checkpoints: Option<Arc<CheckpointScheduler>>,
        active_session: Option<Arc<ActiveSession>>,
        sessions_dir: Option<PathBuf>,
        scope_state: Option<Arc<ScopeState>>,
    ) -> Self {
        let mut r = Self::new();
        let mut stats_resource = stats::Stats::new(
            semantic_store,
            Arc::clone(&procedural_store),
            Arc::clone(&episodic_store),
            cold,
            schema_version,
        );
        if let Some(sched) = consolidation {
            stats_resource = stats_resource.with_consolidation(sched);
        }
        if let Some(sched) = checkpoints {
            stats_resource = stats_resource.with_checkpoints(sched);
        }
        if let Some(s) = scope_state.as_ref() {
            stats_resource = stats_resource.with_scope_state(Arc::clone(s));
        }
        r.register(Arc::new(stats_resource));
        r.register(Arc::new(procedural::Procedural::new(Arc::clone(
            &procedural_store,
        ))));
        r.register(Arc::new(recent::Recent::new(Arc::clone(&episodic_store))));
        r.register(Arc::new(context::Context::new(orchestrator, budget)));

        // Register `mneme://session/{id}` as a template resource. The
        // sessions_dir is required for past-session disk loads;
        // active_session is optional (None ⇒ only past sessions
        // resolvable, useful for tests).
        if let Some(dir) = sessions_dir {
            r.register_template(
                session::URI_PREFIX,
                Arc::new(session::SessionResource::new(active_session, dir)),
            );
        }
        r
    }

    pub fn register(&mut self, resource: Arc<dyn Resource>) {
        let uri = resource.descriptor().uri;
        self.resources.insert(uri, resource);
    }

    /// Register a template resource that handles every URI sharing
    /// the given prefix. The resource's own `descriptor().uri` is
    /// reported in `tools/list` (typically the RFC 6570 form like
    /// `mneme://session/{id}`); the prefix is what's matched at
    /// dispatch time.
    pub fn register_template(&mut self, prefix: impl Into<String>, resource: Arc<dyn Resource>) {
        self.templates.push((prefix.into(), resource));
    }

    /// Look up the resource for a specific URI. Tries exact match
    /// first (fixed URIs), then prefix match (templates).
    pub fn find(&self, uri: &str) -> Option<Arc<dyn Resource>> {
        if let Some(r) = self.resources.get(uri) {
            return Some(Arc::clone(r));
        }
        self.templates
            .iter()
            .find(|(prefix, _)| uri.starts_with(prefix.as_str()))
            .map(|(_, r)| Arc::clone(r))
    }

    /// Convenience for the (legacy) exact-URI lookup. Kept so
    /// existing callers and tests can keep using the old name.
    pub fn get(&self, uri: &str) -> Option<Arc<dyn Resource>> {
        self.find(uri)
    }

    pub fn list(&self) -> Vec<ResourceDescriptor> {
        self.resources
            .values()
            .map(|r| r.descriptor())
            .chain(self.templates.iter().map(|(_, r)| r.descriptor()))
            .collect()
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
