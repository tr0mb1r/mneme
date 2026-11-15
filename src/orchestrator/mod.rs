//! Phase 5 — orchestrator and auto-context assembly.
//!
//! [`Orchestrator::build_context`] is the single entry point. Given
//! an optional natural-language query and an optional scope, it
//! fetches candidate items from every memory layer in parallel,
//! scores them, enforces a token budget, and emits an
//! [`AssembledContext`] suitable for prepending to an LLM prompt.
//!
//! ## Layer fan-out
//!
//! ```text
//!   ┌────────────────────────────────────────────────────────┐
//!   │ L0 procedural   (always-on pinned items)               │
//!   │ L3 episodic     (recent events ∪ query matches)        │
//!   │ L4 semantic     (HNSW search if a query is supplied)    │
//!   └────────────────────────────────────────────────────────┘
//! ```
//!
//! L1 (working session) is folded in when an [`ActiveSession`] is
//! attached via [`Orchestrator::with_active_session`] — production
//! always attaches one; fixtures that only exercise L0/L3/L4 skip it.
//! L5 (reflections) does not exist yet; the plumbing here is
//! deliberately layer-agnostic so dropping it in is a small change.
//!
//! ## Query seeding
//!
//! L4 participates only when the caller supplies a query — a vector
//! search needs something to be similar to. `mneme://context` takes
//! that query off its URI (`mneme://context?q=…`), so a caller who
//! reads the bare `mneme://context` still gets L0 + L1 + L3 only.
//!
//! The same query conditions L3. Episodic candidates are the most
//! recent events plus, when there is a query, the episodic index's best
//! lexical matches for it, so an on-topic event from two months ago can
//! be offered at all. The assembler then ranks them by recency mixed
//! with lexical coverage ([`events::select_events`]).
//!
//! ## Scopes
//!
//! A request names zero or more scopes. None means everything, as
//! always. Otherwise L3 and L4 are filtered to that set, and L0 returns
//! the pins of those scopes *plus* the always-on
//! `[auto_context] pin_scopes` (default `["global"]`), so scoping a read
//! to a project no longer drops the user's global rules.
//!
//! ## Determinism
//!
//! Same inputs + same DB state ⇒ byte-identical [`AssembledContext`].
//! Stable sorts everywhere, no `HashMap` iteration leakage. The Phase
//! 5 exit gate explicitly requires this.
//!
//! ## Budget
//!
//! Token counting uses a `chars / 4` estimator (plan §0 fallback).
//! Replacing it with `tokenizers`/`tiktoken-rs` is a one-function
//! swap; the public API doesn't change. How the budget is split
//! between layers comes from `[auto_context]`: see [`assembly`].

pub mod assembly;
pub mod budget;
pub mod events;
pub mod scoring;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::Result;
use crate::config::AutoContextConfig;
use crate::ids::EventId;
use crate::memory::episodic::{EpisodicEvent, EpisodicStore, RecentFilters};
use crate::memory::procedural::{PinnedItem, ProceduralStore};
use crate::memory::semantic::{RecallFilters, RecallHit, SemanticStore};
use crate::memory::working::{ActiveSession, Turn};

pub use assembly::{AssembledContext, AssemblyParams, LayerTokens};
pub use budget::{LayerShares, TokenBudget};
pub use scoring::ScoredItem;

/// Per-layer over-fetch cap before scoring + trimming. Picked large
/// enough that the budget pass has room to drop low-signal items
/// without re-querying. Tuned for v0.1 cardinalities (~thousands per
/// scope).
const PROCEDURAL_FETCH: usize = 64;
const EPISODIC_FETCH: usize = 64;
const SEMANTIC_FETCH: usize = 32;
/// Working session over-fetch — last N turns from the active session.
/// Working turns are session-local so the cardinality is bounded;
/// the budget pass trims further.
const WORKING_FETCH: usize = 32;

/// The recency window is read this many times wider than
/// [`EPISODIC_FETCH`] before events at or below
/// `[auto_context] min_event_weight` are dropped, so down-weighted rows
/// cannot use up the window.
const EPISODIC_WEIGHT_OVERFETCH: usize = 2;

/// Ceiling on a caller-supplied semantic over-fetch. Guards against a
/// `mneme://context?limit=100000` read turning into an index walk plus
/// 100 000 storage gets; the token budget would discard nearly all of
/// it anyway.
const MAX_SEMANTIC_FETCH: usize = 100;

/// What to assemble into one auto-context read.
///
/// `Default` (everything empty) reproduces the pre-v1.3 behaviour:
/// no semantic seed, no scope filter, built-in over-fetch caps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextRequest {
    /// Natural-language seed for the L4 semantic layer, and the query
    /// episodic events are ranked against. `None` (or blank) leaves the
    /// semantic section empty — a static read with no query has nothing
    /// to be similar *to* — and ranks events by recency alone.
    pub query: Option<String>,
    /// Restrict L3 / L4 to these scopes, and L0 to these scopes plus
    /// `[auto_context] pin_scopes`. Empty means no restriction.
    /// Working-session turns (L1) are session-local and always
    /// included.
    pub scope: Vec<String>,
    /// How many semantic candidates to over-fetch before the budget
    /// pass trims. Defaults to [`SEMANTIC_FETCH`]; clamped to
    /// [`MAX_SEMANTIC_FETCH`].
    pub semantic_limit: Option<usize>,
    /// Include only episodic events of these kinds. When non-empty it
    /// replaces the default exclusion of [`DEFAULT_EXCLUDED_KINDS`]:
    /// naming a kind is how a caller opts back in to it.
    pub kinds: Vec<String>,
    /// Episodic kinds to hide, in addition to the default exclusions.
    pub exclude_kinds: Vec<String>,
}

impl ContextRequest {
    /// The query, if it has any non-blank text.
    fn effective_query(&self) -> Option<&str> {
        self.query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
    }
}

/// Episodic kinds kept out of auto-context unless the request asks for
/// them via [`ContextRequest::kinds`].
///
/// The first four are auto-logged: mirrored in by the server (ADR-0009)
/// rather than written by the agent, so they dominate the hot tier (56
/// of 61 events in one deployment) and crowd real decisions out of the
/// token budget. `archived` is the agent's own marker for "no longer
/// current", which is exactly what a context block should not carry;
/// such events stay reachable through `recall_recent` and `export`.
pub const DEFAULT_EXCLUDED_KINDS: &[&str] = &[
    "tool_call",
    "tool_call_failed",
    "session_start",
    "session_end",
    "archived",
];

/// Phase 5 §3 owner. Holds an `Arc` to each memory store so it can
/// be cheaply cloned into background tasks (the snapshot scheduler,
/// future `auto_context` tool, etc.).
#[derive(Clone)]
pub struct Orchestrator {
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    /// `Some(_)` once the L1 read-side fold-in is wired (production);
    /// `None` in fixtures that only test L0/L3/L4. Empty session
    /// folds in zero turns, which is fine.
    active_session: Option<Arc<ActiveSession>>,
    /// `[auto_context]`: scoring knobs, layer shares, event selection.
    auto_context: Arc<AutoContextConfig>,
}

impl Orchestrator {
    pub fn new(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
    ) -> Self {
        Self {
            semantic,
            procedural,
            episodic,
            active_session: None,
            auto_context: Arc::new(AutoContextConfig::default()),
        }
    }

    /// Builder hook: attach an active session so `build_context` pulls
    /// the L1 working layer (recent turns) into auto-context. Tests
    /// that only exercise L0/L3/L4 skip this.
    pub fn with_active_session(mut self, session: Arc<ActiveSession>) -> Self {
        self.active_session = Some(session);
        self
    }

    /// Builder hook: use the `[auto_context]` section of the loaded
    /// config instead of the defaults.
    pub fn with_auto_context(mut self, config: AutoContextConfig) -> Self {
        self.auto_context = Arc::new(config);
        self
    }

    /// The `[auto_context]` settings in force.
    pub fn auto_context(&self) -> &AutoContextConfig {
        &self.auto_context
    }

    /// Assemble a structured context for the agent.
    ///
    /// `query` is `Some(text)` when the caller wants L4 semantic
    /// recall folded in; `None` returns procedural + episodic only.
    /// `scope` filters L3 / L4 to a single scope (e.g. `"personal"`,
    /// `"work"`) and L0 to that scope plus `[auto_context] pin_scopes`.
    ///
    /// Thin wrapper over [`Orchestrator::build_context_with`] for the
    /// common two-argument case.
    pub async fn build_context(
        &self,
        query: Option<&str>,
        scope: Option<&str>,
        budget: TokenBudget,
    ) -> Result<AssembledContext> {
        let req = ContextRequest {
            query: query.map(|q| q.to_owned()),
            scope: scope.map(|s| vec![s.to_owned()]).unwrap_or_default(),
            semantic_limit: None,
            kinds: Vec::new(),
            exclude_kinds: Vec::new(),
        };
        self.build_context_with(&req, budget).await
    }

    /// Assemble a structured context from a full [`ContextRequest`],
    /// with ages measured from the current time.
    pub async fn build_context_with(
        &self,
        req: &ContextRequest,
        budget: TokenBudget,
    ) -> Result<AssembledContext> {
        self.build_context_at(req, budget, Utc::now()).await
    }

    /// [`Self::build_context_with`] with the clock passed in, so a
    /// test (or a replay) gets the same context for the same stores
    /// whenever it runs.
    ///
    /// Per spec §5.3, layers are fetched in parallel via
    /// `tokio::join!`; total wall time is the slowest layer plus a
    /// small amount of scoring/assembly overhead. The Phase 5 exit
    /// gate is `mneme://context` p95 < 200 ms.
    pub async fn build_context_at(
        &self,
        req: &ContextRequest,
        budget: TokenBudget,
        now: DateTime<Utc>,
    ) -> Result<AssembledContext> {
        let semantic_limit = req
            .semantic_limit
            .unwrap_or(SEMANTIC_FETCH)
            .min(MAX_SEMANTIC_FETCH);
        let query = req.effective_query();

        let proc_fut = self.fetch_procedural(&req.scope);
        let epi_fut = self.fetch_episodic(req, query);
        let sem_fut = self.fetch_semantic(query, &req.scope, semantic_limit);
        let work_fut = self.fetch_working();

        let (proc_res, epi_res, sem_res, work_res) =
            tokio::join!(proc_fut, epi_fut, sem_fut, work_fut);
        let procedural = proc_res?;
        let (episodic, lex) = epi_res?;
        let semantic = sem_res?;
        let working = work_res?;

        let ctx = assembly::assemble(
            procedural,
            semantic,
            episodic,
            working,
            &AssemblyParams {
                budget,
                config: &self.auto_context,
                now,
                lex: lex.as_ref(),
            },
        );
        // Pins are never trimmed, so the only signal that they are
        // squeezing the rest out is this one.
        // TODO: surface the same figure in `mneme://stats`.
        let pins = ctx.layer_tokens.procedural;
        if pins.saturating_mul(10) > budget.max_tokens.saturating_mul(3) {
            tracing::warn!(
                pinned_tokens = pins,
                max_tokens = budget.max_tokens,
                "pinned items use more than 30% of the auto-context budget"
            );
        }
        Ok(ctx)
    }

    /// Pins for the request: everything when no scope was asked for,
    /// otherwise the requested scopes plus the always-on
    /// `[auto_context] pin_scopes`.
    async fn fetch_procedural(&self, scopes: &[String]) -> Result<Vec<PinnedItem>> {
        // ProceduralStore::list is sync; wrap in async so the join!
        // call site reads uniformly.
        let mut items = if scopes.is_empty() {
            self.procedural.list(None)?
        } else {
            let mut wanted: Vec<String> = scopes.to_vec();
            for s in &self.auto_context.pin_scopes {
                if !wanted.contains(s) {
                    wanted.push(s.clone());
                }
            }
            self.procedural.list_in_scopes(&wanted)?
        };
        items.truncate(PROCEDURAL_FETCH);
        Ok(items)
    }

    /// Episodic candidates and, when the read has a query, their
    /// lexical coverage.
    ///
    /// Candidates are the recency window united with the episodic
    /// index's best matches for the query: a recency window alone can
    /// never surface an on-topic event that is older than the 64 most
    /// recent ones. Selection (dedup, per-kind cap, clipping) happens
    /// in the assembler.
    ///
    /// The index is advisory. If it errors the read falls back to the
    /// recency window, with every event scored as "no lexical match",
    /// rather than failing the whole context.
    async fn fetch_episodic(
        &self,
        req: &ContextRequest,
        query: Option<&str>,
    ) -> Result<(Vec<EpisodicEvent>, Option<HashMap<EventId, f32>>)> {
        let min_weight = self.auto_context.min_event_weight;
        // Filtering happens inside `recall_recent`'s scan so excluded
        // rows never occupy the over-fetch window. An explicit include
        // list opts back in to the default exclusions; without one
        // they apply on top of the caller's list.
        let filters = RecentFilters {
            scopes: req.scope.clone(),
            kinds: req.kinds.clone(),
            exclude_kinds: req.exclude_kinds.clone(),
            exclude_kinds_static: if req.kinds.is_empty() {
                DEFAULT_EXCLUDED_KINDS
            } else {
                &[]
            },
            ..Default::default()
        };

        let mut events: Vec<EpisodicEvent> = Vec::new();
        let mut seen: HashSet<EventId> = HashSet::new();
        let mut recent = self
            .episodic
            .recall_recent(&filters, EPISODIC_FETCH * EPISODIC_WEIGHT_OVERFETCH)
            .await?;
        recent.retain(|e| e.retrieval_weight > min_weight);
        recent.truncate(EPISODIC_FETCH);
        for e in recent {
            if seen.insert(e.id) {
                events.push(e);
            }
        }

        let Some(q) = query else {
            return Ok((events, None));
        };

        match self.episodic.search(q, EPISODIC_FETCH, &filters).await {
            Ok(hits) => {
                for (e, _) in hits {
                    if e.retrieval_weight > min_weight && seen.insert(e.id) {
                        events.push(e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "episodic search failed; using recent events only");
            }
        }

        let ids: Vec<EventId> = events.iter().map(|e| e.id).collect();
        let lex = match self.episodic.lex_scores(q, &ids).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(error = %e, "episodic lexical scoring failed; ranking by recency");
                HashMap::new()
            }
        };
        Ok((events, Some(lex)))
    }

    async fn fetch_semantic(
        &self,
        query: Option<&str>,
        scopes: &[String],
        limit: usize,
    ) -> Result<Vec<RecallHit>> {
        let Some(q) = query else {
            return Ok(Vec::new());
        };
        // Defaults keep superseded and archived memories out: neither
        // is current, and auto-context is for what is.
        let filters = RecallFilters {
            scopes: scopes.to_vec(),
            ..Default::default()
        };
        self.semantic.recall(q, limit, &filters).await
    }

    /// Fetch the last `WORKING_FETCH` turns from the active session,
    /// newest-last (chronological). The assembler re-orders to
    /// newest-first; we hand over the natural append order.
    /// Returns an empty Vec when no `ActiveSession` is attached —
    /// the assembler handles that as a zero-token contribution.
    /// Working turns are *session-local*, so the scope filter does
    /// not apply; the caller's scope filters L0/L3/L4 only.
    async fn fetch_working(&self) -> Result<Vec<Turn>> {
        let Some(active) = self.active_session.as_ref() else {
            return Ok(Vec::new());
        };
        let mut turns = active.turns_snapshot();
        if turns.len() > WORKING_FETCH {
            // Take the most-recent N. `turns_snapshot` is
            // append-ordered, so the tail is newest.
            turns = turns.split_off(turns.len() - WORKING_FETCH);
        }
        Ok(turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::episodic::EpisodicStore;
    use crate::memory::procedural::ProceduralStore;
    use crate::memory::semantic::{MemoryKind, SemanticStore};
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    /// Minimal three-store harness used by the orchestrator + assembly
    /// tests. Returns the orchestrator alongside the TempDir + Arcs
    /// the caller can prefill.
    pub(super) fn fresh_orchestrator() -> (Orchestrator, TempDir, Arcs) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        (
            Orchestrator::new(
                Arc::clone(&semantic),
                Arc::clone(&procedural),
                Arc::clone(&episodic),
            ),
            tmp,
            Arcs {
                semantic,
                procedural,
                episodic,
            },
        )
    }

    pub(super) struct Arcs {
        pub semantic: Arc<SemanticStore>,
        pub procedural: Arc<ProceduralStore>,
        pub episodic: Arc<EpisodicStore>,
    }

    #[tokio::test]
    async fn empty_stores_return_empty_context() {
        let (o, _tmp, _a) = fresh_orchestrator();
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert!(ctx.procedural.is_empty());
        assert!(ctx.semantic.is_empty());
        assert!(ctx.episodic.is_empty());
        assert_eq!(ctx.total_tokens, 0);
    }

    #[tokio::test]
    async fn build_context_pulls_each_layer() {
        let (o, _tmp, a) = fresh_orchestrator();
        a.procedural
            .pin("always test".into(), vec![], "personal".into())
            .await
            .unwrap();
        a.episodic
            .record("note", "personal", "\"git status\"")
            .await
            .unwrap();
        let _ = a
            .semantic
            .remember("hello world", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();

        let ctx = o
            .build_context(Some("hello world"), None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(ctx.procedural.len(), 1);
        assert_eq!(ctx.episodic.len(), 1);
        assert!(!ctx.semantic.is_empty());
        assert!(ctx.total_tokens > 0);
    }

    #[tokio::test]
    async fn no_query_skips_semantic_layer() {
        let (o, _tmp, a) = fresh_orchestrator();
        a.semantic
            .remember("anything", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert!(ctx.semantic.is_empty());
    }

    #[tokio::test]
    async fn scope_filter_propagates_to_each_layer() {
        let (o, _tmp, a) = fresh_orchestrator();
        a.procedural
            .pin("work pin".into(), vec![], "work".into())
            .await
            .unwrap();
        a.procedural
            .pin("personal pin".into(), vec![], "personal".into())
            .await
            .unwrap();
        a.episodic.record("note", "work", "\"a\"").await.unwrap();
        a.episodic
            .record("note", "personal", "\"b\"")
            .await
            .unwrap();

        let ctx = o
            .build_context(None, Some("work"), TokenBudget::default())
            .await
            .unwrap();
        assert!(ctx.procedural.iter().all(|p| p.scope == "work"));
        assert_eq!(ctx.episodic.len(), 1);
        assert!(ctx.episodic.iter().all(|e| e.scope == "work"));
    }

    async fn seed_mixed_kinds(a: &Arcs) {
        for kind in [
            "tool_call",
            "tool_call_failed",
            "session_start",
            "session_end",
            "decision",
            "summary",
        ] {
            a.episodic
                .record(kind, "personal", "{\"content\":\"x\"}")
                .await
                .unwrap();
        }
    }

    fn kinds_of(ctx: &AssembledContext) -> Vec<String> {
        let mut k: Vec<String> = ctx.episodic.iter().map(|e| e.kind.clone()).collect();
        k.sort();
        k
    }

    #[tokio::test]
    async fn default_context_hides_auto_logged_kinds() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_mixed_kinds(&a).await;
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(kinds_of(&ctx), vec!["decision", "summary"]);
    }

    #[tokio::test]
    async fn kinds_request_opts_back_in() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_mixed_kinds(&a).await;
        let req = ContextRequest {
            kinds: vec!["tool_call".into()],
            ..Default::default()
        };
        let ctx = o
            .build_context_with(&req, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(kinds_of(&ctx), vec!["tool_call"]);
    }

    #[tokio::test]
    async fn exclude_kinds_adds_to_the_default_exclusions() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_mixed_kinds(&a).await;
        let req = ContextRequest {
            exclude_kinds: vec!["decision".into()],
            ..Default::default()
        };
        let ctx = o
            .build_context_with(&req, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(kinds_of(&ctx), vec!["summary"]);
    }

    /// The point of filtering inside the scan: with more noise rows
    /// than the over-fetch window, the real events still come through.
    #[tokio::test]
    async fn noise_beyond_the_overfetch_window_does_not_crowd_out_events() {
        let (o, _tmp, a) = fresh_orchestrator();
        a.episodic
            .record("decision", "personal", "{\"content\":\"keep\"}")
            .await
            .unwrap();
        for i in 0..(EPISODIC_FETCH + 10) {
            a.episodic
                .record("tool_call", "personal", &format!("{{\"i\":{i}}}"))
                .await
                .unwrap();
        }
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(kinds_of(&ctx), vec!["decision"]);
    }

    #[tokio::test]
    async fn build_context_includes_working_when_session_attached() {
        use crate::memory::working::ActiveSession;
        use tempfile::TempDir;

        let (o, _tmp, _a) = fresh_orchestrator();
        let session_dir = TempDir::new().unwrap();
        let active = ActiveSession::open(session_dir.path().to_path_buf()).unwrap();
        active.push_turn("user", "first message");
        // Spread turns by 2 ms each so the working-layer sort orders
        // them by `Turn.at` rather than falling back to the content-
        // hash tiebreaker (Turn lacks a stable id, so ties on the
        // timestamp axis aren't deterministic by content).
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        active.push_turn("assistant", "reply");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        active.push_turn("user", "follow up");

        let o = o.with_active_session(active);
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(ctx.working.len(), 3);
        // Newest first per the assembly contract.
        assert_eq!(ctx.working[0].content, "follow up");
        assert_eq!(ctx.working[2].content, "first message");
    }

    #[tokio::test]
    async fn build_context_with_no_session_emits_empty_working() {
        let (o, _tmp, _a) = fresh_orchestrator();
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert!(ctx.working.is_empty());
    }

    #[tokio::test]
    async fn build_context_working_caps_to_overfetch() {
        use crate::memory::working::ActiveSession;
        use tempfile::TempDir;

        let (o, _tmp, _a) = fresh_orchestrator();
        let session_dir = TempDir::new().unwrap();
        let active = ActiveSession::open(session_dir.path().to_path_buf()).unwrap();
        // Push more than WORKING_FETCH (32) so the overfetch cap is exercised.
        for i in 0..50 {
            active.push_turn("user", format!("turn {i}"));
        }
        let o = o.with_active_session(active);
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert!(
            ctx.working.len() <= WORKING_FETCH,
            "expected at most {WORKING_FETCH} turns, got {}",
            ctx.working.len()
        );
        // The most recent turn should be present.
        assert!(
            ctx.working.iter().any(|t| t.content == "turn 49"),
            "newest turn missing from working"
        );
    }

    /// Determinism: same inputs + same DB state ⇒ byte-identical context.
    #[tokio::test]
    async fn build_context_is_deterministic() {
        let (o, _tmp, a) = fresh_orchestrator();
        for i in 0..5 {
            a.procedural
                .pin(format!("p{i}"), vec![], "personal".into())
                .await
                .unwrap();
            a.episodic
                .record("k", "personal", &format!("\"e{i}\""))
                .await
                .unwrap();
        }

        let ctx1 = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        let ctx2 = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();

        let to_text = |c: &AssembledContext| -> String {
            let mut s = String::new();
            for p in &c.procedural {
                s.push_str(&p.id.to_string());
                s.push('|');
                s.push_str(&p.content);
                s.push('\n');
            }
            for e in &c.episodic {
                s.push_str(&e.id.to_string());
                s.push('|');
                s.push_str(&e.payload);
                s.push('\n');
            }
            s
        };
        assert_eq!(to_text(&ctx1), to_text(&ctx2));
    }

    // ---------- 1.6: scope sets, pins, archived, injected clock ----------

    fn scopes_of<T>(rows: &[T], scope: impl Fn(&T) -> &str) -> Vec<String> {
        let mut out: Vec<String> = rows.iter().map(|r| scope(r).to_owned()).collect();
        out.sort();
        out
    }

    async fn seed_scopes(a: &Arcs) {
        for scope in ["global", "a", "b", "c"] {
            a.procedural
                .pin(format!("rule {scope}"), vec![], scope.into())
                .await
                .unwrap();
            a.episodic
                .record("decision", scope, &format!("\"event in {scope}\""))
                .await
                .unwrap();
            a.semantic
                .remember(
                    &format!("shared topic {scope}"),
                    MemoryKind::Fact,
                    vec![],
                    scope.into(),
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn scope_set_filters_l3_l4_and_adds_global_pins() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_scopes(&a).await;
        let req = ContextRequest {
            query: Some("shared topic".into()),
            scope: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        let ctx = o
            .build_context_with(&req, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(
            scopes_of(&ctx.procedural, |p| &p.scope),
            ["a", "b", "global"]
        );
        assert_eq!(scopes_of(&ctx.episodic, |e| &e.scope), ["a", "b"]);
        assert_eq!(scopes_of(&ctx.semantic, |h| &h.item.scope), ["a", "b"]);
    }

    #[tokio::test]
    async fn single_scope_gets_global_pins_unless_pin_scopes_is_empty() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_scopes(&a).await;
        let ctx = o
            .build_context(None, Some("a"), TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(scopes_of(&ctx.procedural, |p| &p.scope), ["a", "global"]);
        assert_eq!(scopes_of(&ctx.episodic, |e| &e.scope), ["a"]);

        let old = o.with_auto_context(AutoContextConfig {
            pin_scopes: vec![],
            ..Default::default()
        });
        let ctx = old
            .build_context(None, Some("a"), TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(scopes_of(&ctx.procedural, |p| &p.scope), ["a"]);
    }

    #[tokio::test]
    async fn no_scope_returns_every_pin_once() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_scopes(&a).await;
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(
            scopes_of(&ctx.procedural, |p| &p.scope),
            ["a", "b", "c", "global"]
        );
        // Asking for the pin scope itself does not duplicate its pins.
        let ctx = o
            .build_context(None, Some("global"), TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(scopes_of(&ctx.procedural, |p| &p.scope), ["global"]);
    }

    #[tokio::test]
    async fn archived_and_superseded_memories_stay_out_of_context() {
        use crate::memory::semantic::MemoryStatus;
        let (o, _tmp, a) = fresh_orchestrator();
        let live = a
            .semantic
            .remember("deploy runbook v3", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let archived = a
            .semantic
            .remember("deploy runbook v1", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let replaced = a
            .semantic
            .remember("deploy runbook v2", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        a.semantic
            .set_status(archived, MemoryStatus::Archived)
            .await
            .unwrap();
        a.semantic.supersede(replaced, live).await.unwrap();

        let ctx = o
            .build_context(Some("deploy runbook"), None, TokenBudget::default())
            .await
            .unwrap();
        let ids: Vec<_> = ctx.semantic.iter().map(|h| h.item.id).collect();
        assert_eq!(ids, vec![live]);
    }

    /// Archived and down-weighted events are dropped before the budget
    /// pass, and enough of them cannot crowd a real event out of the
    /// recency window.
    #[tokio::test]
    async fn archived_and_low_weight_events_are_dropped_server_side() {
        let (o, _tmp, a) = fresh_orchestrator();
        a.episodic
            .record("decision", "p", "{\"content\":\"keep me\"}")
            .await
            .unwrap();
        for i in 0..(EPISODIC_FETCH + 10) {
            a.episodic
                .record_weighted(
                    "note",
                    "p",
                    &format!("{{\"content\":\"hygiene {i}\"}}"),
                    0.3,
                )
                .await
                .unwrap();
        }
        a.episodic
            .record("archived", "p", "{\"content\":\"retired\"}")
            .await
            .unwrap();
        let ctx = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(kinds_of(&ctx), vec!["decision"]);
    }

    /// With the clock injected the whole context is a pure function of
    /// the stores: two builds at the same instant are equal, field for
    /// field, including scores' effect on order.
    #[tokio::test]
    async fn build_context_at_is_reproducible_for_a_fixed_now() {
        let (o, _tmp, a) = fresh_orchestrator();
        seed_scopes(&a).await;
        let req = ContextRequest {
            query: Some("shared topic".into()),
            ..Default::default()
        };
        let now = Utc::now() + chrono::Duration::days(3);
        let first = o
            .build_context_at(&req, TokenBudget::default(), now)
            .await
            .unwrap();
        let second = o
            .build_context_at(&req, TokenBudget::default(), now)
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.semantic.len(), 4);
        assert_eq!(first.episodic.len(), 4);
    }

    /// The episodic index contract is stubbed out (or an index is
    /// simply empty): a query must still get recent events, not none.
    #[tokio::test]
    async fn a_query_with_no_episodic_index_still_returns_recent_events() {
        let (o, _tmp, a) = fresh_orchestrator();
        for i in 0..5 {
            a.episodic
                .record("decision", "p", &format!("\"decision number {i}\""))
                .await
                .unwrap();
        }
        let with_query = o
            .build_context(Some("anything at all"), None, TokenBudget::default())
            .await
            .unwrap();
        let without = o
            .build_context(None, None, TokenBudget::default())
            .await
            .unwrap();
        assert_eq!(with_query.episodic.len(), 5);
        assert_eq!(
            with_query.episodic.iter().map(|e| e.id).collect::<Vec<_>>(),
            without.episodic.iter().map(|e| e.id).collect::<Vec<_>>()
        );
    }

    /// End to end through the stores: a memory created long ago but
    /// edited recently is fresher than one of equal relevance that was
    /// never touched.
    #[tokio::test]
    async fn an_edited_memory_is_scored_on_its_edit_time() {
        let (o, _tmp, a) = fresh_orchestrator();
        // Identical text, so identical similarity and keyword evidence.
        let edited = a
            .semantic
            .remember(
                "status of the migration",
                MemoryKind::Fact,
                vec![],
                "x".into(),
            )
            .await
            .unwrap();
        let untouched = a
            .semantic
            .remember(
                "status of the migration",
                MemoryKind::Fact,
                vec![],
                "y".into(),
            )
            .await
            .unwrap();
        // Read the context 60 days from now; `edited` was revised
        // yesterday relative to that.
        let now = Utc::now() + chrono::Duration::days(60);
        a.semantic
            .backdate_updated_at(edited, now - chrono::Duration::days(1))
            .await
            .unwrap();
        let req = ContextRequest {
            query: Some("status of the migration".into()),
            ..Default::default()
        };
        let ctx = o
            .build_context_at(&req, TokenBudget::default(), now)
            .await
            .unwrap();
        let ids: Vec<_> = ctx.semantic.iter().map(|h| h.item.id).collect();
        assert_eq!(ids, vec![edited, untouched]);
    }
}
