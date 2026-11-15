//! Final assembly pass — score, budget, and pack the per-layer
//! candidates into the [`AssembledContext`] returned to the agent.
//!
//! ## Algorithm
//!
//! 1. **Procedural** pins are all kept. They are the agent's standing
//!    rules, so they are never trimmed; everything else shares what
//!    they leave. Pins larger than the whole budget are the one case
//!    where `total_tokens` exceeds `max_tokens`, and then every other
//!    layer is empty.
//! 2. **Episodic** candidates are scored against the query,
//!    de-duplicated, diversified and clipped ([`super::events`]).
//! 3. **Semantic floor.** The best semantic rows are packed into the
//!    reserve from [`LayerShares::semantic_reserve`] before any other
//!    layer gets a token.
//! 4. **Pool.** Remaining semantic rows, events and working turns are
//!    packed together, best score first, while the total stays under
//!    the budget and events and turns stay under their ceilings
//!    ([`LayerShares::episodic_cap`], [`LayerShares::working_cap`]).
//!    Events and fresh turns score above memories, so they fill their
//!    ceilings first and semantic takes the rest.
//! 5. Each layer is sorted for output: procedural oldest first (rules
//!    before details), semantic and episodic by score, working newest
//!    first.
//!
//! A row that does not fit is skipped, not a stopping point: a smaller
//! row further down may still fit.
//!
//! Determinism: scoring is pure (`now` is a parameter), `f32` ties are
//! broken by id, and no `HashMap` iteration order reaches the output.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::config::AutoContextConfig;
use crate::ids::EventId;
use crate::memory::episodic::EpisodicEvent;
use crate::memory::procedural::PinnedItem;
use crate::memory::semantic::RecallHit;
use crate::memory::working::Turn;
use crate::orchestrator::budget::{LayerShares, TokenBudget, estimate_tokens};
use crate::orchestrator::events::select_events;
use crate::orchestrator::scoring::{semantic_score, working_score};

/// One assembled context, ready to render into a prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssembledContext {
    pub procedural: Vec<PinnedItem>,
    pub semantic: Vec<RecallHit>,
    /// Events, best first. Payloads are already clipped to
    /// `[auto_context] event_clip_chars`; see
    /// [`Self::truncated_events`].
    pub episodic: Vec<EpisodicEvent>,
    /// Working-session turns from the active session (if any). Empty
    /// when the orchestrator has no `ActiveSession` attached, e.g.
    /// in tests that exercise only L0/L3/L4. Sorted newest-first
    /// (by `Turn.at`).
    pub working: Vec<Turn>,
    /// Sum of `chars/4` token estimates across every retained item.
    /// At most `TokenBudget::max_tokens`, unless the pins alone exceed
    /// it (they are never trimmed).
    pub total_tokens: usize,
    /// Where `total_tokens` went.
    pub layer_tokens: LayerTokens,
    /// Ids of the events in [`Self::episodic`] whose payload was
    /// clipped. They were charged at the clipped size.
    pub truncated_events: Vec<EventId>,
}

impl AssembledContext {
    /// Whether `id`'s payload was clipped.
    pub fn is_truncated(&self, id: EventId) -> bool {
        self.truncated_events.contains(&id)
    }
}

/// Tokens spent per layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayerTokens {
    pub procedural: usize,
    pub working: usize,
    pub episodic: usize,
    pub semantic: usize,
}

impl LayerTokens {
    pub fn total(&self) -> usize {
        self.procedural + self.working + self.episodic + self.semantic
    }
}

/// Everything [`assemble`] needs besides the candidates. Bundled so the
/// pass stays a pure function of its arguments: no clock, no config
/// lookup.
#[derive(Debug, Clone, Copy)]
pub struct AssemblyParams<'a> {
    pub budget: TokenBudget,
    pub config: &'a AutoContextConfig,
    /// The instant ages are measured from.
    pub now: DateTime<Utc>,
    /// Lexical coverage of the query per event. `None` when the read
    /// carried no query; with a query, an absent id scores zero.
    pub lex: Option<&'a HashMap<EventId, f32>>,
}

/// A semantic row or working turn waiting for the pooled pass. Events
/// arrive pre-scored from [`select_events`].
struct Candidate {
    layer: Layer,
    /// Index into the layer's own vector.
    index: usize,
    cost: usize,
    score: f32,
    /// Deterministic tie-break, higher first.
    tie: [u8; 16],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Semantic,
    Episodic,
    Working,
}

pub fn assemble(
    procedural: Vec<PinnedItem>,
    semantic: Vec<RecallHit>,
    episodic: Vec<EpisodicEvent>,
    working: Vec<Turn>,
    params: &AssemblyParams<'_>,
) -> AssembledContext {
    let AssemblyParams {
        budget,
        config,
        now,
        lex,
    } = *params;
    let max_tokens = budget.max_tokens;
    let shares = LayerShares::from_config(config);

    // Step 1: pins, all of them.
    let mut procedural = procedural;
    procedural.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.0.cmp(&b.id.0)));
    let proc_used: usize = procedural.iter().map(|p| estimate_tokens(&p.content)).sum();
    // What the other three layers share.
    let available = max_tokens.saturating_sub(proc_used);

    // Step 2: score. Semantic and working are scored here; events come
    // back from `select_events` scored, diversified and clipped.
    let mut sem: Vec<Option<RecallHit>> = Vec::with_capacity(semantic.len());
    let mut work: Vec<Option<Turn>> = Vec::with_capacity(working.len());
    let mut candidates: Vec<Candidate> = Vec::new();

    for (index, hit) in semantic.into_iter().enumerate() {
        let cost = estimate_tokens(&hit.item.content);
        candidates.push(Candidate {
            layer: Layer::Semantic,
            index,
            cost,
            score: semantic_score(&hit, now, config),
            tie: hit.item.id.0.to_bytes(),
        });
        sem.push(Some(hit));
    }
    for (index, turn) in working.into_iter().enumerate() {
        let cost = estimate_tokens(&turn.content);
        candidates.push(Candidate {
            layer: Layer::Working,
            index,
            cost,
            score: working_score(&turn, now),
            tie: turn_pseudo_id(&turn),
        });
        work.push(Some(turn));
    }
    let selected = select_events(episodic, lex, now, config);
    let mut truncated: Vec<EventId> = Vec::new();
    let mut epi: Vec<Option<EpisodicEvent>> = Vec::with_capacity(selected.len());
    for (index, s) in selected.into_iter().enumerate() {
        if s.truncated {
            truncated.push(s.event.id);
        }
        candidates.push(Candidate {
            layer: Layer::Episodic,
            index,
            cost: s.tokens,
            score: s.score,
            tie: s.event.id.0.to_bytes(),
        });
        epi.push(Some(s.event));
    }
    candidates.sort_by(order_candidates);

    // Step 3: the semantic floor.
    let ranked_semantic_costs: Vec<usize> = candidates
        .iter()
        .filter(|c| c.layer == Layer::Semantic)
        .map(|c| c.cost)
        .collect();
    let semantic_reserve = shares
        .semantic_reserve(&budget, &ranked_semantic_costs)
        .min(available);
    let episodic_cap = shares.episodic_cap(&budget).min(available);
    let working_cap = shares.working_cap(&budget).min(available);

    let mut used = LayerTokens {
        procedural: proc_used,
        ..Default::default()
    };
    let mut taken = vec![false; candidates.len()];
    for (i, c) in candidates.iter().enumerate() {
        if c.layer == Layer::Semantic && used.semantic + c.cost <= semantic_reserve {
            used.semantic += c.cost;
            taken[i] = true;
        }
    }

    // Step 4: the pool, best score first, under the layer ceilings.
    for (i, c) in candidates.iter().enumerate() {
        if taken[i] {
            continue;
        }
        let spent = used.working + used.episodic + used.semantic;
        if spent + c.cost > available {
            continue;
        }
        match c.layer {
            Layer::Semantic => used.semantic += c.cost,
            Layer::Episodic if used.episodic + c.cost <= episodic_cap => used.episodic += c.cost,
            Layer::Working if used.working + c.cost <= working_cap => used.working += c.cost,
            _ => continue,
        }
        taken[i] = true;
    }

    // Step 5: move the kept rows out. `candidates` is score-ordered, so
    // semantic and episodic come out best first.
    let mut semantic_out = Vec::new();
    let mut episodic_out = Vec::new();
    let mut working_out = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if !taken[i] {
            continue;
        }
        match c.layer {
            Layer::Semantic => semantic_out.extend(sem.get_mut(c.index).and_then(Option::take)),
            Layer::Episodic => episodic_out.extend(epi.get_mut(c.index).and_then(Option::take)),
            Layer::Working => working_out.extend(work.get_mut(c.index).and_then(Option::take)),
        }
    }
    // Working sorted newest-first by `Turn.at` (and content as a
    // last-resort tie-break — turns lack stable ids). The agent
    // reads working as "what just happened", so chronological
    // descending matches that intuition better than score order.
    working_out.sort_by(|a: &Turn, b: &Turn| b.at.cmp(&a.at).then(b.content.cmp(&a.content)));
    truncated.retain(|id| episodic_out.iter().any(|e: &EpisodicEvent| e.id == *id));

    AssembledContext {
        procedural,
        semantic: semantic_out,
        episodic: episodic_out,
        working: working_out,
        total_tokens: used.total(),
        layer_tokens: used,
        truncated_events: truncated,
    }
}

/// Stable ordering: higher score first, then deterministic tiebreaker
/// by id so two equal-score rows don't shuffle between calls.
fn order_candidates(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| b.tie.cmp(&a.tie))
}

/// Working turns lack ULIDs, so we synthesise a 16-byte tiebreaker
/// from the turn's timestamp + a hash of role+content. Sufficient for
/// the score-tie ordering invariant; not used as a real identifier.
fn turn_pseudo_id(turn: &Turn) -> [u8; 16] {
    let mut out = [0u8; 16];
    let ts_nanos = turn.at.timestamp_nanos_opt().unwrap_or(0).to_be_bytes();
    out[..8].copy_from_slice(&ts_nanos);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    turn.role.hash(&mut hasher);
    turn.content.hash(&mut hasher);
    out[8..].copy_from_slice(&hasher.finish().to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MemoryId;
    use crate::memory::episodic::DEFAULT_RETRIEVAL_WEIGHT;
    use crate::memory::semantic::{MatchSource, MemoryItem, MemoryKind, MemoryStatus};
    use chrono::Duration;

    /// A fixed instant: nothing in this module reads the wall clock.
    fn t0() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn cfg() -> AutoContextConfig {
        AutoContextConfig::default()
    }

    fn run(
        procedural: Vec<PinnedItem>,
        semantic: Vec<RecallHit>,
        episodic: Vec<EpisodicEvent>,
        working: Vec<Turn>,
        max_tokens: usize,
    ) -> AssembledContext {
        run_with(procedural, semantic, episodic, working, max_tokens, &cfg())
    }

    fn run_with(
        procedural: Vec<PinnedItem>,
        semantic: Vec<RecallHit>,
        episodic: Vec<EpisodicEvent>,
        working: Vec<Turn>,
        max_tokens: usize,
        config: &AutoContextConfig,
    ) -> AssembledContext {
        assemble(
            procedural,
            semantic,
            episodic,
            working,
            &AssemblyParams {
                budget: TokenBudget::for_tests(max_tokens),
                config,
                now: t0(),
                lex: None,
            },
        )
    }

    fn pin(content: &str) -> PinnedItem {
        PinnedItem {
            id: MemoryId::new(),
            content: content.into(),
            tags: vec![],
            scope: "personal".into(),
            created_at: t0(),
        }
    }

    fn hit(content: &str, distance: f32) -> RecallHit {
        hit_aged(content, 1.0 - distance, 0, 0)
    }

    fn hit_aged(content: &str, similarity: f32, created_days: i64, updated_days: i64) -> RecallHit {
        RecallHit {
            item: MemoryItem {
                id: MemoryId::new(),
                content: content.into(),
                kind: MemoryKind::Fact,
                tags: vec![],
                scope: "personal".into(),
                created_at: t0() - Duration::days(created_days),
            },
            score: 1.0 - similarity,
            superseded_by: None,
            matched: MatchSource::Semantic,
            rank: similarity,
            updated_at: t0() - Duration::days(updated_days),
            status: MemoryStatus::Active,
            demoted: false,
            relations: Default::default(),
            provenance: None,
            source: None,
        }
    }

    fn event(payload: &str) -> EpisodicEvent {
        event_aged("k", payload, 0)
    }

    fn event_aged(kind: &str, payload: &str, age_days: i64) -> EpisodicEvent {
        let ts = t0() - Duration::days(age_days);
        EpisodicEvent {
            id: EventId::new(),
            kind: kind.into(),
            scope: "personal".into(),
            payload: payload.into(),
            tags: vec![],
            retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
            last_accessed: ts,
            created_at: ts,
        }
    }

    /// `n` distinct events, each `chars` characters of distinct words
    /// (so neither the exact nor the near-duplicate filter fires), with
    /// kinds cycling so the per-kind cap does not bind either.
    fn distinct_events(n: usize, chars: usize, age_days: i64) -> Vec<EpisodicEvent> {
        (0..n)
            .map(|i| {
                let mut text = String::new();
                let mut w = 0;
                while text.len() < chars {
                    text.push_str(&format!("e{i}w{w} "));
                    w += 1;
                }
                text.truncate(chars);
                event_aged(&format!("kind{}", i % 16), &text, age_days)
            })
            .collect()
    }

    fn turn(content: &str) -> Turn {
        Turn {
            role: "user".into(),
            content: content.into(),
            at: t0(),
        }
    }

    #[test]
    fn assemble_empty_returns_zero_tokens() {
        let ctx = run(vec![], vec![], vec![], vec![], 1000);
        assert_eq!(ctx, AssembledContext::default());
    }

    #[test]
    fn assemble_respects_max_tokens() {
        let big = "x".repeat(500); // 125 tokens
        let sem: Vec<_> = (0..10).map(|i| hit(&big, 0.1 * i as f32)).collect();
        let ctx = run(vec![], sem, vec![], vec![], 300);
        assert!(ctx.total_tokens <= 300);
        assert_eq!(ctx.semantic.len(), 2);
    }

    /// Pins are binding: a wall of semantic matches cannot evict them.
    #[test]
    fn pins_survive_a_wall_of_semantic_matches() {
        let proc = vec![pin("rule one"), pin("rule two")];
        let semantic_blob = "z".repeat(2000); // 500 tokens
        let semantic = vec![hit(&semantic_blob, 0.05), hit(&semantic_blob, 0.06)];
        let ctx = run(proc, semantic, vec![], vec![], 600);
        assert_eq!(ctx.procedural.len(), 2);
        assert_eq!(ctx.semantic.len(), 1);
        assert!(ctx.total_tokens <= 600);
    }

    /// Pins are never trimmed, even past the budget; the other layers
    /// shrink to make room, down to nothing.
    #[test]
    fn pins_over_budget_are_kept_and_empty_the_other_layers() {
        let big_pin = "p".repeat(800); // 200 tokens
        let proc: Vec<_> = (0..10).map(|_| pin(&big_pin)).collect();
        let ctx = run(
            proc,
            vec![hit("a memory", 0.1)],
            vec![event("\"an event\"")],
            vec![turn("session note")],
            1000,
        );
        assert_eq!(ctx.procedural.len(), 10);
        assert!(ctx.semantic.is_empty());
        assert!(ctx.episodic.is_empty());
        assert!(ctx.working.is_empty());
        assert_eq!(ctx.total_tokens, 2000);
        assert_eq!(ctx.layer_tokens.procedural, 2000);
    }

    /// Pins that fit leave the remainder to the rest, and the total
    /// then honours the budget.
    #[test]
    fn other_layers_share_what_pins_leave() {
        let proc: Vec<_> = (0..3).map(|_| pin(&"p".repeat(800))).collect(); // 600
        let sem: Vec<_> = (0..10)
            .map(|i| hit(&"s".repeat(400), 0.1 + 0.01 * i as f32))
            .collect(); // 100 each
        let ctx = run(proc, sem, vec![], vec![], 1000);
        assert_eq!(ctx.procedural.len(), 3);
        assert_eq!(ctx.semantic.len(), 4);
        assert_eq!(ctx.total_tokens, 1000);
    }

    #[test]
    fn assemble_orders_semantic_by_score_descending() {
        let near = hit("near", 0.1);
        let far = hit("far", 0.9);
        let mid = hit("mid", 0.5);
        let near_id = near.item.id;
        let mid_id = mid.item.id;
        let far_id = far.item.id;
        let ctx = run(vec![], vec![mid, far, near], vec![], vec![], 1000);
        let ids: Vec<_> = ctx.semantic.iter().map(|h| h.item.id).collect();
        assert_eq!(ids, vec![near_id, mid_id, far_id]);
    }

    #[test]
    fn assemble_total_tokens_sums_across_layers() {
        let p = pin("aaaa"); // 1 token
        let s = hit("bbbbbbbb", 0.0); // 2 tokens
        let e = event("\"cc\""); // 1 token
        let w = turn("dddd"); // 1 token
        let ctx = run(vec![p], vec![s], vec![e], vec![w], 1000);
        assert_eq!(ctx.total_tokens, 5);
        assert_eq!(
            ctx.layer_tokens,
            LayerTokens {
                procedural: 1,
                working: 1,
                episodic: 1,
                semantic: 2,
            }
        );
    }

    #[test]
    fn assemble_is_deterministic_under_repeated_calls() {
        let proc = vec![pin("p1"), pin("p2"), pin("p3")];
        let sem = vec![hit("s1", 0.1), hit("s2", 0.2), hit("s3", 0.3)];
        let epi = vec![event("\"e1\""), event("\"e2\"")];
        let work = vec![turn("w1"), turn("w2")];
        let a = run(proc.clone(), sem.clone(), epi.clone(), work.clone(), 1000);
        let b = run(proc, sem, epi, work, 1000);
        assert_eq!(a, b);
        assert_eq!(a.semantic.len(), 3);
        assert_eq!(a.episodic.len(), 2);
    }

    #[test]
    fn zero_budget_keeps_only_pins() {
        let ctx = run(
            vec![pin("anything")],
            vec![hit("anything", 0.0)],
            vec![event("\"x\"")],
            vec![turn("anything")],
            0,
        );
        assert_eq!(ctx.procedural.len(), 1);
        assert!(ctx.semantic.is_empty());
        assert!(ctx.episodic.is_empty());
        assert!(ctx.working.is_empty());
        assert_eq!(ctx.total_tokens, 2);
    }

    #[test]
    fn assemble_aged_items_outranked_by_fresh() {
        let fresh = event("\"x\"");
        let aged = event_aged("k", "\"y\"", 60);
        let fresh_id = fresh.id;
        let ctx = run(vec![], vec![], vec![aged, fresh], vec![], 1000);
        assert_eq!(ctx.episodic.len(), 2);
        assert_eq!(ctx.episodic[0].id, fresh_id);
    }

    #[test]
    fn assemble_passes_working_through() {
        let ws = vec![turn("first"), turn("second"), turn("third")];
        let ctx = run(vec![], vec![], vec![], ws.clone(), 1000);
        assert_eq!(ctx.working.len(), 3);
        assert_eq!(
            ctx.total_tokens,
            ws.iter()
                .map(|t| estimate_tokens(&t.content))
                .sum::<usize>()
        );
    }

    #[test]
    fn assemble_working_orders_newest_first() {
        let now = t0();
        let at = |content: &str, minutes: i64| Turn {
            role: "user".into(),
            content: content.into(),
            at: now - Duration::minutes(minutes),
        };
        let ctx = run(
            vec![],
            vec![],
            vec![],
            vec![at("old", 5), at("mid", 2), at("new", 0)],
            1000,
        );
        let order: Vec<_> = ctx.working.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(order, vec!["new", "mid", "old"]);
    }

    // ---- shares -----------------------------------------------------

    /// The reported failure, at the reported sizes: 64 day-old weight-1
    /// events of ~190 tokens each (12 000 tokens of candidates for a
    /// 4000-token budget) against a handful of memories at similarity
    /// 0.55. Before 1.6 the events took ~3000 tokens and two memories
    /// survived. Now events stop at 35% and every memory is kept.
    #[test]
    fn day_old_events_do_not_displace_sim_0_55_memories() {
        let c = AutoContextConfig {
            // Charge events at full size, as before 1.6: the share cap
            // alone has to hold.
            event_clip_chars: 0,
            max_events_per_kind: 0,
            ..cfg()
        };
        let events = distinct_events(64, 760, 1);
        let memories: Vec<_> = (0..12)
            .map(|i| hit_aged(&format!("memory {i} {}", "m".repeat(390)), 0.55, 30, 30))
            .collect(); // ~100 tokens each
        let ctx = run_with(vec![], memories, events, vec![], 4000, &c);

        assert_eq!(ctx.semantic.len(), 12, "every memory survives");
        assert!(ctx.layer_tokens.episodic <= 1400);
        assert!(
            ctx.layer_tokens.episodic > 1000,
            "events still get their share"
        );
        assert!(ctx.total_tokens <= 4000);
        // Each event outscores each memory; it is the shares, not the
        // scores, that protect the memories.
        let event_score = 0.8 * 0.5f32.powf(1.0 / 14.0);
        let memory_score = 0.55 * 0.7 * 0.5f32.powf(30.0 / 90.0);
        assert!(event_score > memory_score);
    }

    /// Past the semantic floor the memory competes in the pool, where
    /// the event outscores it. It is still kept: the event may only
    /// use the episodic share.
    #[test]
    fn a_fresh_event_cannot_take_the_room_of_a_memory_past_the_floor() {
        // 1000-token budget. Floor = min(30% = 300, 5 rows) = 300, so
        // three 100-token memories are reserved and five more are not.
        let memories: Vec<_> = (0..8)
            .map(|i| hit_aged(&format!("{i} {}", "m".repeat(396)), 0.55, 5, 5))
            .collect();
        let ids: Vec<_> = memories.iter().map(|h| h.item.id).collect();
        // Twenty 100-token day-old events want 2000 tokens.
        let c = AutoContextConfig {
            max_events_per_kind: 0,
            ..cfg()
        };
        let ctx = run_with(
            vec![],
            memories,
            distinct_events(20, 400, 1),
            vec![],
            1000,
            &c,
        );

        assert!(ctx.layer_tokens.episodic <= 350);
        assert_eq!(ctx.episodic.len(), 3);
        // 1000 − 300 of events leaves room for all but one memory.
        assert_eq!(ctx.semantic.len(), 7);
        for kept in &ctx.semantic {
            assert!(ids.contains(&kept.item.id));
        }
        assert!(ctx.total_tokens <= 1000);
    }

    /// Plan §2: similarity 0.58, created 41 days ago, edited in place
    /// since. Under the pre-1.6 scale it scored 0.053 and every event
    /// younger than ~55 days beat it out of the context.
    #[test]
    fn the_41_day_old_current_state_memory_is_kept() {
        let current_state = hit_aged(&"c".repeat(1200), 0.58, 41, 41); // 300 tokens
        let id = current_state.item.id;
        let mut semantic = vec![current_state];
        // Fresher, closer competitors for the semantic share.
        semantic.extend((0..6).map(|i| hit_aged(&format!("{i} {}", "n".repeat(790)), 0.7, 2, 2)));
        // A full window of weight-1 events from the last fortnight.
        let mut events = Vec::new();
        for day in 0..16 {
            events.extend(distinct_events(4, 1500, day).into_iter().enumerate().map(
                |(i, mut e)| {
                    e.payload = format!("day{day}n{i} {}", e.payload);
                    e.kind = format!("kind{}", (day as usize * 4 + i) % 12);
                    e
                },
            ));
        }
        assert_eq!(events.len(), 64);
        let ctx = run(vec![pin("be terse")], semantic, events, vec![], 4000);

        assert!(
            ctx.semantic.iter().any(|h| h.item.id == id),
            "the current-state memory was dropped"
        );
        assert_eq!(ctx.semantic.len(), 7);
        assert!(ctx.layer_tokens.episodic <= 1400);
        assert!(ctx.total_tokens <= 4000);
    }

    /// Whatever the sizes thrown at it, events stay under their
    /// ceiling, turns under theirs, the total under the budget, and
    /// semantic gets its floor whenever it has rows to fill it.
    #[test]
    fn share_caps_hold_under_adversarial_sizes() {
        let sizes = [1usize, 3, 40, 399, 400, 401, 1600, 5000, 20_000];
        for &max_tokens in &[64usize, 500, 1000, 4000, 16_000] {
            for &event_chars in &sizes {
                for &memory_chars in &sizes {
                    for &turn_chars in &[8usize, 400, 4000] {
                        let c = AutoContextConfig {
                            // Unclipped, uncapped: the worst case.
                            event_clip_chars: 0,
                            max_events_per_kind: 0,
                            ..cfg()
                        };
                        let memories: Vec<_> = (0..20)
                            .map(|i| {
                                hit_aged(
                                    &format!("{i} {}", "m".repeat(memory_chars)),
                                    0.3 + 0.02 * i as f32,
                                    i,
                                    i,
                                )
                            })
                            .collect();
                        let memory_cost = estimate_tokens(&memories[0].item.content);
                        let turns: Vec<_> = (0..32)
                            .map(|i| turn(&format!("{i} {}", "t".repeat(turn_chars))))
                            .collect();
                        let ctx = run_with(
                            vec![],
                            memories,
                            distinct_events(64, event_chars, 0),
                            turns,
                            max_tokens,
                            &c,
                        );
                        let label = format!(
                            "budget={max_tokens} event={event_chars} memory={memory_chars} turn={turn_chars}"
                        );
                        let t = ctx.layer_tokens;
                        assert!(t.total() <= max_tokens, "{label}: total {t:?}");
                        assert_eq!(t.total(), ctx.total_tokens, "{label}");
                        assert!(
                            t.episodic as f64 <= 0.35 * max_tokens as f64,
                            "{label}: episodic {t:?}"
                        );
                        assert!(
                            t.working as f64 <= 0.05 * max_tokens as f64,
                            "{label}: working {t:?}"
                        );
                        // The floor: 30% of the budget, or five rows.
                        let floor = (0.30 * max_tokens as f64) as usize;
                        let want = floor.min(5 * memory_cost);
                        if memory_cost <= want {
                            // Packing is whole rows, so allow one row of slack.
                            assert!(
                                t.semantic + memory_cost > want,
                                "{label}: semantic {t:?} under its floor {want}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn working_turns_stop_at_their_ceiling() {
        let turns: Vec<_> = (0..32)
            .map(|i| turn(&format!("{i:02} {}", "t".repeat(37))))
            .collect(); // 10 tokens each
        let ctx = run(vec![], vec![], vec![], turns, 1000);
        assert_eq!(ctx.layer_tokens.working, 50);
        assert_eq!(ctx.working.len(), 5);
    }

    #[test]
    fn semantic_floor_is_the_smaller_of_share_and_item_count() {
        // Five small rows are far below 30%: the reserve is just them,
        // and with events capped the rest of the rows fit anyway.
        let memories: Vec<_> = (0..9)
            .map(|i| hit_aged(&format!("{i} {}", "m".repeat(38)), 0.5, 1, 1))
            .collect(); // 10 tokens each
        let c = AutoContextConfig {
            // No pool for semantic beyond the floor: events may take
            // everything else.
            episodic_max_share: 1.0,
            max_events_per_kind: 0,
            ..cfg()
        };
        let ctx = run_with(
            vec![],
            memories,
            distinct_events(64, 200, 0),
            vec![],
            1000,
            &c,
        );
        // Reserve = 5 rows × 10 tokens, not 300: events fill the rest.
        assert_eq!(ctx.semantic.len(), 5);
        assert_eq!(ctx.layer_tokens.semantic, 50);
        assert_eq!(ctx.layer_tokens.episodic, 950);
    }

    // ---- events -----------------------------------------------------

    #[test]
    fn clipped_events_are_flagged_and_charged_at_clipped_size() {
        let long = serde_json::json!({ "content": "detail ".repeat(300) }).to_string();
        let short = event("\"short\"");
        let long = event_aged("summary", &long, 0);
        let (short_id, long_id) = (short.id, long.id);
        let ctx = run(vec![], vec![], vec![short, long], vec![], 4000);
        assert_eq!(ctx.truncated_events, vec![long_id]);
        assert!(ctx.is_truncated(long_id));
        assert!(!ctx.is_truncated(short_id));
        // 400 chars is 100 tokens; the full payload was over 500.
        assert!(ctx.layer_tokens.episodic <= 102);
    }

    /// Six openers, six topics. With lexical scores from the index each
    /// opener gets its own events; before 1.6 all six got the same list.
    #[test]
    fn six_queries_produce_distinct_event_sets() {
        let topics = ["trading", "ops", "hermes", "billing", "infra", "research"];
        // 48 events, 8 per topic, spread over 12 days so recency alone
        // would always pick the same newest handful.
        let mut events = Vec::new();
        for (t, topic) in topics.iter().enumerate() {
            for i in 0..8 {
                let payload = serde_json::json!({
                    "content": format!("{topic} item {i}: {}", format!("{topic}{i} ").repeat(20)),
                    "topic": topic,
                })
                .to_string();
                events.push(event_aged(
                    ["decision", "lesson", "summary"][i % 3],
                    &payload,
                    ((t + i * 3) % 12) as i64,
                ));
            }
        }
        // The fake index: full coverage for events on the query's topic.
        let lex_for = |topic: &str| -> HashMap<EventId, f32> {
            events
                .iter()
                .filter(|e| e.payload.contains(&format!("\"topic\":\"{topic}\"")))
                .map(|e| (e.id, 1.0))
                .collect()
        };
        let config = cfg();
        let assemble_for = |lex: Option<&HashMap<EventId, f32>>| -> Vec<EventId> {
            let ctx = assemble(
                vec![],
                vec![],
                events.clone(),
                vec![],
                &AssemblyParams {
                    // Room for about seven clipped events.
                    budget: TokenBudget::for_tests(2000),
                    config: &config,
                    now: t0(),
                    lex,
                },
            );
            let mut ids: Vec<EventId> = ctx.episodic.iter().map(|e| e.id).collect();
            ids.sort();
            ids
        };

        let mut sets: Vec<Vec<EventId>> = Vec::new();
        for topic in topics {
            let lex = lex_for(topic);
            let ids = assemble_for(Some(&lex));
            assert!(!ids.is_empty());
            // Most of what each opener gets is on its topic.
            let on_topic = ids.iter().filter(|id| lex.contains_key(id)).count();
            assert!(
                on_topic * 2 > ids.len(),
                "{topic}: {on_topic} of {} on topic",
                ids.len()
            );
            sets.push(ids);
        }
        sets.sort();
        sets.dedup();
        assert!(sets.len() >= 4, "only {} distinct event sets", sets.len());

        // Without a query every read is the same recency list.
        assert_eq!(assemble_for(None), assemble_for(None));
    }
}
