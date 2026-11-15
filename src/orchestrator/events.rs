//! Episodic candidate selection for auto-context: score, de-duplicate,
//! diversify and clip, before the budget pass in [`super::assembly`].
//!
//! Before 1.6 the episodic section was the 64 most recent events,
//! whatever the query, each charged at its full payload size. Six
//! different opening prompts got the identical event list, runs of
//! near-identical events filled it, and most of the tokens paid for
//! text the client clipped away anyway. This module is the fix:
//!
//! 1. **Weight floor.** Events at or below
//!    `[auto_context] min_event_weight` are dropped: down-weighting is
//!    how an agent marks an event as low-signal.
//! 2. **Query-conditioned score.** [`episodic_score`] with the lexical
//!    coverage the episodic index reports for the query.
//! 3. **Diversity**, walking candidates best first:
//!    exact duplicates (same kind, same payload after case and
//!    whitespace normalisation) are dropped; at most
//!    `max_events_per_kind` of one kind are kept; and an event whose
//!    token set overlaps an already-kept one by Jaccard ≥
//!    [`NEAR_DUPLICATE_JACCARD`] is dropped, as in MMR.
//! 4. **Clip.** Each survivor's payload is cut to `event_clip_chars`
//!    and its token cost is the clipped size.
//!
//! Everything here is pure: the caller fetches the candidates and the
//! lexical scores and passes `now` in.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::AutoContextConfig;
use crate::ids::EventId;
use crate::memory::episodic::EpisodicEvent;
use crate::orchestrator::budget::estimate_tokens;
use crate::orchestrator::scoring::episodic_score;

/// Token-set Jaccard similarity at which an event counts as a
/// near-duplicate of one already kept.
pub const NEAR_DUPLICATE_JACCARD: f32 = 0.6;

/// Events with fewer distinct tokens than this are never called
/// near-duplicates. Jaccard over a handful of tokens is noise: two
/// one-line events that differ in the one word that matters ("deployed
/// to staging" / "deployed to prod") are not the same event restated.
const NEAR_DUPLICATE_MIN_TOKENS: usize = 6;

/// Marks the cut in a clipped payload.
const ELLIPSIS: char = '…';

/// One event that survived selection, ready for the budget pass.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedEvent {
    /// The event, with `payload` already clipped.
    pub event: EpisodicEvent,
    /// Query-conditioned score ([`episodic_score`]).
    pub score: f32,
    /// Token cost of the clipped payload.
    pub tokens: usize,
    /// Whether the payload was clipped.
    pub truncated: bool,
}

/// Score, de-duplicate, diversify and clip `candidates`. Returns the
/// survivors best first (ties broken by id, newest first), so the
/// output is deterministic for a given input set whatever its order.
///
/// `lex` is `None` when the read carried no query. With a query it is
/// the map from [`EpisodicStore::lex_scores`]; an id absent from it
/// scores zero lexical coverage.
///
/// [`EpisodicStore::lex_scores`]: crate::memory::episodic::EpisodicStore::lex_scores
pub fn select_events(
    candidates: Vec<EpisodicEvent>,
    lex: Option<&HashMap<EventId, f32>>,
    now: DateTime<Utc>,
    cfg: &AutoContextConfig,
) -> Vec<SelectedEvent> {
    let mut scored: Vec<(EpisodicEvent, f32)> = candidates
        .into_iter()
        .filter(|e| e.retrieval_weight > cfg.min_event_weight)
        .map(|e| {
            let coverage = lex.map(|m| m.get(&e.id).copied().unwrap_or(0.0));
            let score = episodic_score(&e, now, coverage, cfg);
            (e, score)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.id.0.cmp(&a.0.id.0))
    });

    let mut seen_exact: HashSet<(String, String)> = HashSet::new();
    let mut seen_ids: HashSet<EventId> = HashSet::new();
    let mut per_kind: HashMap<String, usize> = HashMap::new();
    let mut kept_tokens: Vec<HashSet<String>> = Vec::new();
    let mut out: Vec<SelectedEvent> = Vec::new();

    for (event, score) in scored {
        // The same event can arrive twice (recent window ∪ index hits).
        if !seen_ids.insert(event.id) {
            continue;
        }
        if !seen_exact.insert((event.kind.clone(), normalise(&event.payload))) {
            continue;
        }
        // 0 means "no cap", so a config typo cannot empty the section.
        let kind_count = per_kind.get(&event.kind).copied().unwrap_or(0);
        if cfg.max_events_per_kind > 0 && kind_count >= cfg.max_events_per_kind {
            continue;
        }
        let tokens = token_set(&event_text(&event.payload));
        if tokens.len() >= NEAR_DUPLICATE_MIN_TOKENS
            && kept_tokens.iter().any(|kept| {
                kept.len() >= NEAR_DUPLICATE_MIN_TOKENS
                    && jaccard(kept, &tokens) >= NEAR_DUPLICATE_JACCARD
            })
        {
            continue;
        }

        *per_kind.entry(event.kind.clone()).or_insert(0) += 1;
        kept_tokens.push(tokens);

        let mut event = event;
        let truncated = match clip_payload(&event.payload, cfg.event_clip_chars) {
            Some(clipped) => {
                event.payload = clipped;
                true
            }
            None => false,
        };
        out.push(SelectedEvent {
            tokens: estimate_tokens(&event.payload),
            event,
            score,
            truncated,
        });
    }
    out
}

/// Cut `payload` to at most `max_chars` characters. `None` if it
/// already fits (or `max_chars` is 0, which turns clipping off).
///
/// Event payloads are JSON by convention, usually an object with the
/// prose under `content` (or `text`). Cutting the raw string would
/// leave the client with unparseable JSON, so when that field is what
/// makes the payload long, only the field is shortened and the payload
/// stays valid. Anything else (plain text, an object that is long for
/// other reasons) is cut as a string. Either way the result ends in
/// `…` and the caller flags the row `truncated`.
pub fn clip_payload(payload: &str, max_chars: usize) -> Option<String> {
    let len = payload.chars().count();
    if max_chars == 0 || len <= max_chars {
        return None;
    }
    if let Some(clipped) = clip_json_field(payload, len - max_chars, max_chars) {
        return Some(clipped);
    }
    Some(take_chars(payload, max_chars.saturating_sub(1)))
}

/// Shorten the `content` / `text` field of a JSON-object payload by
/// `overage` characters (plus one for the ellipsis). `None` if the
/// payload is not such an object, the field is too short to absorb the
/// cut, or the result still does not fit.
fn clip_json_field(payload: &str, overage: usize, max_chars: usize) -> Option<String> {
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(payload) else {
        return None;
    };
    let field = ["content", "text"]
        .into_iter()
        .find(|f| map.get(*f).is_some_and(Value::is_string))?;
    let text = map.get(field).and_then(Value::as_str)?;
    let keep = text.chars().count().checked_sub(overage + 1)?;
    if keep == 0 {
        return None;
    }
    let shortened = take_chars(text, keep);
    map.insert(field.to_owned(), Value::String(shortened));
    let out = serde_json::to_string(&Value::Object(map)).ok()?;
    (out.chars().count() <= max_chars).then_some(out)
}

/// The first `n` characters of `s`, then an ellipsis.
fn take_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    out.push(ELLIPSIS);
    out
}

/// Lowercase with whitespace runs collapsed: the exact-duplicate key.
fn normalise(payload: &str) -> String {
    payload
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The prose of an event: the `content` / `text` field of a JSON-object
/// payload, the string of a JSON-string payload, else the raw payload.
/// Keeps JSON keys and punctuation out of the near-duplicate measure.
fn event_text(payload: &str) -> String {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(map)) => ["content", "text"]
            .into_iter()
            .find_map(|f| map.get(f).and_then(Value::as_str))
            .map_or_else(|| payload.to_owned(), str::to_owned),
        Ok(Value::String(s)) => s,
        _ => payload.to_owned(),
    }
}

/// Distinct lowercase alphanumeric runs. Deliberately simpler than the
/// keyword index's tokenizer: this only measures overlap between two
/// events, it never has to agree with an index.
fn token_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    let shared = a.intersection(b).count();
    let union = a.len() + b.len() - shared;
    if union == 0 {
        return 0.0;
    }
    shared as f32 / union as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t0() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn cfg() -> AutoContextConfig {
        AutoContextConfig::default()
    }

    fn event(kind: &str, content: &str, age_hours: i64) -> EpisodicEvent {
        let ts = t0() - Duration::hours(age_hours);
        EpisodicEvent {
            id: EventId::new(),
            kind: kind.into(),
            scope: "personal".into(),
            payload: serde_json::json!({ "content": content }).to_string(),
            tags: vec![],
            retrieval_weight: 1.0,
            last_accessed: ts,
            created_at: ts,
        }
    }

    fn contents(sel: &[SelectedEvent]) -> Vec<String> {
        sel.iter()
            .map(|s| event_text(&s.event.payload))
            .collect::<Vec<_>>()
    }

    #[test]
    fn low_weight_events_are_dropped() {
        let mut quiet = event("note", "hygiene pass over the backlog", 1);
        quiet.retrieval_weight = 0.3;
        let mut louder = event("note", "decided to ship on friday", 2);
        louder.retrieval_weight = 0.31;
        let sel = select_events(vec![quiet, louder], None, t0(), &cfg());
        assert_eq!(contents(&sel), vec!["decided to ship on friday"]);
    }

    #[test]
    fn exact_duplicates_collapse_to_the_best_scored() {
        let old = event("decision", "Use  redb for storage", 50);
        let new = event("decision", "use redb for STORAGE", 1);
        let other_kind = event("lesson", "use redb for storage", 2);
        let (new_id, other_id) = (new.id, other_kind.id);
        let sel = select_events(vec![old, new, other_kind], None, t0(), &cfg());
        // Same kind + same normalised payload: one survives, the newer.
        // The same text under another kind is a different event.
        let ids: Vec<EventId> = sel.iter().map(|s| s.event.id).collect();
        assert_eq!(ids, vec![new_id, other_id]);
    }

    #[test]
    fn at_most_max_events_per_kind() {
        let mut events: Vec<EpisodicEvent> = (0..10)
            .map(|i| event("observation", &format!("obs {i}"), i))
            .collect();
        events.push(event("decision", "the one decision", 100));
        let sel = select_events(events, None, t0(), &cfg());
        let obs = sel.iter().filter(|s| s.event.kind == "observation").count();
        assert_eq!(obs, 6);
        // The cap makes room: the old decision is still there.
        assert!(sel.iter().any(|s| s.event.kind == "decision"));
        // And the six kept are the six most recent.
        assert_eq!(
            contents(&sel)[..6],
            ["obs 0", "obs 1", "obs 2", "obs 3", "obs 4", "obs 5"]
        );
    }

    #[test]
    fn zero_cap_means_no_cap() {
        let events: Vec<EpisodicEvent> = (0..10)
            .map(|i| event("observation", &format!("obs {i}"), i))
            .collect();
        let c = AutoContextConfig {
            max_events_per_kind: 0,
            ..cfg()
        };
        assert_eq!(select_events(events, None, t0(), &c).len(), 10);
    }

    #[test]
    fn near_duplicates_are_dropped_across_kinds() {
        let a = event(
            "summary",
            "nightly backtest finished: sharpe 1.4 on the momentum book",
            1,
        );
        let b = event(
            "observation",
            "nightly backtest finished: sharpe 1.5 on the momentum book",
            2,
        );
        let c = event("observation", "rotated the gateway api credentials", 3);
        let a_id = a.id;
        let c_id = c.id;
        let sel = select_events(vec![c, b, a], None, t0(), &cfg());
        let ids: Vec<EventId> = sel.iter().map(|s| s.event.id).collect();
        assert_eq!(ids, vec![a_id, c_id]);
    }

    #[test]
    fn short_events_are_never_near_duplicates() {
        let a = event("decision", "x", 1);
        let b = event("summary", "x", 2);
        assert_eq!(select_events(vec![a, b], None, t0(), &cfg()).len(), 2);
    }

    #[test]
    fn a_query_reorders_events_by_lexical_coverage() {
        let fresh_off_topic = event("note", "lunch was a sandwich", 1);
        let old_on_topic = event("decision", "pin the gateway port to 4002", 24 * 9);
        let on_id = old_on_topic.id;
        let off_id = fresh_off_topic.id;

        let no_query = select_events(
            vec![fresh_off_topic.clone(), old_on_topic.clone()],
            None,
            t0(),
            &cfg(),
        );
        assert_eq!(no_query[0].event.id, off_id);

        let lex = HashMap::from([(on_id, 1.0f32)]);
        let with_query = select_events(
            vec![fresh_off_topic, old_on_topic],
            Some(&lex),
            t0(),
            &cfg(),
        );
        assert_eq!(with_query[0].event.id, on_id);
        // The off-topic event is demoted, not removed.
        assert_eq!(with_query.len(), 2);
        assert!(with_query[1].score > 0.0);
    }

    /// The no-index world: a query is present but the index returned
    /// nothing. Order and membership equal the no-query read; only the
    /// scores shrink by β.
    #[test]
    fn an_empty_lex_map_degrades_to_recency_order() {
        let events: Vec<EpisodicEvent> = (0..5)
            .map(|i| event("note", &format!("entry number {i}"), i * 7))
            .collect();
        let none = select_events(events.clone(), None, t0(), &cfg());
        let empty = HashMap::new();
        let some = select_events(events, Some(&empty), t0(), &cfg());
        assert_eq!(
            none.iter().map(|s| s.event.id).collect::<Vec<_>>(),
            some.iter().map(|s| s.event.id).collect::<Vec<_>>()
        );
        for (n, s) in none.iter().zip(&some) {
            assert!((s.score - n.score * 0.3).abs() < 1e-6);
            assert!(s.score > 0.0);
        }
    }

    #[test]
    fn selection_does_not_depend_on_input_order() {
        let events: Vec<EpisodicEvent> = (0..12)
            .map(|i| event(if i % 2 == 0 { "a" } else { "b" }, &format!("e {i}"), 3))
            .collect();
        let mut reversed = events.clone();
        reversed.reverse();
        assert_eq!(
            select_events(events, None, t0(), &cfg()),
            select_events(reversed, None, t0(), &cfg())
        );
    }

    #[test]
    fn long_content_is_clipped_inside_valid_json() {
        let long = "word ".repeat(400);
        let e = event("summary", &long, 1);
        let full_tokens = estimate_tokens(&e.payload);
        let sel = select_events(vec![e], None, t0(), &cfg());
        assert!(sel[0].truncated);
        let payload = &sel[0].event.payload;
        assert!(payload.chars().count() <= 400);
        let v: Value = serde_json::from_str(payload).expect("still JSON");
        assert!(v["content"].as_str().unwrap().ends_with(ELLIPSIS));
        // Charged at the clipped size.
        assert_eq!(sel[0].tokens, estimate_tokens(payload));
        assert!(sel[0].tokens <= 100 && full_tokens > 400);
    }

    #[test]
    fn short_payloads_are_left_alone() {
        let e = event("note", "short", 1);
        let before = e.payload.clone();
        let sel = select_events(vec![e], None, t0(), &cfg());
        assert!(!sel[0].truncated);
        assert_eq!(sel[0].event.payload, before);
    }

    #[test]
    fn clip_falls_back_to_a_string_cut() {
        // Plain text.
        let plain = "p".repeat(50);
        let clipped = clip_payload(&plain, 10).unwrap();
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with(ELLIPSIS));
        // An object that is long because of a field we do not shorten.
        let other = serde_json::json!({ "content": "hi", "blob": "b".repeat(100) }).to_string();
        let clipped = clip_payload(&other, 20).unwrap();
        assert_eq!(clipped.chars().count(), 20);
        // Multi-byte text is cut on a character boundary.
        let cyrillic = "я".repeat(50);
        assert_eq!(clip_payload(&cyrillic, 10).unwrap().chars().count(), 10);
        // Off.
        assert!(clip_payload(&plain, 0).is_none());
        assert!(clip_payload(&plain, 50).is_none());
    }
}
