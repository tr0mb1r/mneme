//! Scoring + ranking used by the orchestrator to trim per-layer
//! candidates down to a single budget-respecting context.
//!
//! Per implementation plan §3 Phase 5: `score = semantic_score ×
//! layer_weight × recency_decay`. The three factors live on three
//! different scales, so we normalise everything into `[0, 1]` before
//! multiplying.
//!
//! * **Semantic score**: cosine *similarity* in `[0, 1]` (we get
//!   *distance* from the HNSW; convert via `1 − distance`). Items
//!   that didn't come from a vector search default to `1.0` so they
//!   don't get artificially down-ranked.
//! * **Layer weight**: per-layer constant (procedural = 1.0,
//!   episodic = 0.8, semantic = 0.7). Procedural is the agent's
//!   anchor and should outrank everything else at equal recency.
//! * **Recency decay**: `exp(-age_days / half_life_days)`. Half-life
//!   defaults to 14 d, matching the episodic-tier window.

use chrono::{DateTime, Utc};

use crate::memory::episodic::EpisodicEvent;
use crate::memory::procedural::PinnedItem;
use crate::memory::semantic::RecallHit;

/// Layer weights. Higher = ranks higher at equal semantic + recency
/// score. Sum doesn't have to equal 1; only relative magnitudes
/// matter for ranking.
pub const PROCEDURAL_WEIGHT: f32 = 1.0;
pub const EPISODIC_WEIGHT: f32 = 0.8;
pub const SEMANTIC_WEIGHT: f32 = 0.7;

/// Half-life for the recency decay factor. Matches the spec's hot
/// tier (≤28 days): at 14 days the factor halves, at 28 days it's
/// ~25%. Items beyond that window keep ranking but with steep
/// decay.
pub const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

/// One scored row. The originating layer is captured in the variant
/// so the assembly pass can keep per-layer minimums honest.
#[derive(Debug, Clone)]
pub enum ScoredItem {
    Procedural { item: PinnedItem, score: f32 },
    Semantic { hit: RecallHit, score: f32 },
    Episodic { event: EpisodicEvent, score: f32 },
}

impl ScoredItem {
    pub fn score(&self) -> f32 {
        match self {
            ScoredItem::Procedural { score, .. } => *score,
            ScoredItem::Semantic { score, .. } => *score,
            ScoredItem::Episodic { score, .. } => *score,
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

/// Score a semantic recall hit. Cosine *distance* in `RecallHit.score`
/// flips to similarity here.
pub fn score_semantic(hit: RecallHit, now: DateTime<Utc>) -> ScoredItem {
    let similarity = (1.0 - hit.score).clamp(0.0, 1.0);
    let recency = recency_factor(hit.item.created_at, now);
    let score = similarity * SEMANTIC_WEIGHT * recency as f32;
    ScoredItem::Semantic { hit, score }
}

/// Score an episodic event.
pub fn score_episodic(event: EpisodicEvent, now: DateTime<Utc>) -> ScoredItem {
    let recency = recency_factor(event.last_accessed, now);
    let weight = (event.retrieval_weight as f64) * (EPISODIC_WEIGHT as f64);
    let score = (weight * recency) as f32;
    ScoredItem::Episodic { event, score }
}

/// `(1/2)^(elapsed / half_life)` — a true half-life curve (factor =
/// 0.5 exactly at one half-life). Equivalent to
/// `exp(-ln(2) * elapsed / half_life)`; we use the latter form
/// because `f64::exp` is faster than `f64::powf`.
fn recency_factor(ts: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let elapsed_days = (now - ts).num_seconds() as f64 / 86_400.0;
    (-elapsed_days.max(0.0) * std::f64::consts::LN_2 / RECENCY_HALF_LIFE_DAYS).exp()
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

    fn hit(distance: f32, age_days: i64) -> RecallHit {
        RecallHit {
            item: MemoryItem {
                id: MemoryId::new(),
                content: "x".into(),
                kind: MemoryKind::Fact,
                tags: vec![],
                scope: "personal".into(),
                created_at: Utc::now() - Duration::days(age_days),
            },
            score: distance,
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
        let close = score_semantic(hit(0.1, 0), now);
        let far = score_semantic(hit(0.8, 0), now);
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
        let recent = score_semantic(hit(0.2, 0), now);
        let aged = score_semantic(hit(0.2, 60), now);
        assert!(recent.score() > aged.score());
    }

    #[test]
    fn episodic_higher_weight_outranks_lower_weight() {
        let now = Utc::now();
        let high = score_episodic(event(0.9, 0), now);
        let low = score_episodic(event(0.1, 0), now);
        assert!(high.score() > low.score());
    }

    #[test]
    fn procedural_outranks_episodic_at_equal_recency() {
        let now = Utc::now();
        let p = score_procedural(pin("anchor"));
        let e = score_episodic(event(DEFAULT_RETRIEVAL_WEIGHT, 0), now);
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
    fn future_dates_clamp_to_full_score() {
        // A timestamp slightly in the future (clock skew between
        // hosts) shouldn't produce a >1 recency factor.
        let now = Utc::now();
        let factor = recency_factor(now + Duration::seconds(60), now);
        assert!((factor - 1.0).abs() < 1e-6);
    }
}
