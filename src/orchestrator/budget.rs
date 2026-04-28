//! Token budget for [`crate::orchestrator::Orchestrator::build_context`].
//!
//! Per the implementation plan §0, the v0.1 token estimator is
//! `chars / 4` — a coarse approximation that's accurate within
//! ±20% across English/code/JSON content. Swapping in a real
//! `tokenizers::Tokenizer` is a one-function change to
//! [`estimate_tokens`]; the rest of the budget machinery stays the
//! same.

use crate::config::BudgetsConfig;

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
#[derive(Debug, Clone, Copy)]
pub struct TokenBudget {
    /// Hard cap. The assembled context is guaranteed to be
    /// `total_tokens <= max_tokens`.
    pub max_tokens: usize,

    /// Per-layer floor: each layer gets at least this many tokens
    /// allocated **if the layer has any content**. The floor protects
    /// the "anchor" property — procedural and episodic shouldn't be
    /// fully evicted just because semantic returned a wall of text.
    /// Empty layers contribute zero either way.
    pub per_layer_min: usize,
}

impl TokenBudget {
    /// Production default — 4000 tokens total, 256 per layer minimum.
    /// Matches `BudgetsConfig::auto_context_token_budget` (4000) and
    /// gives each of L0/L3/L4 about 6% guaranteed share of the
    /// budget.
    pub fn production() -> Self {
        Self {
            max_tokens: 4000,
            per_layer_min: 256,
        }
    }

    /// Aggressive limits for tests. Keeps `max_tokens` small so tests
    /// can prove the trim path actually fires.
    pub fn for_tests(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            per_layer_min: max_tokens.min(64),
        }
    }

    /// Build from the live config. The per-layer floor is held at
    /// 1/16th of the cap (rounded down) to keep proportions sane
    /// across config edits.
    pub fn from_config(c: &BudgetsConfig) -> Self {
        let max = c.auto_context_token_budget;
        Self {
            max_tokens: max,
            per_layer_min: max / 16,
        }
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self::production()
    }
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
}
