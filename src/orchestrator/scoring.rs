//! Scoring + ranking used by the orchestrator to trim per-layer
//! candidates down to a single budget-respecting context.
//!
//! Every score is `relevance × layer_weight × time factor`, but the
//! layers disagree on what "relevance" and "time" mean:
//!
//! * **Semantic (L4)**: `rank × 0.7 × freshness`.
//!   `rank` is [`RecallHit::rank`], similarity plus keyword evidence.
//!   `freshness = max(floor, 2^(−age(updated_at) / half_life))` with
//!   `[auto_context] semantic_half_life_days` (90; `0` = no decay) and
//!   `semantic_recency_floor` (0.75). A long-term fact does not stop
//!   being true after a fortnight, so age only breaks near-ties, and
//!   it is measured from the last *edit*: a memory maintained in place
//!   is as fresh as its latest revision.
//! * **Episodic (L3)**: `weight × 0.8 × recency × (β + (1−β)·lex)`.
//!   `recency` keeps the 14-day half-life on `last_accessed`. `lex` is
//!   the query's lexical coverage of the event in `[0, 1]`; `β` is
//!   `[auto_context] episodic_query_mix` (0.3), the share of the score
//!   an off-topic event keeps. With no query `lex` is 1, i.e. pure
//!   recency.
//! * **Working (L1)** and **procedural (L0)**: unchanged, `0.9 ×
//!   recency` and a flat `1.0`.
//!
//! Until 1.6 semantic rows also decayed on a 14-day half-life from
//! `created_at`, which put a six-week-old current-state memory at a
//! score of 0.05 against 0.8 for any fresh event. Scores are still not
//! directly comparable across layers; the layer shares in
//! [`super::budget::LayerShares`] are what keep one layer from taking
//! the whole budget.
//!
//! All functions are pure: `now` and the config are passed in.

use chrono::{DateTime, Utc};

use crate::config::AutoContextConfig;
use crate::memory::episodic::EpisodicEvent;
use crate::memory::procedural::PinnedItem;
use crate::memory::semantic::RecallHit;
use crate::memory::working::Turn;

/// Layer weights. Higher = ranks higher at equal semantic + recency
/// score. Sum doesn't have to equal 1; only relative magnitudes
/// matter for ranking.
pub const PROCEDURAL_WEIGHT: f32 = 1.0;
/// Working turns are the active session's most recent context.
/// Below procedural (anchors) but above episodic (history) and
/// semantic (long-term recall) — they're the immediate
/// conversation, freshest available.
pub const WORKING_WEIGHT: f32 = 0.9;
pub const EPISODIC_WEIGHT: f32 = 0.8;
pub const SEMANTIC_WEIGHT: f32 = 0.7;

/// Half-life for the recency decay of episodic events and working
/// turns. Matches the spec's hot tier (≤28 days): at 14 days the
/// factor halves, at 28 days it's ~25%. Semantic rows use
/// `[auto_context] semantic_half_life_days` instead.
pub const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

/// One scored row. The originating layer is captured in the variant
/// so the assembly pass can keep per-layer minimums honest.
#[derive(Debug, Clone)]
pub enum ScoredItem {
    Procedural { item: PinnedItem, score: f32 },
    Semantic { hit: RecallHit, score: f32 },
    Episodic { event: EpisodicEvent, score: f32 },
    Working { turn: Turn, score: f32 },
}

impl ScoredItem {
    pub fn score(&self) -> f32 {
        match self {
            ScoredItem::Procedural { score, .. } => *score,
            ScoredItem::Semantic { score, .. } => *score,
            ScoredItem::Episodic { score, .. } => *score,
            ScoredItem::Working { score, .. } => *score,
        }
    }
}

/// Score a procedural item. Procedural rows have no semantic match
/// and no decay (they're "always on"), so the score collapses to
/// the layer weight.
pub fn score_procedural(item: PinnedItem) -> ScoredItem {
    ScoredItem::Procedural {
        item,
        score: PROCEDURAL_WEIGHT,
    }
}

/// Score a semantic recall hit: `rank × SEMANTIC_WEIGHT × freshness`.
pub fn score_semantic(hit: RecallHit, now: DateTime<Utc>, cfg: &AutoContextConfig) -> ScoredItem {
    let score = semantic_score(&hit, now, cfg);
    ScoredItem::Semantic { hit, score }
}

/// The number behind [`score_semantic`], without taking the hit.
pub fn semantic_score(hit: &RecallHit, now: DateTime<Utc>, cfg: &AutoContextConfig) -> f32 {
    hit.rank.max(0.0) * SEMANTIC_WEIGHT * semantic_freshness(hit.updated_at, now, cfg)
}

/// `max(floor, 2^(−age(updated_at) / half_life))`, in `[floor, 1]`.
/// A half-life of zero (or less) turns decay off and returns 1.
pub fn semantic_freshness(
    updated_at: DateTime<Utc>,
    now: DateTime<Utc>,
    cfg: &AutoContextConfig,
) -> f32 {
    let half_life = f64::from(cfg.semantic_half_life_days);
    if half_life <= 0.0 || !half_life.is_finite() {
        return 1.0;
    }
    let floor = cfg.semantic_recency_floor.clamp(0.0, 1.0);
    (half_life_factor(updated_at, now, half_life) as f32).max(floor)
}

/// Score an episodic event:
/// `weight × EPISODIC_WEIGHT × recency × (β + (1−β)·lex)`.
///
/// `lex` is the query's lexical coverage of this event. `None` means
/// the read carried no query, and scores as full coverage so the
/// ranking is recency alone. With a query, an event the index does not
/// know (or that shares nothing with the query) is `Some(0.0)` and
/// keeps the `β` share of its recency score, so an absent or empty
/// index degrades to "recent events, scaled down" rather than to
/// nothing.
pub fn score_episodic(
    event: EpisodicEvent,
    now: DateTime<Utc>,
    lex: Option<f32>,
    cfg: &AutoContextConfig,
) -> ScoredItem {
    let score = episodic_score(&event, now, lex, cfg);
    ScoredItem::Episodic { event, score }
}

/// The number behind [`score_episodic`], without taking the event.
pub fn episodic_score(
    event: &EpisodicEvent,
    now: DateTime<Utc>,
    lex: Option<f32>,
    cfg: &AutoContextConfig,
) -> f32 {
    let recency = recency_factor(event.last_accessed, now);
    let beta = f64::from(cfg.episodic_query_mix.clamp(0.0, 1.0));
    let lex = f64::from(lex.unwrap_or(1.0).clamp(0.0, 1.0));
    let weight = f64::from(event.retrieval_weight) * f64::from(EPISODIC_WEIGHT);
    (weight * recency * (beta + (1.0 - beta) * lex)) as f32
}

/// Score a working-session turn. Turns have no semantic match (they
/// aren't embedded) and no per-row weight; the score collapses to
/// `WORKING_WEIGHT * recency_factor(at)`. Recent turns dominate aged
/// ones with the same half-life as everything else.
pub fn score_working(turn: Turn, now: DateTime<Utc>) -> ScoredItem {
    let score = working_score(&turn, now);
    ScoredItem::Working { turn, score }
}

/// The number behind [`score_working`], without taking the turn.
pub fn working_score(turn: &Turn, now: DateTime<Utc>) -> f32 {
    (WORKING_WEIGHT as f64 * recency_factor(turn.at, now)) as f32
}

/// `(1/2)^(elapsed / half_life)` — a true half-life curve (factor =
/// 0.5 exactly at one half-life). Equivalent to
/// `exp(-ln(2) * elapsed / half_life)`; we use the latter form
/// because `f64::exp` is faster than `f64::powf`.
fn recency_factor(ts: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    half_life_factor(ts, now, RECENCY_HALF_LIFE_DAYS)
}

fn half_life_factor(ts: DateTime<Utc>, now: DateTime<Utc>, half_life_days: f64) -> f64 {
    let elapsed_days = (now - ts).num_seconds() as f64 / 86_400.0;
    (-elapsed_days.max(0.0) * std::f64::consts::LN_2 / half_life_days).exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{EventId, MemoryId};
    use crate::memory::episodic::DEFAULT_RETRIEVAL_WEIGHT;
    use crate::memory::semantic::{MemoryItem, MemoryKind};
    use chrono::Duration;

    fn pin(content: &str) -> PinnedItem {
        PinnedItem {
            id: MemoryId::new(),
            content: content.into(),
            tags: vec![],
            scope: "personal".into(),
            created_at: Utc::now(),
        }
    }

    fn cfg() -> AutoContextConfig {
        AutoContextConfig::default()
    }

    /// A fixed instant, so no test depends on the wall clock.
    fn t0() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn hit(distance: f32, age_days: i64) -> RecallHit {
        hit_at(1.0 - distance, age_days, age_days, Utc::now())
    }

    /// A hit with the given similarity, created `created_days` and last
    /// edited `updated_days` before `now`.
    fn hit_at(
        similarity: f32,
        created_days: i64,
        updated_days: i64,
        now: DateTime<Utc>,
    ) -> RecallHit {
        RecallHit {
            item: MemoryItem {
                id: MemoryId::new(),
                content: "x".into(),
                kind: MemoryKind::Fact,
                tags: vec![],
                scope: "personal".into(),
                created_at: now - Duration::days(created_days),
            },
            score: 1.0 - similarity,
            superseded_by: None,
            matched: crate::memory::semantic::MatchSource::Semantic,
            rank: similarity,
            updated_at: now - Duration::days(updated_days),
            status: crate::memory::semantic::MemoryStatus::Active,
        }
    }

    fn event_at(weight: f32, age_days: i64, now: DateTime<Utc>) -> EpisodicEvent {
        let ts = now - Duration::days(age_days);
        EpisodicEvent {
            id: EventId::new(),
            kind: "k".into(),
            scope: "personal".into(),
            payload: "\"x\"".into(),
            tags: vec![],
            retrieval_weight: weight,
            last_accessed: ts,
            created_at: ts,
        }
    }

    fn event(weight: f32, age_days: i64) -> EpisodicEvent {
        let ts = Utc::now() - Duration::days(age_days);
        EpisodicEvent {
            id: EventId::new(),
            kind: "k".into(),
            scope: "personal".into(),
            payload: "\"x\"".into(),
            tags: vec![],
            retrieval_weight: weight,
            last_accessed: ts,
            created_at: ts,
        }
    }

    #[test]
    fn procedural_collapses_to_layer_weight() {
        let s = score_procedural(pin("hello"));
        assert!((s.score() - PROCEDURAL_WEIGHT).abs() < 1e-6);
    }

    #[test]
    fn semantic_close_match_outranks_far_match() {
        let now = Utc::now();
        let close = score_semantic(hit(0.1, 0), now, &cfg());
        let far = score_semantic(hit(0.8, 0), now, &cfg());
        assert!(
            close.score() > far.score(),
            "close={} far={}",
            close.score(),
            far.score()
        );
    }

    #[test]
    fn semantic_recent_outranks_aged_at_same_distance() {
        let now = Utc::now();
        let recent = score_semantic(hit(0.2, 0), now, &cfg());
        let aged = score_semantic(hit(0.2, 60), now, &cfg());
        assert!(recent.score() > aged.score());
    }

    /// Freshness is measured from the last edit, not from creation: a
    /// memory created 60 days ago and revised yesterday beats one of
    /// equal similarity that has sat untouched for 30 days.
    #[test]
    fn edited_yesterday_outranks_a_sim_equal_30_day_old_memory() {
        let now = t0();
        // No floor, so the comparison is on the decay curve itself.
        let no_floor = AutoContextConfig {
            semantic_recency_floor: 0.0,
            ..cfg()
        };
        for c in [cfg(), no_floor] {
            let edited = score_semantic(hit_at(0.6, 60, 1, now), now, &c);
            let untouched = score_semantic(hit_at(0.6, 30, 30, now), now, &c);
            assert!(
                edited.score() > untouched.score(),
                "edited={} untouched={}",
                edited.score(),
                untouched.score()
            );
        }
    }

    #[test]
    fn semantic_freshness_has_a_floor_and_a_90_day_half_life() {
        let now = t0();
        let c = AutoContextConfig {
            semantic_recency_floor: 0.0,
            ..cfg()
        };
        let at = |days| semantic_freshness(now - Duration::days(days), now, &c);
        assert!((at(0) - 1.0).abs() < 1e-6);
        assert!((at(90) - 0.5).abs() < 1e-3);
        assert!((at(180) - 0.25).abs() < 1e-3);
        // Default floor 0.75: nothing drops below it, however old.
        let floored = semantic_freshness(now - Duration::days(3650), now, &cfg());
        assert!((floored - 0.75).abs() < 1e-6);
    }

    #[test]
    fn zero_half_life_turns_semantic_decay_off() {
        let now = t0();
        let c = AutoContextConfig {
            semantic_half_life_days: 0.0,
            ..cfg()
        };
        let old = score_semantic(hit_at(0.5, 900, 900, now), now, &c);
        let new = score_semantic(hit_at(0.5, 0, 0, now), now, &c);
        assert!((old.score() - new.score()).abs() < 1e-6);
        assert!((old.score() - 0.5 * SEMANTIC_WEIGHT).abs() < 1e-6);
    }

    /// The plan's worked example: similarity 0.58, created 41 days ago,
    /// never edited. Under the pre-1.6 formula (14-day half-life from
    /// `created_at`) it scored 0.053; now the floor holds it at
    /// 0.58 × 0.7 × 0.75.
    #[test]
    fn a_41_day_old_memory_keeps_most_of_its_score() {
        let now = t0();
        let s = score_semantic(hit_at(0.58, 41, 41, now), now, &cfg()).score();
        assert!((s - 0.58 * 0.7 * 0.75).abs() < 1e-4, "got {s}");
        let pre_1_6 = 0.58 * 0.7 * 0.5f32.powf(41.0 / 14.0);
        assert!(s > 5.0 * pre_1_6);
    }

    #[test]
    fn rank_carries_keyword_evidence_into_the_score() {
        let now = t0();
        let plain = hit_at(0.6, 0, 0, now);
        let mut literal = hit_at(0.6, 0, 0, now);
        literal.rank = 0.6 + 0.3;
        assert!(
            score_semantic(literal, now, &cfg()).score()
                > score_semantic(plain, now, &cfg()).score()
        );
    }

    #[test]
    fn episodic_without_a_query_is_recency_only() {
        let now = t0();
        let s = score_episodic(event_at(1.0, 14, now), now, None, &cfg()).score();
        assert!((s - EPISODIC_WEIGHT * 0.5).abs() < 1e-3, "got {s}");
    }

    #[test]
    fn episodic_query_match_scales_between_beta_and_one() {
        let now = t0();
        let c = cfg();
        let score = |lex| score_episodic(event_at(1.0, 0, now), now, lex, &c).score();
        let beta = c.episodic_query_mix;
        assert!((score(Some(1.0)) - EPISODIC_WEIGHT).abs() < 1e-6);
        // Absent from the index: the β share of recency survives.
        assert!((score(Some(0.0)) - EPISODIC_WEIGHT * beta).abs() < 1e-6);
        assert!(score(Some(0.0)) > 0.0);
        let half = score(Some(0.5));
        assert!((half - EPISODIC_WEIGHT * (beta + (1.0 - beta) * 0.5)).abs() < 1e-6);
        // An on-topic old event beats an off-topic fresh one.
        let on_topic_old = score_episodic(event_at(1.0, 10, now), now, Some(1.0), &c).score();
        assert!(on_topic_old > score(Some(0.0)));
    }

    #[test]
    fn episodic_higher_weight_outranks_lower_weight() {
        let now = Utc::now();
        let high = score_episodic(event(0.9, 0), now, None, &cfg());
        let low = score_episodic(event(0.1, 0), now, None, &cfg());
        assert!(high.score() > low.score());
    }

    #[test]
    fn procedural_outranks_episodic_at_equal_recency() {
        let now = Utc::now();
        let p = score_procedural(pin("anchor"));
        let e = score_episodic(event(DEFAULT_RETRIEVAL_WEIGHT, 0), now, None, &cfg());
        assert!(p.score() > e.score());
    }

    #[test]
    fn recency_factor_decays_with_age() {
        let now = Utc::now();
        let fresh = recency_factor(now, now);
        let one_half_life = recency_factor(now - Duration::days(14), now);
        let two_half_lives = recency_factor(now - Duration::days(28), now);
        assert!((fresh - 1.0).abs() < 1e-6);
        assert!((one_half_life - 0.5).abs() < 1e-3);
        assert!((two_half_lives - 0.25).abs() < 1e-3);
    }

    #[test]
    fn working_fresh_collapses_to_layer_weight() {
        let now = Utc::now();
        let turn = Turn {
            role: "user".into(),
            content: "hi".into(),
            at: now,
        };
        let s = score_working(turn, now);
        assert!((s.score() - WORKING_WEIGHT).abs() < 1e-3);
    }

    #[test]
    fn working_recent_outranks_aged() {
        let now = Utc::now();
        let recent = score_working(
            Turn {
                role: "user".into(),
                content: "x".into(),
                at: now,
            },
            now,
        );
        let aged = score_working(
            Turn {
                role: "user".into(),
                content: "x".into(),
                at: now - Duration::days(60),
            },
            now,
        );
        assert!(recent.score() > aged.score());
    }

    #[test]
    fn working_outranks_episodic_at_equal_recency() {
        let now = Utc::now();
        let w = score_working(
            Turn {
                role: "user".into(),
                content: "x".into(),
                at: now,
            },
            now,
        );
        let e = score_episodic(event(DEFAULT_RETRIEVAL_WEIGHT, 0), now, None, &cfg());
        assert!(
            w.score() > e.score(),
            "working {} should outrank episodic {} at equal recency",
            w.score(),
            e.score()
        );
    }

    #[test]
    fn future_dates_clamp_to_full_score() {
        // A timestamp slightly in the future (clock skew between
        // hosts) shouldn't produce a >1 recency factor.
        let now = Utc::now();
        let factor = recency_factor(now + Duration::seconds(60), now);
        assert!((factor - 1.0).abs() < 1e-6);
    }
}
