//! The synthetic corpus: a small, fully deterministic memory store with
//! the shapes that the recall-quality evaluation found mneme getting
//! wrong, so they can be checked on every PR without a model or a
//! private data set.
//!
//! | Part | Count | Why it is here |
//! |---|---|---|
//! | facts (L4) | 300, 50 in each of 6 scopes | enough rows that scope filters, limits and budgets actually cut |
//! | id/path-heavy facts | 60 of the 300 (10 per scope) | exact-token lookups: file paths, env vars, error codes |
//! | stale/current pairs (L4) | 10 pairs, 20 rows | two memories about one subject; the current one was edited later |
//! | curated events (L3, hot) | 20 | answers that exist only on the timeline |
//! | old events (L3, warm) | 60 | rows consolidation moved out of the hot tier |
//! | pins (L0) | 6, 2 of them `global` | rules that must survive a scoped read |
//!
//! Everything is written through the public store API, in a fixed
//! order, with text derived from loop indices only: two runs produce the
//! same contents in the same order (ids and timestamps differ, as they
//! must). No embedding model is involved; callers open the stores with
//! the stub embedder.
//!
//! Used by `tests/quality_structural.rs` and by
//! `cargo run --example seed_storage -- corpus`.

// Shared by several test crates and one example; none uses all of it.
#![allow(dead_code)]

use mneme::Result;
use mneme::ids::{EventId, MemoryId};
use mneme::memory::episodic::EpisodicStore;
use mneme::memory::procedural::ProceduralStore;
use mneme::memory::semantic::{MemoryKind, SemanticStore, UpdatePatch};

/// The six scopes. `global` is first; the others are project scopes.
pub const SCOPES: [&str; 6] = ["global", "trading", "ops", "research", "hermes", "personal"];
pub const FACTS_PER_SCOPE: usize = 50;
/// The last this-many facts of each scope are id/path-heavy.
pub const ID_HEAVY_PER_SCOPE: usize = 10;
pub const PAIRS: usize = 10;
pub const CURATED_EVENTS: usize = 20;
pub const WARM_EVENTS: usize = 60;

/// Kinds of the curated events, cycled. All are in the default
/// `[episodic] index_kinds`.
pub const CURATED_KINDS: [&str; 5] = ["decision", "milestone", "lesson", "resolution", "summary"];

#[derive(Debug, Clone)]
pub struct Fact {
    pub id: MemoryId,
    pub scope: &'static str,
    pub kind: MemoryKind,
    pub content: String,
    /// For id/path-heavy facts: a token that appears in this fact and in
    /// no other row of the corpus.
    pub exact_token: Option<String>,
}

/// Two memories about one subject. `current` was stored after `stale`
/// and then edited, so it is the newer one by creation *and* by update.
#[derive(Debug, Clone)]
pub struct Pair {
    pub scope: &'static str,
    /// Appears in both memories and nowhere else.
    pub subject: String,
    pub stale: MemoryId,
    pub stale_content: String,
    pub current: MemoryId,
    pub current_content: String,
}

/// An event whose answer is in no L4 memory.
#[derive(Debug, Clone)]
pub struct Curated {
    pub id: EventId,
    pub scope: &'static str,
    pub kind: &'static str,
    pub content: String,
    /// Appears in this event and in no other row of the corpus.
    pub answer_token: String,
}

#[derive(Debug, Clone)]
pub struct WarmEvent {
    pub id: EventId,
    pub scope: &'static str,
    pub kind: &'static str,
    /// Appears in this event and in no other row of the corpus.
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct Pin {
    pub id: MemoryId,
    pub scope: &'static str,
    pub content: String,
}

/// What [`seed`] wrote, with the ids the stores assigned.
#[derive(Debug, Clone)]
pub struct Corpus {
    pub facts: Vec<Fact>,
    pub pairs: Vec<Pair>,
    pub curated: Vec<Curated>,
    pub warm: Vec<WarmEvent>,
    pub pins: Vec<Pin>,
}

impl Corpus {
    pub fn facts_in<'a>(&'a self, scope: &'a str) -> impl Iterator<Item = &'a Fact> {
        self.facts.iter().filter(move |f| f.scope == scope)
    }

    pub fn id_heavy(&self) -> impl Iterator<Item = &Fact> {
        self.facts.iter().filter(|f| f.exact_token.is_some())
    }

    pub fn pins_in<'a>(&'a self, scope: &'a str) -> impl Iterator<Item = &'a Pin> {
        self.pins.iter().filter(move |p| p.scope == scope)
    }

    /// Number of L4 rows: the facts plus both halves of every pair.
    pub fn semantic_rows(&self) -> usize {
        self.facts.len() + 2 * self.pairs.len()
    }

    /// The id behind a symbolic key, as used in
    /// `tests/fixtures/eval/questions.jsonl`:
    ///
    /// - `fact:<scope>:<n>`: fact `n` (0..50) of a scope;
    /// - `pair:<n>:current` / `pair:<n>:stale`;
    /// - `event:<n>`: curated (hot) event `n`;
    /// - `warm:<n>`: warm event `n`.
    pub fn resolve(&self, key: &str) -> Option<String> {
        let parts: Vec<&str> = key.split(':').collect();
        match parts.as_slice() {
            ["fact", scope, n] => {
                let n: usize = n.parse().ok()?;
                let s = SCOPES.iter().position(|x| x == scope)?;
                (n < FACTS_PER_SCOPE).then(|| self.facts[s * FACTS_PER_SCOPE + n].id.to_string())
            }
            ["pair", n, which] => {
                let pair = self.pairs.get(n.parse::<usize>().ok()?)?;
                match *which {
                    "current" => Some(pair.current.to_string()),
                    "stale" => Some(pair.stale.to_string()),
                    _ => None,
                }
            }
            ["event", n] => Some(self.curated.get(n.parse::<usize>().ok()?)?.id.to_string()),
            ["warm", n] => Some(self.warm.get(n.parse::<usize>().ok()?)?.id.to_string()),
            _ => None,
        }
    }

    /// Turn the questions template (symbolic keys in `gold`, `gold_l3`
    /// and `stale`) into a questions file for this seeded store. Ids
    /// are assigned at seed time, so a questions file with real ids
    /// cannot be checked in.
    pub fn resolve_questions(&self, template: &str) -> std::result::Result<String, String> {
        let mut out = String::new();
        for (i, line) in template.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut q: serde_json::Value =
                serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
            for field in ["gold", "gold_l3", "stale"] {
                let Some(keys) = q.get(field).and_then(|v| v.as_array()).cloned() else {
                    continue;
                };
                let mut ids = Vec::with_capacity(keys.len());
                for key in keys {
                    let key = key
                        .as_str()
                        .ok_or_else(|| format!("line {}: non-string key", i + 1))?;
                    ids.push(
                        self.resolve(key)
                            .ok_or_else(|| format!("line {}: unknown key `{key}`", i + 1))?,
                    );
                }
                q[field] = serde_json::json!(ids);
            }
            out.push_str(&q.to_string());
            out.push('\n');
        }
        Ok(out)
    }

    /// Every piece of text the corpus wrote, in write order. Two seeded
    /// stores must agree on this exactly.
    pub fn fingerprint(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(
            self.pins
                .iter()
                .map(|p| format!("pin|{}|{}", p.scope, p.content)),
        );
        out.extend(
            self.facts
                .iter()
                .map(|f| format!("fact|{}|{}|{}", f.scope, f.kind.as_str(), f.content)),
        );
        out.extend(
            self.pairs
                .iter()
                .map(|p| format!("pair|{}|{}|{}", p.scope, p.stale_content, p.current_content)),
        );
        out.extend(
            self.warm
                .iter()
                .map(|w| format!("warm|{}|{}|{}", w.scope, w.kind, w.token)),
        );
        out.extend(
            self.curated
                .iter()
                .map(|c| format!("curated|{}|{}|{}", c.scope, c.kind, c.content)),
        );
        out
    }
}

const COMPONENTS: [&str; 10] = [
    "scheduler",
    "gateway",
    "ledger",
    "indexer",
    "notifier",
    "importer",
    "cache",
    "planner",
    "reporter",
    "archiver",
];
const PROPERTIES: [&str; 4] = [
    "retry limit",
    "batch size",
    "timeout in seconds",
    "owner team",
];
const MEMORY_KINDS: [MemoryKind; 4] = [
    MemoryKind::Fact,
    MemoryKind::Decision,
    MemoryKind::Preference,
    MemoryKind::Fact,
];

fn fact_text(scope: &str, i: usize) -> (String, Option<String>) {
    if i >= FACTS_PER_SCOPE - ID_HEAVY_PER_SCOPE {
        // Identifiers, paths, env vars and error codes: the things a
        // keyword match must find exactly and an embedding blurs.
        let token = format!("{scope}_handler_{i:02}.rs");
        let text = format!(
            "src/{scope}/{token} reads MNEME_{}_{i:02}_URL and fails with E{:04} when the \
             {} is unreachable",
            scope.to_uppercase(),
            4000 + i * 7 + scope.len(),
            COMPONENTS[i % COMPONENTS.len()],
        );
        (text, Some(token))
    } else {
        let component = COMPONENTS[i % COMPONENTS.len()];
        let property = PROPERTIES[(i / COMPONENTS.len()) % PROPERTIES.len()];
        let text = format!(
            "in {scope} the {component} {property} is {} (note {i:02})",
            10 + (i * 13 + scope.len() * 5) % 90,
        );
        (text, None)
    }
}

fn pin_specs() -> [(&'static str, &'static str); 6] {
    [
        ("global", "always answer in the language the user wrote in"),
        (
            "global",
            "never run a destructive command without confirming first",
        ),
        (
            "trading",
            "trading: size every position from the risk table, never by feel",
        ),
        (
            "ops",
            "ops: change production only through the deploy pipeline",
        ),
        (
            "research",
            "research: record the verdict of every study when it closes",
        ),
        (
            "hermes",
            "hermes: load pinned rules at the start of each conversation",
        ),
    ]
}

/// Let the clock move between two events. Events are ordered by
/// timestamps with millisecond-level ties broken by the random part of
/// the id; a corpus whose order is meant to be reproducible must not
/// write two events inside one millisecond.
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
}

/// Write the corpus into the three stores. They are expected to be
/// empty; the function does not check.
pub async fn seed(
    semantic: &SemanticStore,
    procedural: &ProceduralStore,
    episodic: &EpisodicStore,
) -> Result<Corpus> {
    // L0: pins.
    let mut pins = Vec::new();
    for (scope, content) in pin_specs() {
        let id = procedural
            .pin(content.to_owned(), vec!["corpus".into()], scope.to_owned())
            .await?;
        pins.push(Pin {
            id,
            scope,
            content: content.to_owned(),
        });
    }

    // L4: 300 facts, scope by scope.
    let mut facts = Vec::with_capacity(SCOPES.len() * FACTS_PER_SCOPE);
    for scope in SCOPES {
        for i in 0..FACTS_PER_SCOPE {
            let (content, exact_token) = fact_text(scope, i);
            let kind = MEMORY_KINDS[i % MEMORY_KINDS.len()];
            let tags = vec![COMPONENTS[i % COMPONENTS.len()].to_owned()];
            let id = semantic
                .remember(&content, kind, tags, scope.to_owned())
                .await?;
            facts.push(Fact {
                id,
                scope,
                kind,
                content,
                exact_token,
            });
        }
    }

    // L4: stale/current pairs. The current memory is written second and
    // then corrected in place, which is how a real "latest value" row
    // comes to be: its content and its update time are both newer.
    let mut pairs = Vec::with_capacity(PAIRS);
    for i in 0..PAIRS {
        let scope = if i % 2 == 0 { "trading" } else { "ops" };
        let subject = format!("account-K{:02}", 11 + i * 3);
        let stale_content = format!(
            "{subject} balance is {} EUR as of 2026-08-01",
            1000 + i * 150
        );
        let draft = format!("{subject} balance is being recounted");
        let current_content = format!(
            "{subject} balance is {} EUR as of 2026-11-01",
            4800 + i * 210
        );
        let stale = semantic
            .remember(&stale_content, MemoryKind::Fact, vec![], scope.to_owned())
            .await?;
        let current = semantic
            .remember(&draft, MemoryKind::Fact, vec![], scope.to_owned())
            .await?;
        semantic
            .update(
                current,
                UpdatePatch {
                    content: Some(current_content.clone()),
                    ..UpdatePatch::default()
                },
            )
            .await?;
        // The two rows are written milliseconds apart, and auto-context
        // freshness has a 90-day half-life, so on the real clock they
        // would be equally fresh. Give the stale row the age it stands
        // for: last touched four months ago.
        semantic
            .backdate_updated_at(stale, chrono::Utc::now() - chrono::Duration::days(120))
            .await?;
        pairs.push(Pair {
            scope,
            subject,
            stale,
            stale_content,
            current,
            current_content,
        });
    }

    // L3 warm: recorded first so they are the oldest events, then moved
    // to the warm tier the way consolidation does. Every fifth one is a
    // curated kind; the rest are plain observations.
    let mut warm = Vec::with_capacity(WARM_EVENTS);
    for i in 0..WARM_EVENTS {
        let scope = SCOPES[i % SCOPES.len()];
        let kind = if i % 5 == 0 {
            "decision"
        } else {
            "observation"
        };
        let token = format!("warmnote-{i:02}");
        let payload = serde_json::json!({
            "content": format!("{token}: the {} was checked during the autumn review", COMPONENTS[i % COMPONENTS.len()]),
        });
        let id = episodic
            .record_full(kind, scope, &payload.to_string(), vec![], 1.0)
            .await?;
        episodic.promote_to_warm(id).await?;
        tick().await;
        warm.push(WarmEvent {
            id,
            scope,
            kind,
            token,
        });
    }

    // L3 hot: curated events whose answer is in no L4 memory.
    let mut curated = Vec::with_capacity(CURATED_EVENTS);
    for i in 0..CURATED_EVENTS {
        let scope = SCOPES[1 + i % (SCOPES.len() - 1)];
        let kind = CURATED_KINDS[i % CURATED_KINDS.len()];
        let answer_token = format!("kestrel-{:02}", 17 + i);
        let content = format!(
            "study {answer_token} closed: the {} change is {} for {scope}",
            COMPONENTS[(i * 3) % COMPONENTS.len()],
            if i % 3 == 0 { "rejected" } else { "accepted" },
        );
        let payload = serde_json::json!({ "content": content });
        let id = episodic
            .record_full(
                kind,
                scope,
                &payload.to_string(),
                vec!["corpus".into()],
                1.0,
            )
            .await?;
        tick().await;
        curated.push(Curated {
            id,
            scope,
            kind,
            content,
            answer_token,
        });
    }

    Ok(Corpus {
        facts,
        pairs,
        curated,
        warm,
        pins,
    })
}

/// Create a data directory at `root` and seed the corpus into it, with
/// vectors from the stub embedder. Serve it with `MNEME_EMBEDDER=stub`.
pub async fn seed_data_dir(root: &std::path::Path) -> Result<Corpus> {
    use mneme::embed::Embedder;
    use mneme::embed::stub::StubEmbedder;
    use mneme::memory::semantic::SnapshotConfig;
    use std::sync::Arc;

    mneme::cli::init::init_at(root)?;
    let keyring = mneme::crypto::OsKeyring::new();
    let (storage, aead) = mneme::crypto::boot::open_episodic_storage_and_aead(root, &keyring)?;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new());
    let semantic = SemanticStore::open_with_crypto(
        root,
        Arc::clone(&storage),
        embedder,
        SnapshotConfig::disabled(),
        aead.clone(),
    )?;
    let procedural = ProceduralStore::open_with_crypto(root, aead)?;
    let episodic = EpisodicStore::new(Arc::clone(&storage));

    let corpus = seed(&semantic, &procedural, &episodic).await?;
    semantic.shutdown().await?;
    storage.flush().await?;
    Ok(corpus)
}
