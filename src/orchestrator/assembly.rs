//! Final assembly pass — score, budget, and pack the per-layer
//! candidates into the [`AssembledContext`] returned to the agent.
//!
//! ## Algorithm
//!
//! 1. Score every candidate from each layer
//!    ([`super::scoring::score_*`]).
//! 2. Reserve `min(per_layer_min, max_tokens / N_LAYERS)` tokens per
//!    non-empty layer. This is the "anchor" guarantee: procedural
//!    pinned items survive even when semantic returns a wall of
//!    matches.
//! 3. Greedy-fill each layer up to its reserved share, biggest score
//!    first.
//! 4. Pool the remaining budget and continue greedy-filling across
//!    every layer combined, again biggest score first.
//! 5. Sort each kept-set by descending score for stable, deterministic
//!    output. Procedural is sub-sorted by `created_at` ascending so
//!    older anchors lead — agents often expect "rules" before
//!    "details".
//!
//! Determinism: scoring is pure (no randomness), `f32` ties are
//! broken by id ordering, and we never iterate over a `HashMap`.

use chrono::Utc;

use crate::memory::episodic::EpisodicEvent;
use crate::memory::procedural::PinnedItem;
use crate::memory::semantic::RecallHit;
use crate::orchestrator::budget::{TokenBudget, estimate_tokens};
use crate::orchestrator::scoring::{ScoredItem, score_episodic, score_procedural, score_semantic};

/// One assembled context, ready to render into a prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssembledContext {
    pub procedural: Vec<PinnedItem>,
    pub semantic: Vec<RecallHit>,
    pub episodic: Vec<EpisodicEvent>,
    /// Sum of `chars/4` token estimates across every retained item.
    /// Guaranteed `<= TokenBudget::max_tokens`.
    pub total_tokens: usize,
}

const N_LAYERS: usize = 3;

pub fn assemble(
    procedural: Vec<PinnedItem>,
    semantic: Vec<RecallHit>,
    episodic: Vec<EpisodicEvent>,
    budget: &TokenBudget,
) -> AssembledContext {
    let now = Utc::now();

    // Step 1: score everything.
    let scored_proc: Vec<(usize, ScoredItem)> = procedural
        .into_iter()
        .map(|p| (estimate_tokens(&p.content), score_procedural(p)))
        .collect();
    let scored_sem: Vec<(usize, ScoredItem)> = semantic
        .into_iter()
        .map(|h| (estimate_tokens(&h.item.content), score_semantic(h, now)))
        .collect();
    let scored_epi: Vec<(usize, ScoredItem)> = episodic
        .into_iter()
        .map(|e| (estimate_tokens(&e.payload), score_episodic(e, now)))
        .collect();

    // Step 2: per-layer reservation. Empty layers don't reserve.
    let layer_reserve = budget.per_layer_min.min(budget.max_tokens / N_LAYERS);

    // Greedy-fill helper. Takes a layer's `(token_cost, scored)` list
    // (already sorted by score desc) and a remaining budget; returns
    // the kept items + tokens consumed.
    let pack_within =
        |mut items: Vec<(usize, ScoredItem)>, cap: usize| -> (Vec<ScoredItem>, usize) {
            items.sort_by(|a, b| order_scored(&a.1, &b.1));
            let mut kept: Vec<ScoredItem> = Vec::new();
            let mut used = 0usize;
            for (cost, scored) in items {
                if used + cost <= cap {
                    used += cost;
                    kept.push(scored);
                }
            }
            (kept, used)
        };

    let (proc_kept, proc_used) =
        pack_within(scored_proc.clone(), layer_reserve.min(budget.max_tokens));
    let (sem_kept, sem_used) =
        pack_within(scored_sem.clone(), layer_reserve.min(budget.max_tokens));
    let (epi_kept, epi_used) =
        pack_within(scored_epi.clone(), layer_reserve.min(budget.max_tokens));

    let mut total_used = proc_used + sem_used + epi_used;
    let mut chosen_ids = ChosenSet::default();
    chosen_ids.extend_proc(&proc_kept);
    chosen_ids.extend_sem(&sem_kept);
    chosen_ids.extend_epi(&epi_kept);

    let mut proc_final = proc_kept;
    let mut sem_final = sem_kept;
    let mut epi_final = epi_kept;

    // Step 3: pool the remainder, fill across layers by score.
    if total_used < budget.max_tokens {
        let mut remainder: Vec<(usize, ScoredItem)> = Vec::new();
        for (cost, item) in scored_proc {
            if !chosen_ids.contains(&item) {
                remainder.push((cost, item));
            }
        }
        for (cost, item) in scored_sem {
            if !chosen_ids.contains(&item) {
                remainder.push((cost, item));
            }
        }
        for (cost, item) in scored_epi {
            if !chosen_ids.contains(&item) {
                remainder.push((cost, item));
            }
        }
        remainder.sort_by(|a, b| order_scored(&a.1, &b.1));

        for (cost, item) in remainder {
            if total_used + cost > budget.max_tokens {
                continue;
            }
            total_used += cost;
            match item {
                ScoredItem::Procedural { .. } => proc_final.push(item),
                ScoredItem::Semantic { .. } => sem_final.push(item),
                ScoredItem::Episodic { .. } => epi_final.push(item),
            }
        }
    }

    // Step 4: deterministic per-layer ordering.
    proc_final.sort_by(|a, b| match (a, b) {
        (ScoredItem::Procedural { item: ai, .. }, ScoredItem::Procedural { item: bi, .. }) => ai
            .created_at
            .cmp(&bi.created_at)
            .then(ai.id.0.cmp(&bi.id.0)),
        _ => std::cmp::Ordering::Equal,
    });
    sem_final.sort_by(order_scored);
    epi_final.sort_by(order_scored);

    AssembledContext {
        procedural: proc_final
            .into_iter()
            .map(|s| match s {
                ScoredItem::Procedural { item, .. } => item,
                _ => unreachable!("scored_proc only holds Procedural"),
            })
            .collect(),
        semantic: sem_final
            .into_iter()
            .map(|s| match s {
                ScoredItem::Semantic { hit, .. } => hit,
                _ => unreachable!("scored_sem only holds Semantic"),
            })
            .collect(),
        episodic: epi_final
            .into_iter()
            .map(|s| match s {
                ScoredItem::Episodic { event, .. } => event,
                _ => unreachable!("scored_epi only holds Episodic"),
            })
            .collect(),
        total_tokens: total_used,
    }
}

/// Stable ordering: higher score first, then deterministic tiebreaker
/// by id so two equal-score rows don't shuffle between calls.
fn order_scored(a: &ScoredItem, b: &ScoredItem) -> std::cmp::Ordering {
    b.score()
        .partial_cmp(&a.score())
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| id_bytes(b).cmp(&id_bytes(a)))
}

fn id_bytes(item: &ScoredItem) -> [u8; 16] {
    match item {
        ScoredItem::Procedural { item, .. } => item.id.0.to_bytes(),
        ScoredItem::Semantic { hit, .. } => hit.item.id.0.to_bytes(),
        ScoredItem::Episodic { event, .. } => event.id.0.to_bytes(),
    }
}

#[derive(Default)]
struct ChosenSet {
    proc_ids: std::collections::HashSet<[u8; 16]>,
    sem_ids: std::collections::HashSet<[u8; 16]>,
    epi_ids: std::collections::HashSet<[u8; 16]>,
}

impl ChosenSet {
    fn extend_proc(&mut self, kept: &[ScoredItem]) {
        for it in kept {
            if let ScoredItem::Procedural { item, .. } = it {
                self.proc_ids.insert(item.id.0.to_bytes());
            }
        }
    }
    fn extend_sem(&mut self, kept: &[ScoredItem]) {
        for it in kept {
            if let ScoredItem::Semantic { hit, .. } = it {
                self.sem_ids.insert(hit.item.id.0.to_bytes());
            }
        }
    }
    fn extend_epi(&mut self, kept: &[ScoredItem]) {
        for it in kept {
            if let ScoredItem::Episodic { event, .. } = it {
                self.epi_ids.insert(event.id.0.to_bytes());
            }
        }
    }
    fn contains(&self, item: &ScoredItem) -> bool {
        match item {
            ScoredItem::Procedural { item, .. } => self.proc_ids.contains(&item.id.0.to_bytes()),
            ScoredItem::Semantic { hit, .. } => self.sem_ids.contains(&hit.item.id.0.to_bytes()),
            ScoredItem::Episodic { event, .. } => self.epi_ids.contains(&event.id.0.to_bytes()),
        }
    }
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

    fn hit(content: &str, distance: f32) -> RecallHit {
        RecallHit {
            item: MemoryItem {
                id: MemoryId::new(),
                content: content.into(),
                kind: MemoryKind::Fact,
                tags: vec![],
                scope: "personal".into(),
                created_at: Utc::now(),
            },
            score: distance,
        }
    }

    fn event(payload: &str) -> EpisodicEvent {
        EpisodicEvent {
            id: EventId::new(),
            kind: "k".into(),
            scope: "personal".into(),
            payload: payload.into(),
            retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
            last_accessed: Utc::now(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn assemble_empty_returns_zero_tokens() {
        let ctx = assemble(vec![], vec![], vec![], &TokenBudget::for_tests(1000));
        assert_eq!(ctx, AssembledContext::default());
    }

    #[test]
    fn assemble_respects_max_tokens() {
        let big = "x".repeat(500); // 125 tokens
        let proc: Vec<_> = (0..10).map(|_| pin(&big)).collect();
        let ctx = assemble(proc, vec![], vec![], &TokenBudget::for_tests(300));
        assert!(ctx.total_tokens <= 300);
        // Should fit ~2 of the 125-token items.
        assert!(ctx.procedural.len() <= 2);
    }

    #[test]
    fn assemble_per_layer_minimum_protects_each_layer() {
        // Procedural rows are individually small; semantic results
        // are large. Without the per-layer floor, one big semantic
        // hit could starve the procedural anchor.
        let proc = vec![pin("rule one"), pin("rule two")];
        let semantic_blob = "z".repeat(2000); // 500 tokens
        let semantic = vec![hit(&semantic_blob, 0.05)];
        let ctx = assemble(
            proc,
            semantic,
            vec![],
            &TokenBudget {
                max_tokens: 600,
                per_layer_min: 200,
            },
        );
        // Procedural shouldn't be empty even though semantic
        // outscored everything.
        assert!(!ctx.procedural.is_empty());
    }

    #[test]
    fn assemble_orders_semantic_by_score_descending() {
        let near = hit("near", 0.1);
        let far = hit("far", 0.9);
        let mid = hit("mid", 0.5);
        let near_id = near.item.id;
        let mid_id = mid.item.id;
        let far_id = far.item.id;
        let ctx = assemble(
            vec![],
            vec![mid, far, near],
            vec![],
            &TokenBudget::for_tests(1000),
        );
        let ids: Vec<_> = ctx.semantic.iter().map(|h| h.item.id).collect();
        assert_eq!(ids, vec![near_id, mid_id, far_id]);
    }

    #[test]
    fn assemble_total_tokens_sums_across_layers() {
        let p = pin("aaaa"); // 1 token
        let s = hit("bbbbbbbb", 0.0); // 2 tokens
        let e = event("\"cc\""); // 1 token
        let ctx = assemble(vec![p], vec![s], vec![e], &TokenBudget::for_tests(1000));
        // Allow ±1 for token-rounding boundaries.
        assert!(
            (ctx.total_tokens as i64 - 4).abs() <= 1,
            "got {}",
            ctx.total_tokens
        );
    }

    #[test]
    fn assemble_is_deterministic_under_repeated_calls() {
        let proc = vec![pin("p1"), pin("p2"), pin("p3")];
        let sem = vec![hit("s1", 0.1), hit("s2", 0.2), hit("s3", 0.3)];
        let epi = vec![event("\"e1\""), event("\"e2\"")];
        let budget = TokenBudget::for_tests(1000);
        let a = assemble(proc.clone(), sem.clone(), epi.clone(), &budget);
        let b = assemble(proc, sem, epi, &budget);
        let to_ids = |c: &AssembledContext| -> Vec<String> {
            c.procedural
                .iter()
                .map(|p| p.id.to_string())
                .chain(c.semantic.iter().map(|s| s.item.id.to_string()))
                .chain(c.episodic.iter().map(|e| e.id.to_string()))
                .collect()
        };
        assert_eq!(to_ids(&a), to_ids(&b));
    }

    #[test]
    fn assemble_zero_max_tokens_drops_everything() {
        let ctx = assemble(
            vec![pin("anything")],
            vec![hit("anything", 0.0)],
            vec![event("\"x\"")],
            &TokenBudget {
                max_tokens: 0,
                per_layer_min: 0,
            },
        );
        assert!(ctx.procedural.is_empty());
        assert!(ctx.semantic.is_empty());
        assert!(ctx.episodic.is_empty());
        assert_eq!(ctx.total_tokens, 0);
    }

    #[test]
    fn assemble_aged_items_outranked_by_fresh() {
        // Same payload; one is freshly-created, the other is
        // backdated 60 days. Score should put fresh first.
        let fresh = event("\"x\"");
        let mut aged = event("\"x\"");
        aged.last_accessed = Utc::now() - Duration::days(60);
        aged.created_at = aged.last_accessed;
        let fresh_id = fresh.id;
        // Make sure both are within layer minimum so the score, not
        // the floor, drives ordering.
        let ctx = assemble(
            vec![],
            vec![],
            vec![aged, fresh],
            &TokenBudget::for_tests(1000),
        );
        assert!(ctx.episodic.len() >= 2);
        assert_eq!(ctx.episodic[0].id, fresh_id);
    }
}
