//! Token budget for [`crate::orchestrator::Orchestrator::build_context`].
//!
//! Per the implementation plan §0, the v0.1 token estimator is
//! `chars / 4` — a coarse approximation that's accurate within
//! ±20% across English/code/JSON content. Swapping in a real
//! `tokenizers::Tokenizer` is a one-function change to
//! [`estimate_tokens`]; the rest of the budget machinery stays the
//! same.

use crate::config::{AutoContextConfig, BudgetsConfig};

/// `chars / 4` approximation. Slightly under-counts on dense text
/// (English averages ~4.5 chars/token in cl100k_base) and slightly
/// over-counts on code with heavy whitespace, but it's monotonic in
/// content length, which is enough for budget enforcement.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Tunable budget. Production builds construct this from
/// `config.budgets.auto_context_token_budget`; tests use
/// [`TokenBudget::for_tests`].
///
/// How the budget is split between layers is not here: see
/// [`LayerShares`], built from `[auto_context]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    /// Cap on the assembled context. Working turns, episodic events
    /// and semantic rows are trimmed to fit under it. Pinned procedural
    /// items are never trimmed, so the one way `total_tokens` exceeds
    /// this is pins that are larger than the whole budget by
    /// themselves; then every other layer is empty.
    pub max_tokens: usize,
}

impl TokenBudget {
    /// Production default — 4000 tokens, matching
    /// `BudgetsConfig::auto_context_token_budget`.
    pub fn production() -> Self {
        Self { max_tokens: 4000 }
    }

    /// Aggressive limits for tests. Keeps `max_tokens` small so tests
    /// can prove the trim path actually fires.
    pub fn for_tests(max_tokens: usize) -> Self {
        Self { max_tokens }
    }

    /// Build from the live config.
    pub fn from_config(c: &BudgetsConfig) -> Self {
        Self {
            max_tokens: c.auto_context_token_budget,
        }
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self::production()
    }
}

/// How the token budget is divided between layers, from
/// `[auto_context]`. Replaces the pre-1.6 flat reservation of 1/16 of
/// the budget per layer, under which events (scored higher than
/// memories and charged at full payload size) took about three
/// quarters of every context.
///
/// * **Procedural** has no share: pins are binding and always kept.
/// * **Episodic** and **working** have a *ceiling*.
/// * **Semantic** has a *floor*, reserved before anything else is
///   packed, and no ceiling: whatever the capped layers leave is its.
///
/// Shares are fractions of the whole budget, not of what is left after
/// pins, so a caller can reason about them from the config alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerShares {
    pub episodic_max: f32,
    pub semantic_min: f32,
    pub semantic_min_items: usize,
    pub working_max: f32,
}

impl LayerShares {
    /// Shares from the config, each clamped into `[0, 1]`. A NaN share
    /// counts as zero.
    pub fn from_config(c: &AutoContextConfig) -> Self {
        Self {
            episodic_max: unit(c.episodic_max_share),
            semantic_min: unit(c.semantic_min_share),
            semantic_min_items: c.semantic_min_items,
            working_max: unit(c.working_max_share),
        }
    }

    /// Most tokens episodic events may take.
    pub fn episodic_cap(&self, budget: &TokenBudget) -> usize {
        share_of(budget.max_tokens, self.episodic_max)
    }

    /// Most tokens working-session turns may take.
    pub fn working_cap(&self, budget: &TokenBudget) -> usize {
        share_of(budget.max_tokens, self.working_max)
    }

    /// Tokens reserved for semantic rows before the other layers are
    /// packed: `semantic_min` of the budget, or the size of the top
    /// `semantic_min_items` rows if that is smaller. `ranked_costs` is
    /// the token cost of each semantic candidate, best first.
    pub fn semantic_reserve(&self, budget: &TokenBudget, ranked_costs: &[usize]) -> usize {
        let by_share = share_of(budget.max_tokens, self.semantic_min);
        let by_items: usize = ranked_costs.iter().take(self.semantic_min_items).sum();
        by_share.min(by_items)
    }
}

fn unit(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

fn share_of(max_tokens: usize, share: f32) -> usize {
    // The share arrives as an f32 (0.35 is really 0.34999999), so
    // round it to six decimals first: 35% of 4000 must be 1400, not
    // 1399.
    let share = (f64::from(share) * 1e6).round() / 1e6;
    (max_tokens as f64 * share + 1e-6).floor() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_returns_zero_on_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_rounds_up() {
        // 1 char  →  ceil(1/4) = 1 token
        // 4 chars →  1 token
        // 5 chars →  2 tokens
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn estimate_tokens_counts_unicode_codepoints() {
        // The "é" char is one codepoint in `chars()` even though it
        // takes 2 bytes in UTF-8. We count codepoints because that's
        // what real tokenizers approximate.
        assert_eq!(estimate_tokens("café"), estimate_tokens("cafe"));
    }

    #[test]
    fn default_shares_of_the_default_budget() {
        let shares = LayerShares::from_config(&AutoContextConfig::default());
        let b = TokenBudget::production();
        assert_eq!(shares.episodic_cap(&b), 1400);
        assert_eq!(shares.working_cap(&b), 200);
        // Five rows of 100 tokens are smaller than 30% of 4000.
        assert_eq!(shares.semantic_reserve(&b, &[100; 8]), 500);
        // Five rows of 1000 are not.
        assert_eq!(shares.semantic_reserve(&b, &[1000; 8]), 1200);
        // No candidates, nothing reserved.
        assert_eq!(shares.semantic_reserve(&b, &[]), 0);
    }

    #[test]
    fn out_of_range_shares_are_clamped() {
        let cfg = AutoContextConfig {
            episodic_max_share: 7.0,
            working_max_share: -1.0,
            semantic_min_share: f32::NAN,
            ..Default::default()
        };
        let shares = LayerShares::from_config(&cfg);
        let b = TokenBudget::for_tests(1000);
        assert_eq!(shares.episodic_cap(&b), 1000);
        assert_eq!(shares.working_cap(&b), 0);
        assert_eq!(shares.semantic_reserve(&b, &[10, 10]), 0);
    }
}
