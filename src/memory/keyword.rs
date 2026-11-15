//! In-memory keyword (BM25) index — the lexical half of hybrid `recall`.
//!
//! The index is generic over its document key. `KeywordIndex` (the
//! default, keyed by [`MemoryId`]) covers L4 memory contents;
//! `KeywordIndex<EventId>` covers the curated L3 events
//! ([`EpisodicStore`](super::episodic::EpisodicStore)), which have no
//! vector index at all (ADR-0007) and so are found by their words only.
//!
//! Vector search is weakest exactly where an agent often needs strength:
//! exact names. `live_bt.py`, `D account`, an env var, a ULID, an error
//! string — embeddings smear these into "something about a script" or
//! "something about an account", and a near-identical neighbour
//! (`report_gen.py`) can outrank the real match. This index finds them
//! by their tokens.
//!
//! It lives only in memory. [`SemanticStore`](super::semantic::SemanticStore)
//! builds it from the stored items on the first recall and keeps it in
//! step on every `remember` / `update` / `forget`, so nothing new is
//! written to disk — which also means nothing new to encrypt when
//! encryption at rest is on.
//!
//! # Tokens
//!
//! Text is lowercased and split into words: runs of alphanumerics plus
//! the joiners `_ . - / : @ # +` that hold identifiers together. Each
//! word is indexed whole (`live_bt.py`, `2026-10-04`, `/var/log/bt.log`)
//! *and* as its alphanumeric parts (`live`, `bt`, `py`), so a query for
//! either form matches. Single characters are kept: "D account" has to
//! be able to match on `d`. Unicode letters (Cyrillic and so on) count
//! as alphanumeric. There's no stemming; inflected forms are left to the
//! semantic half.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::ids::MemoryId;

/// BM25 term-frequency saturation.
const K1: f32 = 1.2;
/// BM25 document-length normalisation.
const B: f32 = 0.75;

/// Characters that may appear inside a word without splitting it.
fn is_joiner(c: char) -> bool {
    matches!(c, '_' | '.' | '-' | '/' | ':' | '@' | '#' | '+')
}

/// Index terms for `text`, in order of appearance, duplicates kept
/// (term frequency matters to BM25).
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    for word in lower.split(|c: char| !(c.is_alphanumeric() || is_joiner(c))) {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        if word.is_empty() {
            continue;
        }
        let parts: Vec<&str> = word
            .split(|c: char| !c.is_alphanumeric())
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() > 1 {
            out.push(word.to_owned());
        }
        out.extend(parts.into_iter().map(str::to_owned));
    }
    out
}

/// A term that names something rather than phrasing a question: it
/// holds a joiner (`live_bt.py`, `ibkr_gateway_port`) or a digit
/// (`9090`, `01k8zq4m7xh2`).
fn is_identifier_like(term: &str) -> bool {
    term.chars().any(|c| is_joiner(c) || c.is_ascii_digit())
}

/// Plain alphanumeric word sequence, for phrase matching: `"D account"`
/// → `["d", "account"]`.
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `needle` occurs as a contiguous run of whole words in
/// `haystack`. `["d", "account"]` matches "the D account" but not
/// "a used account".
pub fn contains_phrase(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Words that phrase a question rather than name its subject. Only the
/// [strict](KeywordIndex::strict) index drops them from a query, and
/// only when the query has other words left.
const STOPWORDS: &[&str] = &[
    "a", "about", "all", "am", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by",
    "can", "could", "did", "do", "does", "for", "from", "had", "has", "have", "how", "i", "if",
    "in", "into", "is", "it", "its", "me", "my", "no", "not", "of", "on", "or", "our", "should",
    "so", "than", "that", "the", "their", "then", "there", "these", "this", "those", "to", "us",
    "was", "we", "were", "what", "when", "where", "which", "who", "why", "will", "with", "would",
    "you", "your",
];

/// What a [`KeywordIndex`] can be keyed by: [`MemoryId`],
/// [`EventId`](crate::ids::EventId).
pub trait DocKey: Copy + Eq + Hash + Ord {}
impl<T: Copy + Eq + Hash + Ord> DocKey for T {}

/// One keyword match.
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordHit<K = MemoryId> {
    pub id: K,
    /// BM25 score; only meaningful relative to other hits of the same
    /// query.
    pub score: f32,
    /// How much of the query the memory contains, in `(0, 1]`: the
    /// IDF-weighted share of the query's distinct terms it matches. A
    /// rare term (`live_bt.py`, `01K8Z…`) weighs far more than a common
    /// one (`the`, `user`), so matching only stopwords scores near 0.
    pub coverage: f32,
}

#[derive(Debug)]
pub struct KeywordIndex<K = MemoryId> {
    /// term → (memory → term frequency).
    postings: HashMap<String, HashMap<K, u32>>,
    /// memory → (distinct terms, token count). The term list lets
    /// `remove` find the postings without the original text.
    docs: HashMap<K, (Vec<String>, u32)>,
    total_tokens: u64,
    /// See [`KeywordIndex::strict`].
    strict: bool,
}

// Hand-written: `#[derive(Default)]` would demand `K: Default`.
impl<K> Default for KeywordIndex<K> {
    fn default() -> Self {
        Self {
            postings: HashMap::new(),
            docs: HashMap::new(),
            total_tokens: 0,
            strict: false,
        }
    }
}

/// Per-document tallies for one query: BM25 and the IDF mass of the
/// query terms the document contains.
struct Tally<K> {
    /// Sum of the IDF of every query term that counts (see
    /// [`KeywordIndex::tally`]).
    total_idf: f32,
    /// document → (BM25, matched IDF mass).
    scores: HashMap<K, (f32, f32)>,
}

impl<K> Tally<K> {
    fn coverage(&self, matched_idf: f32) -> f32 {
        if self.total_idf > 0.0 {
            (matched_idf / self.total_idf).min(1.0)
        } else {
            0.0
        }
    }
}

impl<K: DocKey> KeywordIndex<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// An index whose coverage counts every query term, present in the
    /// index or not.
    ///
    /// The default index leaves a query term that no document contains
    /// out of the coverage denominator, on the theory that it is
    /// phrasing. That holds when the index is the only place the answer
    /// could be, and its hits are fused with a vector score that
    /// disagrees when the match is hollow. It does not hold for an index
    /// searched *next to* another: a query about ports and accounts
    /// would "fully cover" an event whose only shared word is `the`,
    /// because none of the real terms occur in this index at all. Here
    /// an absent term counts at full rarity, and the phrasing is taken
    /// out by a fixed English stopword list instead (non-English
    /// questions get lower coverage; keyword-style queries are
    /// unaffected).
    pub fn strict() -> Self {
        Self {
            strict: true,
            ..Self::default()
        }
    }

    /// Whether `id` is indexed.
    pub fn contains(&self, id: &K) -> bool {
        self.docs.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Index `text` under `id`, replacing whatever was indexed for it.
    pub fn insert(&mut self, id: K, text: &str) {
        self.remove(id);
        let tokens = tokenize(text);
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in &tokens {
            *tf.entry(t.clone()).or_default() += 1;
        }
        let len = tokens.len() as u32;
        let terms: Vec<String> = tf.keys().cloned().collect();
        for (term, n) in tf {
            self.postings.entry(term).or_default().insert(id, n);
        }
        self.total_tokens += u64::from(len);
        self.docs.insert(id, (terms, len));
    }

    /// Drop `id`. No-op if it isn't indexed.
    pub fn remove(&mut self, id: K) {
        let Some((terms, len)) = self.docs.remove(&id) else {
            return;
        };
        self.total_tokens = self.total_tokens.saturating_sub(u64::from(len));
        for term in terms {
            if let Some(p) = self.postings.get_mut(&term) {
                p.remove(&id);
                if p.is_empty() {
                    self.postings.remove(&term);
                }
            }
        }
    }

    /// Score every document that shares a term with `query`. `None`
    /// when the index or the query is empty.
    fn tally(&self, query: &str) -> Option<Tally<K>> {
        if self.docs.is_empty() {
            return None;
        }
        let mut terms: HashSet<String> = tokenize(query).into_iter().collect();
        if self.strict && terms.iter().any(|t| !STOPWORDS.contains(&t.as_str())) {
            terms.retain(|t| !STOPWORDS.contains(&t.as_str()));
        }
        if terms.is_empty() {
            return None;
        }
        let n = self.docs.len() as f32;
        let avg_len = (self.total_tokens as f32 / n).max(1.0);
        let idf = |df: f32| (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        // The query's total weight. A term no memory contains is
        // usually phrasing ("what value should … have"): leaving it in
        // would cap every memory's coverage and drown the one term that
        // does match. Identifier-like terms are the exception — asking
        // for a `foo_bar.py` that doesn't exist must not fully match
        // every memory mentioning `py` — so those count at full rarity.
        let total_idf: f32 = terms
            .iter()
            .filter_map(|t| match self.postings.get(t) {
                Some(p) => Some(idf(p.len() as f32)),
                None if self.strict || is_identifier_like(t) => Some(idf(0.0)),
                None => None,
            })
            .sum();
        // memory → (BM25, matched IDF mass)
        let mut scores: HashMap<K, (f32, f32)> = HashMap::new();
        for term in &terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let term_idf = idf(postings.len() as f32);
            for (id, &tf) in postings {
                let dl = self.docs.get(id).map(|d| d.1).unwrap_or(0) as f32;
                let tf = tf as f32;
                let s = term_idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_len));
                let e = scores.entry(*id).or_default();
                e.0 += s;
                e.1 += term_idf;
            }
        }
        Some(Tally { total_idf, scores })
    }

    /// Up to `limit` memories containing at least one query term, best
    /// BM25 first. Ties break on id so the order is deterministic.
    pub fn search(&self, query: &str, limit: usize) -> Vec<KeywordHit<K>> {
        if limit == 0 {
            return Vec::new();
        }
        let Some(tally) = self.tally(query) else {
            return Vec::new();
        };
        let mut hits: Vec<KeywordHit<K>> = tally
            .scores
            .iter()
            .map(|(id, &(score, matched_idf))| KeywordHit {
                id: *id,
                score,
                coverage: tally.coverage(matched_idf),
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(limit);
        hits
    }

    /// [`KeywordHit::coverage`] of `query` for each of `ids`, in
    /// `(0, 1]`. Ids that are not indexed or share no term with the
    /// query are absent. The values equal what [`search`](Self::search)
    /// reports for the same query: coverage is measured against the
    /// whole index, not against `ids`.
    pub fn coverage_of(&self, query: &str, ids: &[K]) -> HashMap<K, f32> {
        let Some(tally) = self.tally(query) else {
            return HashMap::new();
        };
        ids.iter()
            .filter_map(|id| {
                let (_, matched_idf) = tally.scores.get(id)?;
                let c = tally.coverage(*matched_idf);
                (c > 0.0).then_some((*id, c))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn identifiers_are_indexed_whole_and_in_parts() {
        assert_eq!(
            tokenize("Run live_bt.py now."),
            s(&["run", "live_bt.py", "live", "bt", "py", "now"])
        );
        // Leading/trailing joiners are trimmed off the whole word.
        assert_eq!(
            tokenize("logs: /var/log/bt.log"),
            s(&["logs", "var/log/bt.log", "var", "log", "bt", "log"])
        );
    }

    #[test]
    fn single_letters_and_unicode_survive() {
        assert_eq!(tokenize("D account"), s(&["d", "account"]));
        assert_eq!(
            tokenize("Сервер на порту 8080"),
            s(&["сервер", "на", "порту", "8080"])
        );
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_a_word() {
        assert_eq!(
            tokenize("(see README.md)."),
            s(&["see", "readme.md", "readme", "md"])
        );
    }

    #[test]
    fn phrase_matching_respects_word_boundaries() {
        let hay = words("The D account balance is 5,200");
        assert!(contains_phrase(&hay, &words("d account")));
        assert!(!contains_phrase(
            &words("a used account"),
            &words("d account")
        ));
        assert!(!contains_phrase(&hay, &[]));
    }

    #[test]
    fn exact_identifier_outranks_a_lookalike() {
        let mut idx = KeywordIndex::new();
        let a = MemoryId::new();
        let b = MemoryId::new();
        let c = MemoryId::new();
        idx.insert(a, "live_bt.py runs the live backtest every 15 minutes");
        idx.insert(b, "report_gen.py runs every 15 minutes");
        idx.insert(c, "the cat is called Murka");
        let hits = idx.search("live_bt.py", 10);
        assert_eq!(hits[0].id, a);
        assert_eq!(hits[0].coverage, 1.0);
        assert!(hits.iter().all(|h| h.id != c));
    }

    #[test]
    fn rare_terms_weigh_more_than_common_ones() {
        let mut idx = KeywordIndex::new();
        let target = MemoryId::new();
        idx.insert(target, "D account is the main brokerage account");
        for i in 0..20 {
            idx.insert(MemoryId::new(), &format!("account note number {i}"));
        }
        let hits = idx.search("D account", 5);
        assert_eq!(hits[0].id, target);
    }

    #[test]
    fn insert_replaces_and_remove_forgets() {
        let mut idx = KeywordIndex::new();
        let id = MemoryId::new();
        idx.insert(id, "alpha beta");
        idx.insert(id, "gamma");
        assert!(idx.search("alpha", 5).is_empty());
        assert_eq!(idx.search("gamma", 5)[0].id, id);
        idx.remove(id);
        assert!(idx.search("gamma", 5).is_empty());
        assert!(idx.is_empty());
        assert!(
            idx.postings.is_empty(),
            "no empty posting lists left behind"
        );
        assert_eq!(idx.total_tokens, 0);
    }

    #[test]
    fn empty_query_and_limit_return_nothing() {
        let mut idx = KeywordIndex::new();
        idx.insert(MemoryId::new(), "alpha");
        assert!(idx.search("   ", 5).is_empty());
        assert!(idx.search("alpha", 0).is_empty());
    }

    #[test]
    fn absent_phrasing_words_do_not_dilute_coverage() {
        let mut idx = KeywordIndex::new();
        let target = MemoryId::new();
        idx.insert(target, "Set IBKR_GATEWAY_PORT=4002 for the paper gateway");
        idx.insert(MemoryId::new(), "Set IBKR_GATEWAY_HOST to localhost");
        let hits = idx.search("what value should IBKR_GATEWAY_PORT have", 5);
        assert_eq!(hits[0].id, target);
        assert!(hits[0].coverage > 0.9, "coverage {}", hits[0].coverage);
    }

    #[test]
    fn index_is_generic_over_the_key() {
        use crate::ids::EventId;
        let mut idx: KeywordIndex<EventId> = KeywordIndex::new();
        let a = EventId::new();
        let b = EventId::new();
        idx.insert(a, "switched the backtest to live_bt.py");
        idx.insert(b, "lunch was fine");
        let hits = idx.search("live_bt.py", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a);
        assert!(idx.contains(&a) && idx.contains(&b));
        idx.remove(a);
        assert!(!idx.contains(&a));
    }

    #[test]
    fn coverage_of_matches_search_and_stays_in_range() {
        let mut idx = KeywordIndex::new();
        let full = MemoryId::new();
        let partial = MemoryId::new();
        let none = MemoryId::new();
        idx.insert(full, "rotate the gateway token every month");
        idx.insert(partial, "the gateway is on port 4002");
        idx.insert(none, "the cat is called Murka");
        let unindexed = MemoryId::new();
        let query = "gateway token rotate";

        let cov = idx.coverage_of(query, &[full, partial, none, unindexed]);
        assert_eq!(cov.len(), 2, "non-matching and unindexed ids are absent");
        assert_eq!(cov[&full], 1.0);
        assert!(cov[&partial] > 0.0 && cov[&partial] < 1.0);
        assert!(cov.values().all(|c| (0.0..=1.0).contains(c)));

        for hit in idx.search(query, 10) {
            assert_eq!(cov[&hit.id], hit.coverage);
        }
        assert!(idx.coverage_of("   ", &[full]).is_empty());
        assert!(
            KeywordIndex::<MemoryId>::new()
                .coverage_of(query, &[full])
                .is_empty()
        );
    }

    #[test]
    fn strict_index_counts_absent_terms_and_drops_stopwords() {
        let lenient = |docs: &[(MemoryId, &str)]| {
            let mut idx = KeywordIndex::new();
            for (id, text) in docs {
                idx.insert(*id, text);
            }
            idx
        };
        let strict = |docs: &[(MemoryId, &str)]| {
            let mut idx = KeywordIndex::strict();
            for (id, text) in docs {
                idx.insert(*id, text);
            }
            idx
        };
        let dog = MemoryId::new();
        let port = MemoryId::new();
        let docs = [
            (dog, "walked the dog"),
            (port, "paper gateway port is 4002"),
        ];
        let query = "which port belongs to the paper account";

        // Lenient: `the` is the only query word the dog note has, the
        // rest of what it lacks is not in the index either, so it
        // "covers" a large share of the query.
        let hits = lenient(&docs).search(query, 5);
        let dog_cov = hits.iter().find(|h| h.id == dog).map(|h| h.coverage);
        assert!(dog_cov.is_some_and(|c| c > 0.2), "{dog_cov:?}");

        // Strict: stopwords are not query terms, so the dog note does
        // not match at all, and the absent `belongs` / `account` hold
        // the real match below full coverage.
        let hits = strict(&docs).search(query, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, port);
        assert!(hits[0].coverage > 0.1 && hits[0].coverage < 0.8, "{hits:?}");

        // A query of nothing but stopwords keeps them.
        assert_eq!(strict(&docs).search("the", 5)[0].id, dog);
        // Exact names still cover fully.
        let hits = strict(&[(port, "run live_bt.py nightly")]).search("live_bt.py", 5);
        assert_eq!(hits[0].coverage, 1.0);
    }

    #[test]
    fn an_absent_identifier_still_counts() {
        let mut idx = KeywordIndex::new();
        idx.insert(MemoryId::new(), "report_gen.py emails the report");
        let hits = idx.search("foo_bar.py", 5);
        assert!(hits.iter().all(|h| h.coverage < 0.5), "{hits:?}");
    }
}
