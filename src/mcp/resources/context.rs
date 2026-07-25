//! `mneme://context` — Phase 5. The auto-context the agent reads at
//! session start: pinned procedural anchors (L0) + working-session
//! turns (L1) + recent episodic events (L3) + — when the read carries
//! a query — semantically-similar long-term memories (L4), all packed
//! inside the configured token budget.
//!
//! # Query parameters
//!
//! The bare URI `mneme://context` returns L0 + L1 + L3. A vector
//! search needs something to be similar *to*, so L4 participates only
//! when the caller seeds it. Parameters ride on the URI, which is the
//! only channel MCP's `resources/read` gives a client (it takes a URI
//! and nothing else):
//!
//! | Param   | Meaning |
//! |---------|---------|
//! | `q`     | Natural-language seed for the L4 semantic layer. Percent-decoded. |
//! | `scope` | Restrict L0 / L3 / L4 to one scope. L1 turns are session-local and always included. |
//! | `limit` | Semantic over-fetch before the budget pass trims. Clamped by the orchestrator. |
//!
//! `query` is accepted as an alias for `q`. Unknown parameters are
//! ignored rather than rejected — a host that appends a cache-buster
//! should still get its context.
//!
//! Before v1.3 this resource passed no query and emitted a hardcoded
//! empty `semantic` array, so the entire L4 layer was unreachable
//! through auto-context even though the docs advertised it. The
//! sections below are all live now.
//!
//! The output is JSON with four layer sections + a `total_tokens`
//! count; the agent's host renders it however it likes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::memory::semantic::similarity_from_distance;
use crate::orchestrator::{ContextRequest, Orchestrator, TokenBudget};

/// Fixed URI — the no-parameter form, and what `resources/list`
/// advertises.
pub const URI: &str = "mneme://context";

/// Prefix the registry matches so parameterised reads
/// (`mneme://context?q=…`) route here too.
pub const URI_QUERY_PREFIX: &str = "mneme://context?";

/// RFC 6570 form advertised in `resources/templates/list`.
pub const URI_TEMPLATE: &str = "mneme://context{?q,scope,limit}";

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

    /// Turn `mneme://context?q=how%20do%20we%20deploy&scope=work` into
    /// a [`ContextRequest`].
    ///
    /// Deliberately lenient: a malformed `limit`, an unknown key, or a
    /// stray `&` yields a request that still assembles something
    /// useful rather than an error. The one thing we do reject is
    /// nothing — auto-context is the resource an agent reads
    /// unattended, and failing it closed would strand the session with
    /// no memory at all.
    fn parse_request(uri: &str) -> ContextRequest {
        let Some((_, raw_query)) = uri.split_once('?') else {
            return ContextRequest::default();
        };
        // Drop any fragment; it is not part of the query string.
        let raw_query = raw_query.split('#').next().unwrap_or("");

        let mut req = ContextRequest::default();
        for pair in raw_query.split('&').filter(|s| !s.is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                // A bare flag has no value to use; skip it.
                None => continue,
            };
            let decoded = percent_decode(value);
            if decoded.trim().is_empty() {
                continue;
            }
            match key {
                "q" | "query" => req.query = Some(decoded),
                "scope" => req.scope = Some(decoded),
                "limit" => req.semantic_limit = decoded.trim().parse::<usize>().ok(),
                _ => {}
            }
        }
        req
    }
}

/// Minimal `application/x-www-form-urlencoded` decoder: `%XX` escapes
/// plus `+` for space.
///
/// Hand-rolled rather than pulling a URL crate for one call site — the
/// input is a single query-string value from a local MCP client, and
/// `percent-encoding` would be a new dependency in a tree we keep
/// deliberately small. Invalid escapes are passed through verbatim,
/// matching the lenient parsing contract above.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // A client that sent invalid UTF-8 through percent escapes gets
    // lossy replacement rather than a failed read.
    String::from_utf8_lossy(&out).into_owned()
}

#[async_trait]
impl Resource for Context {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: URI,
            name: "context",
            description: "Auto-assembled context block: pinned procedural rules + \
                          working-session turns + recent episodic events, packed inside \
                          the configured token budget. Append `?q=<text>` to also fold in \
                          semantically-similar long-term memories, and `?scope=<name>` to \
                          restrict it to one scope.",
            mime_type: "application/json",
        }
    }

    async fn read(&self, uri: &str) -> Result<ResourceContent, ResourceError> {
        let req = Self::parse_request(uri);
        let ctx = self
            .orchestrator
            .build_context_with(&req, self.budget)
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
            // L1 working-session turns. The assembler has always
            // scored and budgeted these (WORKING_WEIGHT = 0.9); before
            // v1.3 the section was missing from this body, so their
            // tokens were charged against the budget while the content
            // itself never reached the agent.
            "working": ctx.working.iter().map(|t| json!({
                "role": t.role,
                "content": t.content,
                "at": t.at.to_rfc3339(),
            })).collect::<Vec<_>>(),
            "episodic": ctx.episodic.iter().map(|e| json!({
                "id": e.id.to_string(),
                "kind": e.kind,
                "scope": e.scope,
                "payload": e.payload,
                "tags": e.tags,
                "last_accessed": e.last_accessed.to_rfc3339(),
                "created_at": e.created_at.to_rfc3339(),
            })).collect::<Vec<_>>(),
            // L4 semantic hits. Non-empty only when the read carried a
            // `?q=` seed. Field names match the `recall` tool's rows so
            // an agent can treat both surfaces identically.
            "semantic": ctx.semantic.iter().map(|h| json!({
                "id": h.item.id.to_string(),
                "content": h.item.content,
                "kind": h.item.kind.as_str(),
                "tags": h.item.tags,
                "scope": h.item.scope,
                "created_at": h.item.created_at.to_rfc3339(),
                "score": h.score,
                "similarity": similarity_from_distance(h.score),
            })).collect::<Vec<_>>(),
            // Echo what the read was interpreted as, so a caller can
            // tell "no semantic hits" from "no query supplied".
            "request": {
                "query": req.query,
                "scope": req.scope,
            },
            "total_tokens": ctx.total_tokens,
            "max_tokens": self.budget.max_tokens,
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ResourceError::Internal(format!("serialise: {e}")))?;
        Ok(ResourceContent {
            // Echo the URI the client asked for, per MCP — not the
            // canonical fixed form. A client correlating the response
            // to its request needs the parameters back.
            uri: uri.to_owned(),
            mime_type: "application/json",
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::episodic::EpisodicStore;
    use crate::memory::procedural::ProceduralStore;
    use crate::memory::semantic::SemanticStore;
    use crate::orchestrator::TokenBudget;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn fixture() -> (Context, TempDir) {
        let (ctx, tmp, _semantic) = fixture_with_semantic();
        (ctx, tmp)
    }

    /// Same fixture, but hands back the semantic store so a test can
    /// prefill L4 before reading the resource.
    fn fixture_with_semantic() -> (Context, TempDir, Arc<SemanticStore>) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(backing));
        let orch = Arc::new(Orchestrator::new(
            Arc::clone(&semantic),
            procedural,
            episodic,
        ));
        let budget = TokenBudget::for_tests(2000);
        (Context::new(orch, budget), tmp, semantic)
    }

    #[tokio::test]
    async fn empty_context_returns_valid_json() {
        let (ctx, _tmp) = fixture();
        let c = ctx.read("mneme://context").await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert!(v["procedural"].as_array().unwrap().is_empty());
        assert!(v["episodic"].as_array().unwrap().is_empty());
        assert!(v["semantic"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serialisation_shape_is_correct() {
        let (ctx, _tmp) = fixture();
        let c = ctx.read("mneme://context").await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert!(v.get("procedural").is_some());
        assert!(v.get("episodic").is_some());
        assert!(v.get("semantic").is_some());
        assert!(v.get("total_tokens").is_some());
        assert!(v.get("max_tokens").is_some());
        assert_eq!(v["max_tokens"], 2000);
        assert_eq!(c.mime_type, "application/json");
        assert_eq!(c.uri, "mneme://context");
        // v1.3: the L1 section is emitted, not silently dropped.
        assert!(v.get("working").is_some(), "missing `working` section");
    }

    // ---------- query-parameter parsing ----------

    #[test]
    fn parse_request_bare_uri_is_default() {
        assert_eq!(
            Context::parse_request("mneme://context"),
            ContextRequest::default()
        );
    }

    #[test]
    fn parse_request_reads_q_scope_and_limit() {
        let req = Context::parse_request("mneme://context?q=deploy&scope=work&limit=5");
        assert_eq!(req.query.as_deref(), Some("deploy"));
        assert_eq!(req.scope.as_deref(), Some("work"));
        assert_eq!(req.semantic_limit, Some(5));
    }

    #[test]
    fn parse_request_accepts_query_alias_and_percent_escapes() {
        let req = Context::parse_request("mneme://context?query=how%20do%20we%20deploy%3F");
        assert_eq!(req.query.as_deref(), Some("how do we deploy?"));
        // `+` is the form-encoded space.
        let req = Context::parse_request("mneme://context?q=two+words");
        assert_eq!(req.query.as_deref(), Some("two words"));
    }

    /// Auto-context is read unattended; a malformed parameter must
    /// degrade rather than fail the read and strand the session with no
    /// memory at all.
    #[test]
    fn parse_request_is_lenient_about_junk() {
        let req =
            Context::parse_request("mneme://context?limit=abc&unknown=1&bareflag&=&q=real#frag");
        assert_eq!(req.semantic_limit, None, "unparseable limit is ignored");
        assert_eq!(req.query.as_deref(), Some("real"));
        assert_eq!(req.scope, None);
    }

    #[test]
    fn parse_request_ignores_blank_values() {
        let req = Context::parse_request("mneme://context?q=&scope=%20");
        assert_eq!(req.query, None);
        assert_eq!(req.scope, None);
    }

    #[test]
    fn percent_decode_passes_through_invalid_escapes() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
    }

    // ---------- semantic fold-in ----------

    /// The headline v1.3 fix: before this, `read` passed no query and
    /// emitted a hardcoded `"semantic": []`, so nothing an agent ever
    /// `remember`ed could reach auto-context.
    #[tokio::test]
    async fn query_parameter_folds_in_semantic_hits() {
        use crate::memory::semantic::MemoryKind;
        let (ctx, _tmp, semantic) = fixture_with_semantic();
        let id = semantic
            .remember(
                "we deploy with flyctl, never docker push",
                MemoryKind::Decision,
                vec![],
                "global".into(),
            )
            .await
            .unwrap();

        let c = ctx
            .read("mneme://context?q=how%20do%20we%20deploy")
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        let sem = v["semantic"].as_array().unwrap();
        assert_eq!(sem.len(), 1, "semantic section should carry the memory");
        assert_eq!(sem[0]["id"], id.to_string());
        assert_eq!(sem[0]["kind"], "decision");
        assert!(sem[0].get("similarity").is_some());
        // The echoed request lets a caller distinguish "no hits" from
        // "no query supplied".
        assert_eq!(v["request"]["query"], "how do we deploy");
    }

    #[tokio::test]
    async fn bare_read_leaves_semantic_empty() {
        use crate::memory::semantic::MemoryKind;
        let (ctx, _tmp, semantic) = fixture_with_semantic();
        semantic
            .remember(
                "something memorable",
                MemoryKind::Fact,
                vec![],
                "global".into(),
            )
            .await
            .unwrap();

        let c = ctx.read("mneme://context").await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert!(
            v["semantic"].as_array().unwrap().is_empty(),
            "a read with no query has nothing to be similar to"
        );
        assert!(v["request"]["query"].is_null());
    }

    #[tokio::test]
    async fn scope_parameter_filters_semantic_hits() {
        use crate::memory::semantic::MemoryKind;
        let (ctx, _tmp, semantic) = fixture_with_semantic();
        semantic
            .remember(
                "shared topic in work",
                MemoryKind::Fact,
                vec![],
                "work".into(),
            )
            .await
            .unwrap();
        semantic
            .remember(
                "shared topic in personal",
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();

        let c = ctx
            .read("mneme://context?q=shared%20topic&scope=work")
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        let sem = v["semantic"].as_array().unwrap();
        assert_eq!(sem.len(), 1);
        assert_eq!(sem[0]["scope"], "work");
    }

    /// MCP correlates a `resources/read` reply to its request by URI,
    /// so the parameterised form must come back, not the canonical one.
    #[tokio::test]
    async fn read_echoes_the_requested_uri() {
        let (ctx, _tmp) = fixture();
        let uri = "mneme://context?q=anything&scope=work";
        let c = ctx.read(uri).await.unwrap();
        assert_eq!(c.uri, uri);
    }
}
