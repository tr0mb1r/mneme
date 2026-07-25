//! Resource registry. A `Resource` is a noun the agent can read.
//!
//! v1 surface (all shipping): `mneme://stats`, `mneme://procedural`,
//! `mneme://recent`, `mneme://context`, and the template resource
//! `mneme://session/{id}`. See `book/src/mcp-surface.md` for the
//! authoritative inventory.

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
use crate::storage::Storage;
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

/// A parameterised resource, advertised through MCP's
/// `resources/templates/list`. Distinct from [`ResourceDescriptor`]
/// because the spec puts templates on their own endpoint with a
/// `uriTemplate` key instead of `uri` — a client that only reads
/// `resources/list` cannot discover them.
#[derive(Debug, Clone)]
pub struct ResourceTemplateDescriptor {
    /// RFC 6570 form, e.g. `mneme://session/{id}`.
    pub uri_template: &'static str,
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

/// One registered template: the dispatch prefix, the RFC 6570 form to
/// advertise, and whether the same handler is also reachable at a
/// fixed URI.
struct TemplateEntry {
    /// Matched with `starts_with` at dispatch time.
    prefix: String,
    /// What `resources/templates/list` advertises.
    uri_template: &'static str,
    resource: Arc<dyn Resource>,
    /// `true` when this handler already appears in `resources/list`
    /// under a fixed URI, so listing it again would duplicate the
    /// entry. `mneme://context` is both a fixed resource and a
    /// template (`mneme://context{?q,scope,limit}`);
    /// `mneme://session/{id}` is template-only.
    also_fixed: bool,
}

/// Registry that supports both fixed and *template* URIs. Fixed URIs
/// (`mneme://stats`, `mneme://procedural`, etc.) match by equality
/// in the BTreeMap. Template URIs are stored as a prefix string —
/// any incoming `read` URI starting with that prefix routes to the
/// template's handler. This is the simplest URI-template scheme that
/// covers the v1 surface (`mneme://session/{id}` and the query form of
/// `mneme://context`); a real RFC 6570 parser would be overkill until
/// a template needs mid-path variables.
#[derive(Default)]
pub struct ResourceRegistry {
    resources: BTreeMap<&'static str, Arc<dyn Resource>>,
    templates: Vec<TemplateEntry>,
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The default resource set with no schedulers or session state
    /// attached: `mneme://stats`, `mneme://procedural`,
    /// `mneme://recent`, and `mneme://context` (both its bare and
    /// parameterised forms). `mneme://session/{id}` needs a sessions
    /// directory, so it registers only via
    /// [`defaults_with_schedulers`](Self::defaults_with_schedulers).
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
        size_scan: Option<(Arc<dyn Storage>, usize)>,
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
        if let Some((storage, max_chars)) = size_scan {
            stats_resource = stats_resource.with_size_scan(storage, max_chars);
        }
        r.register(Arc::new(stats_resource));
        r.register(Arc::new(procedural::Procedural::new(Arc::clone(
            &procedural_store,
        ))));
        r.register(Arc::new(recent::Recent::new(Arc::clone(&episodic_store))));

        // `mneme://context` is registered twice against one handler:
        // once as the fixed bare URI (what `resources/list` shows and
        // what an unparameterised read hits), and once as a prefix so
        // `mneme://context?q=…&scope=…` routes to the same place.
        // Without the second registration a parameterised read would
        // 404 on the exact-match lookup.
        let context_resource = Arc::new(context::Context::new(orchestrator, budget));
        r.register(Arc::clone(&context_resource) as Arc<dyn Resource>);
        r.register_query_template(
            context::URI_QUERY_PREFIX,
            context::URI_TEMPLATE,
            context_resource,
        );

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
    /// reported in `resources/list` (typically the RFC 6570 form like
    /// `mneme://session/{id}`); the prefix is what's matched at
    /// dispatch time.
    pub fn register_template(&mut self, prefix: impl Into<String>, resource: Arc<dyn Resource>) {
        let prefix = prefix.into();
        let uri_template = resource.descriptor().uri;
        self.templates.push(TemplateEntry {
            prefix,
            uri_template,
            resource,
            also_fixed: false,
        });
    }

    /// Register an additional *parameterised* route to a resource that
    /// is already registered at a fixed URI. Used for
    /// `mneme://context?…`: the bare URI keeps its `resources/list`
    /// entry while the query form gets its own `uriTemplate` in
    /// `resources/templates/list`, both served by the same handler.
    pub fn register_query_template(
        &mut self,
        prefix: impl Into<String>,
        uri_template: &'static str,
        resource: Arc<dyn Resource>,
    ) {
        self.templates.push(TemplateEntry {
            prefix: prefix.into(),
            uri_template,
            resource,
            also_fixed: true,
        });
    }

    /// Look up the resource for a specific URI. Tries exact match
    /// first (fixed URIs), then prefix match (templates).
    pub fn find(&self, uri: &str) -> Option<Arc<dyn Resource>> {
        if let Some(r) = self.resources.get(uri) {
            return Some(Arc::clone(r));
        }
        self.templates
            .iter()
            .find(|t| uri.starts_with(t.prefix.as_str()))
            .map(|t| Arc::clone(&t.resource))
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
            .chain(
                self.templates
                    .iter()
                    .filter(|t| !t.also_fixed)
                    .map(|t| t.resource.descriptor()),
            )
            .collect()
    }

    /// Every parameterised route, for MCP's `resources/templates/list`.
    /// Includes the query form of resources that also have a fixed URI.
    pub fn list_templates(&self) -> Vec<ResourceTemplateDescriptor> {
        self.templates
            .iter()
            .map(|t| {
                let d = t.resource.descriptor();
                ResourceTemplateDescriptor {
                    uri_template: t.uri_template,
                    name: d.name,
                    description: d.description,
                    mime_type: d.mime_type,
                }
            })
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

pub fn template_descriptor_to_json(d: &ResourceTemplateDescriptor) -> Value {
    json!({
        "uriTemplate": d.uri_template,
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
        // BTreeMap ordering. `mneme://context` appears exactly once
        // even though it is registered both as a fixed URI and as a
        // query template.
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

    /// A parameterised context read must resolve. The exact-match
    /// lookup misses it, so this exercises the prefix fallback.
    #[test]
    fn parameterised_context_uri_routes_to_the_context_resource() {
        let (r, _tmp) = fresh_registry();
        let found = r
            .find("mneme://context?q=deploy&scope=work")
            .expect("query form must route");
        assert_eq!(found.descriptor().uri, "mneme://context");
    }

    #[test]
    fn list_templates_advertises_the_context_query_form() {
        let (r, _tmp) = fresh_registry();
        let templates: Vec<_> = r.list_templates().iter().map(|t| t.uri_template).collect();
        assert!(
            templates.contains(&"mneme://context{?q,scope,limit}"),
            "templates were {templates:?}"
        );
    }

    /// With a sessions dir attached, both templates are advertised and
    /// `mneme://session/{id}` still appears in `resources/list` (it has
    /// no fixed-URI counterpart to duplicate).
    #[test]
    fn session_template_is_listed_in_both_places() {
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
        let r = ResourceRegistry::defaults_with_schedulers(
            semantic,
            pstore,
            estore,
            orch,
            cold,
            1,
            TokenBudget::for_tests(2000),
            None,
            None,
            None,
            Some(tmp.path().join("sessions")),
            None,
            None,
        );

        let uris: Vec<_> = r.list().iter().map(|d| d.uri).collect();
        assert!(uris.contains(&"mneme://session/{id}"), "got {uris:?}");
        assert_eq!(
            uris.iter().filter(|u| **u == "mneme://context").count(),
            1,
            "context must not be listed twice: {uris:?}"
        );

        let templates: Vec<_> = r.list_templates().iter().map(|t| t.uri_template).collect();
        assert!(
            templates.contains(&"mneme://session/{id}"),
            "got {templates:?}"
        );
        assert!(templates.contains(&"mneme://context{?q,scope,limit}"));
    }
}
