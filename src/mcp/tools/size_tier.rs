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

use std::sync::Arc;

use serde_json::{Value, json};

use crate::Result;
use crate::ids::MemoryId;
use crate::memory::semantic::MemoryItem;
use crate::storage::MEM_KEY_PREFIX;
use crate::storage::Storage;

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

/// Per-tier counts across the L4 corpus, plus the IDs of any
/// over-limit memories. Built by [`count_corpus`]; consumed by
/// `mneme://stats` (as `large_memory_count`) and by Q4's first-boot
/// upgrade audit (which writes `over_limit_ids` to
/// `~/.mneme/diagnostics.log` so the user can `recall` and trim them
/// without the server auto-modifying anything — verbatim principle).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CorpusSizeStats {
    /// Memories under the advisory boundary (< 500 chars).
    pub normal: u64,
    /// Memories in [500, 2,000) chars.
    pub advisory: u64,
    /// Memories in [2,000, max_chars] chars.
    pub warning: u64,
    /// Memories above the configured `max_remember_chars` ceiling.
    /// Existing oversized memories are NOT auto-modified — they
    /// remain `recall`-able.
    pub over_limit: u64,
    /// IDs of the over-limit memories. Useful for Q4's audit log
    /// so users can find what they have without spelunking.
    pub over_limit_ids: Vec<MemoryId>,
}

impl CorpusSizeStats {
    /// Total count across all tiers.
    pub fn total(&self) -> u64 {
        self.normal + self.advisory + self.warning + self.over_limit
    }

    /// JSON shape emitted on `mneme://stats` as `large_memory_count`.
    /// `over_limit_ids` is included so an agent reading stats can
    /// surface the offenders without a separate scan.
    pub fn to_json(&self) -> Value {
        json!({
            "tier_normal": self.normal,
            "tier_advisory": self.advisory,
            "tier_warning": self.warning,
            "tier_over_limit": self.over_limit,
            "over_limit_ids": self.over_limit_ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>(),
        })
    }
}

/// Walk the L4 corpus (`mem:` prefix in redb), decode each
/// `MemoryItem`, classify by content length against `max_chars`, and
/// accumulate per-tier counts. O(N) over the corpus — `mneme://stats`
/// reads are low-frequency (per-session diagnostic / per-boot audit),
/// so this is acceptable without caching for v1.1 cardinalities.
/// If that assumption breaks in v1.2+, cache invalidation hooks into
/// `remember` / `update` / `forget` are the right place to add
/// memoisation.
pub async fn count_corpus(storage: &Arc<dyn Storage>, max_chars: usize) -> Result<CorpusSizeStats> {
    let raw = storage.scan_prefix(MEM_KEY_PREFIX).await?;
    let mut counts = CorpusSizeStats::default();
    for (_k, v) in raw {
        let item: MemoryItem = postcard::from_bytes(&v).map_err(|e| {
            crate::MnemeError::Storage(format!("decode MemoryItem in size scan: {e}"))
        })?;
        let len = count_chars(&item.content);
        match classify(len, max_chars) {
            Tier::Normal => counts.normal += 1,
            Tier::Advisory => counts.advisory += 1,
            Tier::Warning => counts.warning += 1,
            Tier::OverLimit => {
                counts.over_limit += 1;
                counts.over_limit_ids.push(item.id);
            }
        }
    }
    Ok(counts)
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

    #[tokio::test]
    async fn count_corpus_classifies_seeded_memories() {
        use crate::embed::Embedder;
        use crate::embed::stub::StubEmbedder;
        use crate::memory::semantic::{MemoryKind, SemanticStore};
        use crate::storage::memory_impl::MemoryStorage;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();

        // Seed one memory in each tier against the default 10_000
        // ceiling — sizes chosen to land cleanly in Normal, Advisory,
        // Warning, and OverLimit.
        semantic
            .remember(&"n".repeat(50), MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap(); // Normal
        semantic
            .remember(&"a".repeat(700), MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap(); // Advisory
        semantic
            .remember(&"w".repeat(5_000), MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap(); // Warning
        let over_id = semantic
            .remember(&"o".repeat(15_000), MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap(); // OverLimit @ default ceiling

        let counts = count_corpus(&storage, DEFAULT_MAX_CHARS).await.unwrap();
        assert_eq!(counts.normal, 1);
        assert_eq!(counts.advisory, 1);
        assert_eq!(counts.warning, 1);
        assert_eq!(counts.over_limit, 1);
        assert_eq!(counts.over_limit_ids, vec![over_id]);
        assert_eq!(counts.total(), 4);

        // Tighter ceiling demotes the 5_000-char Warning entry to
        // OverLimit too — verifies max_chars actually flows through.
        let tight = count_corpus(&storage, 1_000).await.unwrap();
        assert_eq!(tight.normal, 1);
        assert_eq!(tight.advisory, 1); // 700 still ≤ 1_000
        assert_eq!(tight.warning, 0); // 5k > 1k → OverLimit
        assert_eq!(tight.over_limit, 2);
        assert_eq!(tight.over_limit_ids.len(), 2);
        assert_eq!(tight.total(), 4);
    }

    #[tokio::test]
    async fn count_corpus_empty_storage_is_all_zeros() {
        let storage: Arc<dyn Storage> = crate::storage::memory_impl::MemoryStorage::new();
        let counts = count_corpus(&storage, DEFAULT_MAX_CHARS).await.unwrap();
        assert_eq!(counts, CorpusSizeStats::default());
        assert_eq!(counts.total(), 0);
    }

    #[test]
    fn corpus_size_stats_to_json_emits_id_strings() {
        use ulid::Ulid;
        let mut counts = CorpusSizeStats {
            normal: 10,
            advisory: 5,
            warning: 2,
            over_limit: 1,
            over_limit_ids: vec![MemoryId(Ulid::new())],
        };
        let v = counts.to_json();
        assert_eq!(v["tier_normal"], 10);
        assert_eq!(v["tier_advisory"], 5);
        assert_eq!(v["tier_warning"], 2);
        assert_eq!(v["tier_over_limit"], 1);
        let ids = v["over_limit_ids"].as_array().unwrap();
        assert_eq!(ids.len(), 1);
        // ULID string form is 26 chars; not asserting exact value,
        // just that it serialises as a string.
        assert!(ids[0].as_str().unwrap().len() == 26);
        // total sums all four tiers.
        counts.over_limit = 3;
        assert_eq!(counts.total(), 10 + 5 + 2 + 3);
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
