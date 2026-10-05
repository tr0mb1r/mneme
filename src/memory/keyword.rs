//! In-memory keyword (BM25) index over L4 memory contents — the lexical
//! half of hybrid `recall`.
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

/// One keyword match.
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordHit {
    pub id: MemoryId,
    /// BM25 score; only meaningful relative to other hits of the same
    /// query.
    pub score: f32,
    /// How much of the query the memory contains, in `(0, 1]`: the
    /// IDF-weighted share of the query's distinct terms it matches. A
    /// rare term (`live_bt.py`, `01K8Z…`) weighs far more than a common
    /// one (`the`, `user`), so matching only stopwords scores near 0.
    pub coverage: f32,
}

#[derive(Debug, Default)]
pub struct KeywordIndex {
    /// term → (memory → term frequency).
    postings: HashMap<String, HashMap<MemoryId, u32>>,
    /// memory → (distinct terms, token count). The term list lets
    /// `remove` find the postings without the original text.
    docs: HashMap<MemoryId, (Vec<String>, u32)>,
    total_tokens: u64,
}

impl KeywordIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Index `text` under `id`, replacing whatever was indexed for it.
    pub fn insert(&mut self, id: MemoryId, text: &str) {
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
    pub fn remove(&mut self, id: MemoryId) {
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

    /// Up to `limit` memories containing at least one query term, best
    /// BM25 first. Ties break on id so the order is deterministic.
    pub fn search(&self, query: &str, limit: usize) -> Vec<KeywordHit> {
        if limit == 0 || self.docs.is_empty() {
            return Vec::new();
        }
        let terms: HashSet<String> = tokenize(query).into_iter().collect();
        if terms.is_empty() {
            return Vec::new();
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
                None if is_identifier_like(t) => Some(idf(0.0)),
                None => None,
            })
            .sum();
        // memory → (BM25, matched IDF mass)
        let mut scores: HashMap<MemoryId, (f32, f32)> = HashMap::new();
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
        let mut hits: Vec<KeywordHit> = scores
            .into_iter()
            .map(|(id, (score, matched_idf))| KeywordHit {
                id,
                score,
                coverage: if total_idf > 0.0 {
                    (matched_idf / total_idf).min(1.0)
                } else {
                    0.0
                },
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
    fn an_absent_identifier_still_counts() {
        let mut idx = KeywordIndex::new();
        idx.insert(MemoryId::new(), "report_gen.py emails the report");
        let hits = idx.search("foo_bar.py", 5);
        assert!(hits.iter().all(|h| h.coverage < 0.5), "{hits:?}");
    }
}
