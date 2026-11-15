//! `recall` across layers: L4 semantic memories and, on request, the
//! keyword-indexed L3 events, ranked on one scale.
//!
//! L4 is found by meaning and by words ([`SemanticStore::recall`]). L3
//! has no vectors (ADR-0007), so an event is found by its words only
//! ([`EpisodicStore::search_tiered`]). A decision recorded with
//! `record_event` and never promoted to a memory used to be invisible
//! to `recall`; with `layers = ["semantic", "episodic"]` it is a row in
//! the same list.
//!
//! # One scale
//!
//! An L4 row ranks at its cosine similarity plus its keyword evidence.
//! An L3 row has no similarity, so its rank is built from the evidence
//! alone and scaled to sit where a comparable memory would:
//!
//! ```text
//! rank = W_EPI_RECALL × (A × coverage² + PHRASE) × retrieval_weight
//! ```
//!
//! `coverage` is the IDF-weighted share of the query the event contains
//! ([`KeywordHit::coverage`](super::keyword::KeywordHit)); squared, as
//! in L4's hybrid ranking, so sharing a common word or two counts for
//! almost nothing. `A` ([`EPI_COVERAGE_WEIGHT`]) is set so that an event
//! containing every query term ranks like a memory at cosine 0.65: a
//! solid match, below a near-verbatim memory, above a merely related
//! one. `W_EPI_RECALL` is the general discount of an event against a
//! curated memory.
//!
//! Events of kind `archived`, and events the writer weighted at
//! [`LOW_WEIGHT`] or less, are demoted (rank × [`EPI_DEMOTE_FACTOR`]),
//! not hidden: they are still returned when little else matches.
//!
//! The default is L4 only, so a caller that does not ask for `episodic`
//! gets exactly the rows and order it got before.

use chrono::{DateTime, Utc};

use crate::Result;
use crate::memory::episodic::{
    EpisodicEvent, EpisodicStore, RecentFilters, Tier, WarmRead, indexable_text,
};
use crate::memory::keyword;
pub use crate::memory::semantic::TimeField;
use crate::memory::semantic::{RecallFilters, RecallHit, RecallMode, SemanticStore};

/// Discount of an L3 event against an L4 memory with the same evidence.
pub const W_EPI_RECALL: f32 = 0.8;

/// `A` in the rank formula. `W_EPI_RECALL × A = 0.65`: full query
/// coverage, no phrase bonus, weight 1.0 ranks like a memory at cosine
/// similarity 0.65.
pub const EPI_COVERAGE_WEIGHT: f32 = 0.8125;

/// Added (before the layer discount) when the event contains the query
/// as a contiguous run of whole words. Same size as L4's phrase bonus.
pub const EPI_PHRASE_WEIGHT: f32 = 0.1;

/// Events covering less of the query than this are not returned: below
/// it the overlap is stopwords.
pub const EPI_MIN_COVERAGE: f32 = 0.25;

/// `retrieval_weight` at or below which an event counts as low-signal
/// and is demoted.
pub const LOW_WEIGHT: f32 = 0.3;

/// Rank multiplier for demoted events (`archived` kind, low weight).
pub const EPI_DEMOTE_FACTOR: f32 = 0.5;

/// The kind `record_event` uses for "this is no longer current".
pub const ARCHIVED_KIND: &str = "archived";

/// How many lexical candidates are read back from storage per recall.
const EPI_FETCH: usize = 200;

/// Which layers a recall covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layers {
    pub semantic: bool,
    pub episodic: bool,
}

impl Default for Layers {
    /// L4 only: the behaviour of every release before 1.6.
    fn default() -> Self {
        Self {
            semantic: true,
            episodic: false,
        }
    }
}

impl Layers {
    /// Both layers.
    pub fn both() -> Self {
        Self {
            semantic: true,
            episodic: true,
        }
    }

    /// Parse layer names (`"semantic"`, `"episodic"`). `Err` carries the
    /// first name that is neither; an empty list is `Err("")`.
    pub fn parse<'a>(
        names: impl IntoIterator<Item = &'a str>,
    ) -> std::result::Result<Self, String> {
        let mut layers = Self {
            semantic: false,
            episodic: false,
        };
        for name in names {
            match name {
                "semantic" => layers.semantic = true,
                "episodic" => layers.episodic = true,
                other => return Err(other.to_owned()),
            }
        }
        if layers.semantic || layers.episodic {
            Ok(layers)
        } else {
            Err(String::new())
        }
    }
}

/// Everything `recall` can be asked for.
#[derive(Debug, Clone, Default)]
pub struct RecallRequest {
    /// Kind, tags, similarity floor, mode, superseded. Its `scope` is
    /// ignored in favour of [`scopes`](Self::scopes).
    pub filters: RecallFilters,
    pub layers: Layers,
    /// A row must be in one of these scopes. Empty: any scope.
    pub scopes: Vec<String>,
    /// Half-open `[since, until)` window on [`time_field`](Self::time_field).
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub time_field: TimeField,
    /// Also return L4 memories whose status is archived.
    pub include_archived: bool,
}

/// One L3 row of a mixed recall.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodicHit {
    pub event: EpisodicEvent,
    pub tier: Tier,
    /// The event's indexed text (`payload.content` / `payload.text` and
    /// tags): what the query was matched against.
    pub text: String,
    pub coverage: f32,
    /// The event contains the query as a whole-word phrase.
    pub phrase: bool,
    /// `archived` kind or low `retrieval_weight`: rank was halved.
    pub demoted: bool,
    pub rank: f32,
}

/// One row of a recall, from either layer.
#[derive(Debug, Clone, PartialEq)]
pub enum UnifiedHit {
    Semantic { hit: RecallHit, rank: f32 },
    Episodic(EpisodicHit),
}

impl UnifiedHit {
    pub fn rank(&self) -> f32 {
        match self {
            Self::Semantic { rank, .. } => *rank,
            Self::Episodic(e) => e.rank,
        }
    }
}

/// Rank of an L3 event on the L4 scale; see the module docs.
pub fn episodic_rank(coverage: f32, phrase: bool, retrieval_weight: f32, demoted: bool) -> f32 {
    let evidence =
        EPI_COVERAGE_WEIGHT * coverage * coverage + if phrase { EPI_PHRASE_WEIGHT } else { 0.0 };
    let rank = W_EPI_RECALL * evidence * retrieval_weight;
    if demoted {
        rank * EPI_DEMOTE_FACTOR
    } else {
        rank
    }
}

/// Whether an event is demoted in mixed recall.
pub fn is_demoted(event: &EpisodicEvent) -> bool {
    event.kind == ARCHIVED_KIND || event.retrieval_weight <= LOW_WEIGHT
}

/// Rank of an L4 hit on the shared scale: its similarity plus the
/// keyword evidence hybrid recall found ([`RecallHit::rank`]).
pub fn semantic_rank(hit: &RecallHit) -> f32 {
    hit.rank
}

/// Recall across the layers `req` names, best first, at most `k`.
///
/// `episodic` may be `None` when `req.layers.episodic` is false.
pub async fn recall(
    semantic: &SemanticStore,
    episodic: Option<&EpisodicStore>,
    query: &str,
    k: usize,
    req: &RecallRequest,
) -> Result<Vec<UnifiedHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let l4 = if req.layers.semantic {
        semantic_hits(semantic, query, k, req).await?
    } else {
        Vec::new()
    };
    // L3 has no vectors: `mode = "semantic"` (meaning only) has nothing
    // to ask it.
    let l3 = match episodic {
        Some(store) if req.layers.episodic && req.filters.mode != RecallMode::Semantic => {
            episodic_hits(store, query, k, req).await?
        }
        _ => Vec::new(),
    };
    Ok(interleave(l4, l3, k))
}

/// The L4 half: one `SemanticStore::recall` under the request's scopes,
/// time window and archive flag. The store applies them inside its own
/// widening probe, so a selective window still fills `k`.
async fn semantic_hits(
    store: &SemanticStore,
    query: &str,
    k: usize,
    req: &RecallRequest,
) -> Result<Vec<RecallHit>> {
    let filters = RecallFilters {
        // `RecallRequest::scopes` replaces the single-scope field.
        scope: None,
        scopes: req.scopes.clone(),
        since: req.since,
        until: req.until,
        time_field: req.time_field,
        include_archived: req.include_archived,
        ..req.filters.clone()
    };
    store.recall(query, k, &filters).await
}

/// The L3 half: lexical matches under the request's filters, ranked.
async fn episodic_hits(
    store: &EpisodicStore,
    query: &str,
    k: usize,
    req: &RecallRequest,
) -> Result<Vec<EpisodicHit>> {
    let filters = RecentFilters {
        scopes: req.scopes.clone(),
        // `recall`'s kind filter names L4 kinds; an event of the same
        // name (`decision`) passes, others do not.
        kind: req.filters.kind.map(|kind| kind.as_str().to_owned()),
        tags: req.filters.tags.clone(),
        since: req.since,
        until: req.until,
        warm: WarmRead::Always,
        ..Default::default()
    };
    let query_words = keyword::words(query);
    let mut hits: Vec<EpisodicHit> = store
        .search_tiered(query, EPI_FETCH, &filters)
        .await?
        .into_iter()
        .filter(|h| h.coverage >= EPI_MIN_COVERAGE)
        .map(|h| {
            let text = indexable_text(&h.event);
            let phrase = keyword::contains_phrase(&keyword::words(&text), &query_words);
            let demoted = is_demoted(&h.event);
            let rank = episodic_rank(h.coverage, phrase, h.event.retrieval_weight, demoted);
            EpisodicHit {
                event: h.event,
                tier: h.tier,
                text,
                coverage: h.coverage,
                phrase,
                demoted,
                rank,
            }
        })
        // The rank is on the cosine scale, so the caller's similarity
        // floor means the same thing for an event as for a memory.
        .filter(|h| {
            req.filters
                .min_similarity
                .is_none_or(|floor| h.rank >= floor)
        })
        .collect();
    hits.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.event.created_at.cmp(&a.event.created_at))
            .then_with(|| b.event.id.cmp(&a.event.id))
    });
    hits.truncate(k);
    Ok(hits)
}

/// Merge the two best-first lists by rank into one of at most `k`.
///
/// Each list keeps its own order: a row is only ever compared with the
/// head of the other list. So with no L3 rows the result is the L4 list
/// untouched, and L4 rows never change places with each other whatever
/// the ranks say. Superseded memories stay last, after every event,
/// and only fill leftover room, as in an L4-only recall.
fn interleave(l4: Vec<RecallHit>, l3: Vec<EpisodicHit>, k: usize) -> Vec<UnifiedHit> {
    let (current, stale): (Vec<RecallHit>, Vec<RecallHit>) =
        l4.into_iter().partition(|h| h.superseded_by.is_none());
    let wrap = |hit: RecallHit| {
        let rank = semantic_rank(&hit);
        UnifiedHit::Semantic { hit, rank }
    };
    let mut out = Vec::with_capacity(k);
    let mut current = current.into_iter().peekable();
    let mut l3 = l3.into_iter().peekable();
    while out.len() < k {
        let take_event = match (current.peek(), l3.peek()) {
            (Some(memory), Some(event)) => event.rank > semantic_rank(memory),
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => break,
        };
        if take_event {
            out.extend(l3.next().map(UnifiedHit::Episodic));
        } else {
            out.extend(current.next().map(wrap));
        }
    }
    let room = k.saturating_sub(out.len());
    out.extend(stale.into_iter().take(room).map(wrap));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::MemoryKind;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Fixture {
        semantic: Arc<SemanticStore>,
        episodic: EpisodicStore,
        _tmp: TempDir,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(8));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        Fixture {
            semantic,
            episodic: EpisodicStore::new(storage),
            _tmp: tmp,
        }
    }

    fn both() -> RecallRequest {
        RecallRequest {
            layers: Layers::both(),
            ..Default::default()
        }
    }

    fn event_ids(hits: &[UnifiedHit]) -> Vec<crate::ids::EventId> {
        hits.iter()
            .filter_map(|h| match h {
                UnifiedHit::Episodic(e) => Some(e.event.id),
                UnifiedHit::Semantic { .. } => None,
            })
            .collect()
    }

    fn memory_ids(hits: &[UnifiedHit]) -> Vec<crate::ids::MemoryId> {
        hits.iter()
            .filter_map(|h| match h {
                UnifiedHit::Semantic { hit, .. } => Some(hit.item.id),
                UnifiedHit::Episodic(_) => None,
            })
            .collect()
    }

    #[test]
    fn full_coverage_ranks_like_a_cosine_065_memory() {
        let rank = episodic_rank(1.0, false, 1.0, false);
        assert!((rank - 0.65).abs() < 1e-6, "{rank}");
        // A phrase lifts it, never past a near-verbatim memory.
        let phrase = episodic_rank(1.0, true, 1.0, false);
        assert!(phrase > rank && phrase < 0.8);
        // Squared coverage: half the query is a quarter of the evidence.
        assert!((episodic_rank(0.5, false, 1.0, false) - 0.1625).abs() < 1e-6);
        // Weight scales, demotion halves.
        assert!((episodic_rank(1.0, false, 0.5, false) - 0.325).abs() < 1e-6);
        assert!((episodic_rank(1.0, false, 1.0, true) - 0.325).abs() < 1e-6);
        assert_eq!(episodic_rank(0.0, false, 1.0, false), 0.0);
    }

    #[test]
    fn layers_parse() {
        assert_eq!(Layers::parse(["semantic"]).unwrap(), Layers::default());
        assert_eq!(
            Layers::parse(["episodic", "semantic"]).unwrap(),
            Layers::both()
        );
        let only_l3 = Layers::parse(["episodic"]).unwrap();
        assert!(only_l3.episodic && !only_l3.semantic);
        assert_eq!(Layers::parse(["procedural"]).unwrap_err(), "procedural");
        assert_eq!(Layers::parse([]).unwrap_err(), "");
    }

    /// The reason the workstream exists: a decision that was only ever
    /// recorded as an event is found by `recall` once L3 is asked.
    #[tokio::test]
    async fn both_layers_surface_an_event_that_semantic_only_misses() {
        let f = fixture();
        f.semantic
            .remember(
                "the cat is called Murka",
                MemoryKind::Fact,
                vec![],
                "p".into(),
            )
            .await
            .unwrap();
        let decision = f
            .episodic
            .record_json(
                "decision",
                "p",
                &serde_json::json!({"content": "switched the nightly backtest to live_bt.py"}),
            )
            .await
            .unwrap();

        let semantic_only = recall(
            &f.semantic,
            Some(&f.episodic),
            "live_bt.py",
            5,
            &RecallRequest::default(),
        )
        .await
        .unwrap();
        assert!(event_ids(&semantic_only).is_empty());

        let mixed = recall(&f.semantic, Some(&f.episodic), "live_bt.py", 5, &both())
            .await
            .unwrap();
        assert_eq!(event_ids(&mixed), vec![decision]);
        let UnifiedHit::Episodic(hit) = mixed
            .iter()
            .find(|h| matches!(h, UnifiedHit::Episodic(_)))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(hit.coverage, 1.0);
        assert!(hit.phrase);
        assert!(!hit.demoted);
        assert_eq!(hit.text, "switched the nightly backtest to live_bt.py");
    }

    /// Asking for L3 as well must not disturb a query L4 answers alone.
    #[tokio::test]
    async fn l4_only_query_keeps_its_top_five_when_l3_is_added() {
        let f = fixture();
        for i in 0..12 {
            f.semantic
                .remember(
                    &format!("gateway note {i}: port 40{i:02} belongs to the paper account"),
                    MemoryKind::Fact,
                    vec![],
                    "p".into(),
                )
                .await
                .unwrap();
        }
        // Events exist, but none shares a word with the query.
        for text in ["lunch was fine", "walked the dog", "rain all morning"] {
            f.episodic
                .record_json("observation", "p", &serde_json::json!({ "content": text }))
                .await
                .unwrap();
        }
        let query = "which port belongs to the paper account";
        let before = recall(&f.semantic, None, query, 5, &RecallRequest::default())
            .await
            .unwrap();
        let after = recall(&f.semantic, Some(&f.episodic), query, 5, &both())
            .await
            .unwrap();
        assert_eq!(before.len(), 5);
        assert_eq!(memory_ids(&after), memory_ids(&before));
        assert!(event_ids(&after).is_empty());
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn unindexed_kinds_and_weak_overlap_are_not_returned() {
        let f = fixture();
        // `tool_call` is not in index_kinds.
        f.episodic
            .record_json(
                "tool_call",
                "p",
                &serde_json::json!({"content": "remember live_bt.py"}),
            )
            .await
            .unwrap();
        // Shares only a common word with the query.
        for i in 0..6 {
            f.episodic
                .record_json(
                    "observation",
                    "p",
                    &serde_json::json!({"content": format!("the build {i} was slow")}),
                )
                .await
                .unwrap();
        }
        let hits = recall(
            &f.semantic,
            Some(&f.episodic),
            "the live_bt.py schedule",
            10,
            &both(),
        )
        .await
        .unwrap();
        assert!(event_ids(&hits).is_empty(), "{hits:?}");
    }

    #[tokio::test]
    async fn archived_and_low_weight_events_are_demoted_not_hidden() {
        let f = fixture();
        let payload =
            serde_json::json!({"content": "rotate the gateway token monthly"}).to_string();
        let plain = f
            .episodic
            .record_full("decision", "p", &payload, vec![], 1.0)
            .await
            .unwrap();
        let archived = f
            .episodic
            .record_full("archived", "p", &payload, vec![], 1.0)
            .await
            .unwrap();
        let quiet = f
            .episodic
            .record_full("decision", "p", &payload, vec![], 0.3)
            .await
            .unwrap();
        let req = RecallRequest {
            layers: Layers::parse(["episodic"]).unwrap(),
            ..Default::default()
        };
        let hits = recall(&f.semantic, Some(&f.episodic), "gateway token", 10, &req)
            .await
            .unwrap();
        assert_eq!(event_ids(&hits), vec![plain, archived, quiet]);
        let ranks: Vec<f32> = hits.iter().map(UnifiedHit::rank).collect();
        assert!((ranks[1] - ranks[0] * EPI_DEMOTE_FACTOR).abs() < 1e-6);
        assert!((ranks[2] - ranks[0] * 0.3 * EPI_DEMOTE_FACTOR).abs() < 1e-6);
    }

    #[tokio::test]
    async fn filters_apply_to_events() {
        let f = fixture();
        let content = serde_json::json!({"content": "moved the deploy to Thursday"}).to_string();
        let work = f
            .episodic
            .record_full("decision", "work", &content, vec!["ops".into()], 1.0)
            .await
            .unwrap();
        let home = f
            .episodic
            .record_full("milestone", "home", &content, vec![], 1.0)
            .await
            .unwrap();
        let run = |req: RecallRequest| {
            let f = &f;
            async move {
                event_ids(
                    &recall(&f.semantic, Some(&f.episodic), "deploy Thursday", 10, &req)
                        .await
                        .unwrap(),
                )
            }
        };

        // Ids minted in the same millisecond are not ordered; compare
        // as sets.
        let mut all = run(both()).await;
        all.sort();
        let mut want = vec![work, home];
        want.sort();
        assert_eq!(all, want);

        // Scopes: one, then both.
        let mut req = both();
        req.scopes = vec!["work".into()];
        assert_eq!(run(req).await, vec![work]);
        let mut req = both();
        req.scopes = vec!["work".into(), "home".into()];
        assert_eq!(run(req).await.len(), 2);

        // Tags are AND, as for memories.
        let mut req = both();
        req.filters.tags = vec!["ops".into()];
        assert_eq!(run(req).await, vec![work]);

        // The kind filter names an L4 kind; `decision` events pass.
        let mut req = both();
        req.filters.kind = Some(MemoryKind::Decision);
        assert_eq!(run(req).await, vec![work]);

        // Meaning-only mode has nothing to ask L3.
        let mut req = both();
        req.filters.mode = RecallMode::Semantic;
        assert!(run(req).await.is_empty());

        // The similarity floor reads the event's rank.
        let mut req = both();
        req.filters.min_similarity = Some(0.9);
        assert!(run(req).await.is_empty());
        let mut req = both();
        req.filters.min_similarity = Some(0.5);
        assert_eq!(run(req).await.len(), 2);
    }

    #[tokio::test]
    async fn since_and_until_bound_both_layers() {
        let f = fixture();
        f.semantic
            .remember(
                "the release train leaves on Tuesday",
                MemoryKind::Fact,
                vec![],
                "p".into(),
            )
            .await
            .unwrap();
        f.episodic
            .record_json(
                "decision",
                "p",
                &serde_json::json!({"content": "release train moved to Tuesday"}),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mid = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let late_memory = f
            .semantic
            .remember(
                "the release train now leaves on Friday",
                MemoryKind::Fact,
                vec![],
                "p".into(),
            )
            .await
            .unwrap();
        let late_event = f
            .episodic
            .record_json(
                "decision",
                "p",
                &serde_json::json!({"content": "release train moved to Friday"}),
            )
            .await
            .unwrap();

        let query = "release train";
        let all = recall(&f.semantic, Some(&f.episodic), query, 10, &both())
            .await
            .unwrap();
        assert_eq!(memory_ids(&all).len(), 2);
        assert_eq!(event_ids(&all).len(), 2);

        let mut req = both();
        req.since = Some(mid);
        let after = recall(&f.semantic, Some(&f.episodic), query, 10, &req)
            .await
            .unwrap();
        assert_eq!(memory_ids(&after), vec![late_memory]);
        assert_eq!(event_ids(&after), vec![late_event]);

        let mut req = both();
        req.until = Some(mid);
        let before = recall(&f.semantic, Some(&f.episodic), query, 10, &req)
            .await
            .unwrap();
        assert_eq!(memory_ids(&before).len(), 1);
        assert_ne!(memory_ids(&before), vec![late_memory]);
        assert_eq!(event_ids(&before).len(), 1);
        assert_ne!(event_ids(&before), vec![late_event]);

        // An empty window is empty, not an error.
        let mut req = both();
        req.until = Some(mid - chrono::Duration::days(1));
        assert!(
            recall(&f.semantic, Some(&f.episodic), query, 10, &req)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The window is applied after the search, so the search is widened
    /// until the window is filled: one old memory among many new ones
    /// is still found with `limit = 1`.
    #[tokio::test]
    async fn time_window_widens_the_l4_probe() {
        let f = fixture();
        let old = f
            .semantic
            .remember("alpha note zero", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let cut = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        for i in 1..40 {
            f.semantic
                .remember(
                    &format!("alpha note {i}"),
                    MemoryKind::Fact,
                    vec![],
                    "p".into(),
                )
                .await
                .unwrap();
        }
        let req = RecallRequest {
            until: Some(cut),
            ..Default::default()
        };
        let hits = recall(&f.semantic, None, "alpha note", 1, &req)
            .await
            .unwrap();
        assert_eq!(memory_ids(&hits), vec![old]);
    }

    #[tokio::test]
    async fn multiple_scopes_cover_l4() {
        let f = fixture();
        let mut ids = Vec::new();
        for scope in ["work", "home", "other"] {
            ids.push(
                f.semantic
                    .remember(
                        &format!("the {scope} wifi password hint"),
                        MemoryKind::Fact,
                        vec![],
                        scope.into(),
                    )
                    .await
                    .unwrap(),
            );
        }
        let req = RecallRequest {
            scopes: vec!["work".into(), "home".into()],
            ..Default::default()
        };
        let mut got = memory_ids(
            &recall(&f.semantic, None, "wifi password", 10, &req)
                .await
                .unwrap(),
        );
        got.sort();
        let mut want = vec![ids[0], ids[1]];
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn limit_caps_the_merged_list_and_zero_is_empty() {
        let f = fixture();
        for i in 0..4 {
            f.semantic
                .remember(
                    &format!("backup rotation note {i}"),
                    MemoryKind::Fact,
                    vec![],
                    "p".into(),
                )
                .await
                .unwrap();
            f.episodic
                .record_json(
                    "lesson",
                    "p",
                    &serde_json::json!({"content": format!("backup rotation lesson {i}")}),
                )
                .await
                .unwrap();
        }
        let hits = recall(
            &f.semantic,
            Some(&f.episodic),
            "backup rotation",
            3,
            &both(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 3);
        // Best first across layers.
        assert!(hits.windows(2).all(|w| {
            // L4 rows keep their own order, so only compare across layers.
            !matches!(
                (&w[0], &w[1]),
                (UnifiedHit::Semantic { .. }, UnifiedHit::Episodic(_))
            ) || w[0].rank() >= w[1].rank()
        }));
        assert!(
            recall(&f.semantic, Some(&f.episodic), "backup", 0, &both())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
