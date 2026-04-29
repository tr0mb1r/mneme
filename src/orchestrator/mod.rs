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
//!   │ L3 episodic     (recent events, ranked by last_accessed)│
//!   │ L4 semantic     (HNSW search if a query is supplied)    │
//!   └────────────────────────────────────────────────────────┘
//! ```
//!
//! L1 (working session) and L5 (reflections) live in `memory::working`
//! / future modules; they are not yet folded in. The plumbing here is
//! deliberately layer-agnostic so dropping them in is a small change.
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
//! swap; the public API doesn't change.

pub mod assembly;
pub mod budget;
pub mod scoring;

use std::sync::Arc;

use crate::Result;
use crate::memory::episodic::{EpisodicStore, RecentFilters};
use crate::memory::procedural::{PinnedItem, ProceduralStore};
use crate::memory::semantic::{RecallFilters, RecallHit, SemanticStore};
use crate::memory::working::{ActiveSession, Turn};

pub use assembly::AssembledContext;
pub use budget::TokenBudget;
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
        }
    }

    /// Builder hook: attach an active session so `build_context` pulls
    /// the L1 working layer (recent turns) into auto-context. Tests
    /// that only exercise L0/L3/L4 skip this.
    pub fn with_active_session(mut self, session: Arc<ActiveSession>) -> Self {
        self.active_session = Some(session);
        self
    }

    /// Assemble a structured context for the agent.
    ///
    /// `query` is `Some(text)` when the caller wants L4 semantic
    /// recall folded in; `None` returns procedural + episodic only.
    /// `scope` filters every layer to a single scope (e.g.
    /// `"personal"`, `"work"`).
    ///
    /// Per spec §5.3, layers are fetched in parallel via
    /// `tokio::join!`; total wall time is the slowest layer plus a
    /// small amount of scoring/assembly overhead. The Phase 5 exit
    /// gate is `mneme://context` p95 < 200 ms.
    pub async fn build_context(
        &self,
        query: Option<&str>,
        scope: Option<&str>,
        budget: TokenBudget,
    ) -> Result<AssembledContext> {
        let scope_owned = scope.map(|s| s.to_owned());

        let proc_fut = self.fetch_procedural(scope_owned.clone());
        let epi_fut = self.fetch_episodic(scope_owned.clone());
        let sem_fut = self.fetch_semantic(query, scope_owned.clone());
        let work_fut = self.fetch_working();

        let (proc_res, epi_res, sem_res, work_res) =
            tokio::join!(proc_fut, epi_fut, sem_fut, work_fut);
        let procedural = proc_res?;
        let episodic = epi_res?;
        let semantic = sem_res?;
        let working = work_res?;

        Ok(assembly::assemble(
            procedural, semantic, episodic, working, &budget,
        ))
    }

    async fn fetch_procedural(&self, scope: Option<String>) -> Result<Vec<PinnedItem>> {
        // ProceduralStore::list is sync; wrap in async so the join!
        // call site reads uniformly.
        let mut items = self.procedural.list(scope.as_deref())?;
        items.truncate(PROCEDURAL_FETCH);
        Ok(items)
    }

    async fn fetch_episodic(
        &self,
        scope: Option<String>,
    ) -> Result<Vec<crate::memory::episodic::EpisodicEvent>> {
        let filters = RecentFilters { scope, kind: None };
        self.episodic.recall_recent(&filters, EPISODIC_FETCH).await
    }

    async fn fetch_semantic(
        &self,
        query: Option<&str>,
        scope: Option<String>,
    ) -> Result<Vec<RecallHit>> {
        match query {
            None => Ok(Vec::new()),
            Some(q) => {
                let filters = RecallFilters { scope, kind: None };
                self.semantic.recall(q, SEMANTIC_FETCH, &filters).await
            }
        }
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
            .record("tool_call", "personal", "\"git status\"")
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
        a.episodic
            .record("tool_call", "work", "\"a\"")
            .await
            .unwrap();
        a.episodic
            .record("tool_call", "personal", "\"b\"")
            .await
            .unwrap();

        let ctx = o
            .build_context(None, Some("work"), TokenBudget::default())
            .await
            .unwrap();
        assert!(ctx.procedural.iter().all(|p| p.scope == "work"));
        assert!(ctx.episodic.iter().all(|e| e.scope == "work"));
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
}
