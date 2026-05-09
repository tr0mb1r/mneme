//! Size-tier classification for `remember` and `update` content
//! (release-planning v2.1 §5.3). Shared by both tools so the limits,
//! response shape, and rejection text stay in lockstep.
//!
//! Tiers measured in **characters** (not tokens or bytes — see §5.5):
//!
//! | Range            | Tier         | Tool behaviour                |
//! |------------------|--------------|-------------------------------|
//! | < 500 chars      | `Normal`     | Stored. No annotation.        |
//! | 500 – 2,000      | `Advisory`   | Stored. `length_advisory` meta. |
//! | 2,000 – 10,000   | `Warning`    | Stored. `length_warning` meta. |
//! | > 10,000 chars   | `OverLimit`  | Rejected with `memory_too_large`. |
//!
//! The 10,000-character ceiling is configurable via
//! `[budgets] max_remember_chars` so users with legitimate longer-form
//! needs can extend it. The 500/2,000 advisory boundaries are
//! deliberately fixed — they represent the steady-state size guidance
//! and should not vary per-installation.

use serde_json::{Value, json};

/// Lower bound (inclusive) of the advisory tier — stored, but the
/// response carries a `length_advisory` annotation suggesting future
/// memories be more concise.
pub const TIER_ADVISORY_MIN: usize = 500;

/// Lower bound (inclusive) of the warning tier — stored, but the
/// response carries a stronger `length_warning` annotation and the
/// write is logged at `info` level.
pub const TIER_WARNING_MIN: usize = 2_000;

/// Default ceiling for `max_remember_chars`. Above this, writes are
/// rejected with a `memory_too_large` structured error per §5.4.
pub const DEFAULT_MAX_CHARS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Normal,
    Advisory,
    Warning,
    OverLimit,
}

/// Classify a content length against the configured ceiling. Tier
/// boundaries are fixed; only the over-limit ceiling varies per-config.
pub fn classify(len: usize, max_chars: usize) -> Tier {
    if len > max_chars {
        Tier::OverLimit
    } else if len >= TIER_WARNING_MIN {
        Tier::Warning
    } else if len >= TIER_ADVISORY_MIN {
        Tier::Advisory
    } else {
        Tier::Normal
    }
}

/// Per-call character count. Counts unicode scalar values (chars),
/// not bytes — matches the "characters" framing in the tool
/// description and §5.3.
pub fn count_chars(s: &str) -> usize {
    s.chars().count()
}

/// Optional `_meta` annotation for a successful write. Returns
/// `None` for `Normal` (no noise) and `OverLimit` (caller emits the
/// rejection separately). The annotation key (`length_advisory` vs
/// `length_warning`) lets agents key on the tier without parsing
/// numeric thresholds.
pub fn success_meta(tier: Tier, content_length: usize, max_chars: usize) -> Option<Value> {
    match tier {
        Tier::Normal | Tier::OverLimit => None,
        Tier::Advisory => Some(json!({
            "length_advisory": {
                "tier": "advisory",
                "content_length": content_length,
                "limit": max_chars,
                "message": "Memory stored. Future memories should be more concise — target under 500 characters.",
            }
        })),
        Tier::Warning => Some(json!({
            "length_warning": {
                "tier": "warning",
                "content_length": content_length,
                "limit": max_chars,
                "message": "Memory stored, but unusually long. Extract a key insight rather than storing source material verbatim.",
            }
        })),
    }
}

/// Build the rejection `_meta` + display message for over-limit
/// content (release-planning §5.4). Caller wraps in
/// `ToolResult::text(text).with_error().with_meta(meta)`.
pub fn rejection(content_length: usize, max_chars: usize) -> (String, Value) {
    let text = format!(
        "Memory content exceeds {max_chars} characters ({content_length} chars). \
         Mneme stores concise facts; for longer content, extract the key insight \
         or store a brief summary plus a source reference."
    );
    let meta = json!({
        "error": {
            "code": "memory_too_large",
            "content_length": content_length,
            "limit": max_chars,
            "suggestion": "Consider what specifically you want to remember. Store that fact, not the source material.",
        }
    });
    (text, meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_at_tier_boundaries() {
        // Normal tier
        assert_eq!(classify(0, DEFAULT_MAX_CHARS), Tier::Normal);
        assert_eq!(classify(499, DEFAULT_MAX_CHARS), Tier::Normal);
        // Advisory tier — inclusive lower
        assert_eq!(classify(500, DEFAULT_MAX_CHARS), Tier::Advisory);
        assert_eq!(classify(1_999, DEFAULT_MAX_CHARS), Tier::Advisory);
        // Warning tier — inclusive lower
        assert_eq!(classify(2_000, DEFAULT_MAX_CHARS), Tier::Warning);
        assert_eq!(classify(10_000, DEFAULT_MAX_CHARS), Tier::Warning);
        // Over limit — exclusive upper, the ceiling itself is still a Warning
        assert_eq!(classify(10_001, DEFAULT_MAX_CHARS), Tier::OverLimit);
    }

    #[test]
    fn classify_respects_custom_ceiling() {
        // With a higher ceiling, the advisory/warning bounds stay
        // fixed; only OverLimit shifts.
        assert_eq!(classify(50_000, 100_000), Tier::Warning);
        assert_eq!(classify(100_001, 100_000), Tier::OverLimit);
        // With a ceiling below the warning bound (unusual but legal),
        // any content above the ceiling rejects regardless of tier.
        assert_eq!(classify(1_500, 1_000), Tier::OverLimit);
    }

    #[test]
    fn count_chars_counts_unicode_scalars_not_bytes() {
        // 4 bytes UTF-8, 1 char.
        assert_eq!(count_chars("é"), 1);
        assert_eq!(count_chars("héllo"), 5);
    }

    #[test]
    fn success_meta_is_none_for_normal_and_overlimit() {
        assert!(success_meta(Tier::Normal, 100, DEFAULT_MAX_CHARS).is_none());
        assert!(success_meta(Tier::OverLimit, 20_000, DEFAULT_MAX_CHARS).is_none());
    }

    #[test]
    fn success_meta_keys_distinguish_tiers() {
        let advisory = success_meta(Tier::Advisory, 700, DEFAULT_MAX_CHARS).unwrap();
        assert!(advisory.get("length_advisory").is_some());
        assert!(advisory.get("length_warning").is_none());
        assert_eq!(advisory["length_advisory"]["content_length"], 700);
        assert_eq!(advisory["length_advisory"]["limit"], DEFAULT_MAX_CHARS);

        let warning = success_meta(Tier::Warning, 5_000, DEFAULT_MAX_CHARS).unwrap();
        assert!(warning.get("length_warning").is_some());
        assert!(warning.get("length_advisory").is_none());
        assert_eq!(warning["length_warning"]["content_length"], 5_000);
    }

    #[test]
    fn rejection_carries_structured_details() {
        let (text, meta) = rejection(15_234, DEFAULT_MAX_CHARS);
        assert!(text.contains("10000 characters"));
        assert!(text.contains("15234 chars"));
        assert_eq!(meta["error"]["code"], "memory_too_large");
        assert_eq!(meta["error"]["content_length"], 15_234);
        assert_eq!(meta["error"]["limit"], DEFAULT_MAX_CHARS);
        assert!(
            meta["error"]["suggestion"]
                .as_str()
                .unwrap()
                .contains("Store that fact")
        );
    }
}
