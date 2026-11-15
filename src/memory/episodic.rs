//! Phase 4 L3 — episodic memory.
//!
//! Episodic events are short, time-stamped records of *what happened*
//! during a session: a tool was called, a user said something, the
//! agent reached a checkpoint. Distinct from L4 semantic memories
//! (`memory::semantic`) which are *facts the user asked us to
//! remember*; we store far more episodic events than semantic ones,
//! and they age out faster (consolidation moves them through hot →
//! warm → cold tiers per spec §8).
//!
//! # Storage
//!
//! Events live in the same redb database as semantic metadata, under
//! the `epi:` key prefix. The key suffix is the raw 16-byte ULID, so
//! prefix scans iterate in chronological order without a side index.
//!
//! ```text
//! key   = b"epi:" || event_id.0.to_bytes()        (20 bytes)
//! value = postcard(EpisodicEvent)
//! ```
//!
//! # Ranking
//!
//! Per the implementation plan §3 Phase 4 deliverable
//! "retrieval_weight + last_accessed scoring", `recall_recent` orders
//! events lexicographically by:
//!
//!   1. `last_accessed`  — newer first (this is the dominant signal)
//!   2. `retrieval_weight` — higher first, breaks ties between peers
//!      whose `last_accessed` lands in the same millisecond
//!   3. `created_at`     — newer first, deterministic final tiebreaker
//!
//! `retrieval_weight` is a per-event constant set at `record()` time
//! (default 1.0); the agent can lower it for chatty/low-signal events
//! so they fall behind their high-signal peers. `last_accessed` is
//! bumped by `touch()` whenever an event is consulted, surfacing
//! recently-revisited items above pure-chronological recency. We
//! deliberately *don't* apply an exponential time-decay multiplier:
//! at the v0.1 cardinalities we expect (~thousands of events per
//! scope), straight-recency ordering is both sufficient and easier
//! to reason about in tests. Decay can come back as a tunable knob
//! when an event store grows past the hot-tier window without the
//! consolidation task pruning it.
//!
//! # Tiers
//!
//! `EpisodicStore` writes to the *hot* tier (`epi:`). The
//! consolidation task (`memory::consolidation`) walks that prefix
//! periodically and rekeys aged-out events to the *warm* tier
//! (`wepi:`, same redb), then later folds them into a cold zstd
//! archive (see `storage::archive`).
//!
//! Reads cover hot and warm. [`EpisodicStore::recall_recent`] scans
//! the warm prefix when asked to ([`WarmRead::Always`]) or, by
//! default, when the requested time window reaches back past the
//! hot-to-warm threshold ([`WarmRead::Auto`]).
//! [`EpisodicStore::list_hot_and_warm`] is the full-dump reader used
//! by `export`. The cold archive is not read from here.
//!
//! # Keyword index
//!
//! L3 events are not embedded (ADR-0007), so until 1.6 they could be
//! listed by time but not searched by content. The store now keeps an
//! in-memory [`KeywordIndex`] over the hot and warm events whose kind is
//! in `[episodic] index_kinds` (decisions, milestones, lessons and the
//! like; `tool_call` rows are left out on purpose). The indexed text is
//! `payload.content`, else `payload.text`, plus the tags; see
//! [`indexable_text`].
//!
//! The index is built from storage on first use and kept in step by
//! `record_full`, `forget`, `rewrite_payloads` and the consolidation
//! deletes. Moving a row hot → warm does not touch it: the index is
//! keyed by event id, which does not change. Nothing is written to
//! disk. Clones of a store share one index.
//!
//! [`EpisodicStore::search`] confirms every index hit against storage
//! and drops entries whose row is gone, so an index that has drifted
//! (a row removed through another `EpisodicStore` over the same
//! storage, say) repairs itself instead of returning ghosts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::EventId;
use crate::memory::activity::ActivityCounter;
use crate::memory::keyword::KeywordIndex;
use crate::storage::Storage;
use crate::{MnemeError, Result};

/// Hot-tier key prefix in redb. Mirrors the `mem:` prefix used by
/// [`crate::memory::semantic`] so a future GC sweep can disambiguate
/// metadata records from episodic ones in a single `scan_prefix(b"")`.
pub const EPI_KEY_PREFIX: &[u8] = b"epi:";

/// Warm-tier key prefix. Phase 4 §3 consolidation rekeys events
/// from `epi:` to `wepi:` once they age past the hot window. Layered
/// in the same redb so a single `Storage` handle can serve both
/// tiers; reads merge results from both prefixes.
pub const WARM_KEY_PREFIX: &[u8] = b"wepi:";

/// Which tier a row was read from. Surfaced on `recall_recent` and
/// `export` rows so a caller can tell an aged-out event from a current
/// one. The store itself only ever returns `Hot` and `Warm`; `Cold`
/// tags rows a tool read from the cold archive on request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Hot,
    Warm,
    Cold,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Hot => "hot",
            Tier::Warm => "warm",
            Tier::Cold => "cold",
        }
    }
}

/// An event plus the tier it was read from.
#[derive(Debug, Clone, PartialEq)]
pub struct TieredEvent {
    pub event: EpisodicEvent,
    pub tier: Tier,
}

/// `recall_recent`'s order: `last_accessed` newest first, then higher
/// `retrieval_weight`, then `created_at` newest first.
pub fn recency_order(a: &EpisodicEvent, b: &EpisodicEvent) -> std::cmp::Ordering {
    // Primary: most-recently-touched wins.
    b.last_accessed
        .cmp(&a.last_accessed)
        // Secondary: higher retrieval_weight wins among equally-recent
        // peers. f32 comparison can produce NaN; treat NaN as ::Equal so
        // we don't panic on pathological inputs (record_weighted already
        // rejects out-of-range values, so this is defensive).
        .then_with(|| {
            b.retrieval_weight
                .partial_cmp(&a.retrieval_weight)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        // Final deterministic tiebreaker.
        .then_with(|| b.created_at.cmp(&a.created_at))
}

/// `export`'s order: `created_at` newest first, ties broken by id
/// (newest first) so the order does not depend on a row's tier.
pub fn newest_first(a: &EpisodicEvent, b: &EpisodicEvent) -> std::cmp::Ordering {
    b.created_at
        .cmp(&a.created_at)
        .then_with(|| b.id.cmp(&a.id))
}

/// Append `cold` rows, tagged [`Tier::Cold`], to rows read from redb.
/// A cold row whose id is already in `live` is dropped: warm → cold
/// writes the bundle before deleting the warm row, so a crash in
/// between leaves the event in both, and the redb copy is the one that
/// can still be forgotten.
pub fn with_cold_rows(mut live: Vec<TieredEvent>, cold: Vec<EpisodicEvent>) -> Vec<TieredEvent> {
    let seen: std::collections::HashSet<EventId> = live.iter().map(|t| t.event.id).collect();
    live.extend(
        cold.into_iter()
            .filter(|e| !seen.contains(&e.id))
            .map(|event| TieredEvent {
                event,
                tier: Tier::Cold,
            }),
    );
    live
}

/// Whether [`EpisodicStore::recall_recent`] scans the warm tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WarmRead {
    /// Scan warm only when the time window can contain warm rows:
    /// `since` is older than `now - hot_to_warm_days`, or only
    /// `until` is set (the window is open to the past). A call with
    /// no time bound stays hot-only, so "what just happened?" reads
    /// cost what they did before.
    ///
    /// The rule is exact for a constant threshold: a row is promoted
    /// on `last_accessed <= now - hot_to_warm_days` and
    /// `created_at <= last_accessed`, so no warm row is newer than
    /// the cutoff. If the threshold was raised after rows were
    /// promoted, pass [`WarmRead::Always`] to reach the younger ones.
    #[default]
    Auto,
    /// Always scan warm.
    Always,
    /// Never scan warm (the pre-1.5.2 behaviour).
    Never,
}

/// Minimum age of `last_accessed` before
/// [`EpisodicStore::touch_if_stale`] writes a new one when called from
/// `recall`. A touch is a WAL-synced write made on behalf of a read, so
/// an event recalled many times a day is refreshed at most once a day.
pub const RECALL_TOUCH_DEBOUNCE_HOURS: i64 = 24;

/// How many times [`EpisodicStore`] rebuilds its keyword index when
/// writes keep landing during the build, before it keeps what it has.
const INDEX_BUILD_ATTEMPTS: usize = 8;

/// The text of `event` that the keyword index covers: `payload.content`
/// if the payload is a JSON object with that string field, else
/// `payload.text`, else every top-level string value of the object; a
/// payload that is a bare JSON string, or not JSON at all, is used
/// whole. Tags are appended, one per line, so a tag is searchable too.
pub fn indexable_text(event: &EpisodicEvent) -> String {
    use serde_json::Value;
    let mut text = match serde_json::from_str::<Value>(&event.payload) {
        Ok(Value::Object(map)) => {
            let field = |name: &str| map.get(name).and_then(Value::as_str).map(str::to_owned);
            field("content")
                .or_else(|| field("text"))
                .unwrap_or_else(|| {
                    map.values()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
        }
        Ok(Value::String(s)) => s,
        _ => event.payload.clone(),
    };
    for tag in &event.tags {
        text.push('\n');
        text.push_str(tag);
    }
    text
}

/// One lexical match from [`EpisodicStore::search_tiered`].
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalHit {
    pub event: EpisodicEvent,
    pub tier: Tier,
    /// IDF-weighted share of the query the event contains, in `(0, 1]`;
    /// the number [`EpisodicStore::lex_scores`] reports.
    pub coverage: f32,
    /// BM25 score, only meaningful against other hits of the same query.
    pub bm25: f32,
}

/// The lazily built keyword index plus a counter of writes, shared by
/// every clone of a store.
#[derive(Default)]
struct SharedIndex {
    inner: RwLock<Option<KeywordIndex<EventId>>>,
    /// Bumped, under the write lock, by every change to the indexed
    /// rows. A build compares it before its storage scan and at install
    /// time and starts over if a write slipped in between.
    generation: AtomicU64,
}

impl SharedIndex {
    fn read<T>(&self, f: impl FnOnce(Option<&KeywordIndex<EventId>>) -> T) -> T {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        f(guard.as_ref())
    }

    /// Note a change and apply it to the index if one is built.
    fn mutate(&self, f: impl FnOnce(&mut KeywordIndex<EventId>)) {
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(idx) = guard.as_mut() {
            f(idx);
        }
    }
}

fn default_index_kinds() -> Arc<[String]> {
    crate::config::EpisodicConfig::default().index_kinds.into()
}

/// Default `retrieval_weight` when the caller doesn't specify one.
/// Equal-weight events sort purely by recency.
pub const DEFAULT_RETRIEVAL_WEIGHT: f32 = 1.0;

/// One row in the episodic hot tier.
///
/// `kind` is a free-form string (e.g. `"tool_call"`, `"user_message"`,
/// `"checkpoint"`) — keeping it un-enumerated avoids forcing a schema
/// migration every time an agent invents a new event class.
/// `MemoryKind` (the enum) is reserved for `MemoryItem` and
/// `PinnedItem` where the four canonical types make sense.
///
/// `tags` mirrors the same field on
/// [`crate::memory::semantic::MemoryItem`] and
/// [`crate::memory::procedural::PinnedItem`] — every memory layer
/// can be tagged uniformly. Loaded with `#[serde(default)]` so old
/// postcard rows written before this field existed still decode.
///
/// `payload` is opaque to this module — typically a JSON string the
/// caller encoded itself. We deliberately don't serialise
/// `serde_json::Value` directly because postcard can't round-trip its
/// self-describing variants. [`EpisodicStore::record_json`] is a
/// convenience that takes a `serde_json::Value` and stringifies it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpisodicEvent {
    pub id: EventId,
    pub kind: String,
    pub scope: String,
    pub payload: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Per-event constant in `[0.0, 1.0]`. Higher values rank above
    /// lower ones at the same `last_accessed`. Default
    /// [`DEFAULT_RETRIEVAL_WEIGHT`] (1.0).
    pub retrieval_weight: f32,
    /// Bumped on `touch()` whenever the event is consulted. Drives
    /// the recency-decay factor in `recall_recent`.
    pub last_accessed: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl EpisodicEvent {
    /// Parse `payload` as JSON. Errors are surfaced to the caller —
    /// some events store plain text instead of JSON, and we don't want
    /// to lie about that.
    pub fn payload_json(&self) -> Result<serde_json::Value> {
        serde_json::from_str(&self.payload)
            .map_err(|e| MnemeError::Storage(format!("payload of event {}: {e}", self.id)))
    }
}

/// Optional filters for [`EpisodicStore::recall_recent`].
///
/// `since` and `until` form a half-open `[since, until)` interval
/// against `EpisodicEvent::created_at`. Either bound may be omitted.
/// Time-range filtering is in-memory after the hot-tier prefix scan —
/// hot-tier cardinality is bounded by consolidation so the cost stays
/// proportional to the prefix size, not the bounded slice.
#[derive(Debug, Clone, Default)]
pub struct RecentFilters {
    pub scope: Option<String>,
    pub kind: Option<String>,
    /// Include-only list: when non-empty an event passes only if its
    /// kind equals one of these. Combines (AND) with `kind`.
    pub kinds: Vec<String>,
    /// Exclude list: an event whose kind equals any of these is
    /// skipped. Checked per row with plain string compares, so the
    /// scan loop stays allocation-free.
    pub exclude_kinds: Vec<String>,
    /// Fixed exclusions that need no per-call allocation (the
    /// orchestrator's `DEFAULT_EXCLUDED_KINDS`). Applied on top of
    /// `exclude_kinds`; empty by default.
    pub exclude_kinds_static: &'static [&'static str],
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// Warm-tier policy; [`WarmRead::Auto`] by default.
    pub warm: WarmRead,
    /// Include-only list of scopes: when non-empty an event passes only
    /// if its scope equals one of these. Combines (AND) with `scope`.
    pub scopes: Vec<String>,
    /// Every tag listed here must be on the event (AND, exact,
    /// case-sensitive), as for `recall`'s `tags`. Empty: no constraint.
    pub tags: Vec<String>,
}

impl RecentFilters {
    /// Whether `event` passes every populated filter. The `warm` policy
    /// is not a row predicate and is not checked here.
    pub fn matches(&self, event: &EpisodicEvent) -> bool {
        if let Some(ref s) = self.scope
            && &event.scope != s
        {
            return false;
        }
        if !self.scopes.is_empty() && !self.scopes.contains(&event.scope) {
            return false;
        }
        if !self.tags.iter().all(|t| event.tags.contains(t)) {
            return false;
        }
        if let Some(ref k) = self.kind
            && &event.kind != k
        {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&event.kind) {
            return false;
        }
        if self.exclude_kinds.contains(&event.kind)
            || self.exclude_kinds_static.contains(&event.kind.as_str())
        {
            return false;
        }
        if let Some(since) = self.since
            && event.created_at < since
        {
            return false;
        }
        if let Some(until) = self.until
            && event.created_at >= until
        {
            return false;
        }
        true
    }
}

/// Single-scan summary of the hot tier's age profile, produced by
/// [`EpisodicStore::hot_age_summary`]. Lets `stats` report how close
/// the oldest event is to the hot-to-warm threshold without a second
/// pass over the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotAgeSummary {
    /// Rows under the hot prefix.
    pub count: usize,
    /// Minimum `last_accessed` across the hot tier (the field
    /// consolidation ages on); `None` when the tier is empty.
    pub oldest_last_accessed: Option<DateTime<Utc>>,
    /// Rows with `last_accessed <= cutoff`, i.e. what the next
    /// consolidation pass would promote to warm.
    pub eligible: usize,
}

/// Reader/writer against the redb-backed [`Storage`] handle. Writes
/// go to the hot tier; reads cover hot and warm.
///
/// Cheap to construct (just clones the `Arc<dyn Storage>`) — call
/// sites can hold one per request if that simplifies threading.
#[derive(Clone)]
pub struct EpisodicStore {
    storage: Arc<dyn Storage>,
    /// Bumped on every user-driven append (`record_*`). Read by the
    /// L3 consolidation scheduler to gate its passes; consolidation's
    /// own `promote_to_warm` / `delete_warm` paths don't bump this so
    /// the scheduler can't mistake itself for fresh user activity.
    activity: Arc<ActivityCounter>,
    /// The consolidation threshold, used only to decide when a
    /// [`WarmRead::Auto`] read has to look at the warm tier.
    hot_to_warm_days: u32,
    /// Keyword index over the events of `index_kinds`. `None` inside
    /// until the first search builds it.
    index: Arc<SharedIndex>,
    /// `[episodic] index_kinds`.
    index_kinds: Arc<[String]>,
    /// `[episodic] touch_on_recall`. Read by the `recall` tool.
    touch_on_recall: bool,
}

impl EpisodicStore {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            activity: ActivityCounter::new(),
            hot_to_warm_days: crate::config::DEFAULT_HOT_TO_WARM_DAYS,
            index: Arc::new(SharedIndex::default()),
            index_kinds: default_index_kinds(),
            touch_on_recall: crate::config::EpisodicConfig::default().touch_on_recall,
        }
    }

    /// Set which event kinds the keyword index covers, from `[episodic]
    /// index_kinds`. Call it while building the store: it starts a
    /// fresh, unbuilt index, which is no longer shared with clones made
    /// earlier.
    pub fn with_index_kinds(mut self, kinds: Vec<String>) -> Self {
        self.index_kinds = kinds.into();
        self.index = Arc::new(SharedIndex::default());
        self
    }

    /// Set `[episodic] touch_on_recall`: whether `recall` refreshes the
    /// `last_accessed` of the episodic rows it returns.
    pub fn with_touch_on_recall(mut self, on: bool) -> Self {
        self.touch_on_recall = on;
        self
    }

    /// `[episodic] touch_on_recall` as configured on this store.
    pub fn touch_on_recall(&self) -> bool {
        self.touch_on_recall
    }

    /// Whether events of `kind` are keyword-indexed.
    pub fn is_indexed_kind(&self, kind: &str) -> bool {
        self.index_kinds.iter().any(|k| k == kind)
    }

    /// Build the keyword index from storage if it is not built yet.
    async fn ensure_index(&self) -> Result<()> {
        for attempt in 1..=INDEX_BUILD_ATTEMPTS {
            if self.index.read(|idx| idx.is_some()) {
                return Ok(());
            }
            let generation = self.index.generation.load(Ordering::SeqCst);
            let rows = self
                .scan_tiers(true, |e| self.is_indexed_kind(&e.kind))
                .await?;
            // Strict coverage: this index is searched next to L4's, so
            // a term it has never seen is a miss, not phrasing.
            let mut built = KeywordIndex::strict();
            for row in &rows {
                built.insert(row.event.id, &indexable_text(&row.event));
            }
            if self.install_index(built, generation, attempt == INDEX_BUILD_ATTEMPTS) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Install `built` unless a write landed since `generation` was
    /// read (then the build is missing a row, which nothing would
    /// repair, and the caller scans again). `last_attempt` installs
    /// regardless. Returns whether an index is now in place.
    fn install_index(
        &self,
        built: KeywordIndex<EventId>,
        generation: u64,
        last_attempt: bool,
    ) -> bool {
        let mut guard = self.index.inner.write().unwrap_or_else(|p| p.into_inner());
        if guard.is_some() {
            return true;
        }
        let raced = self.index.generation.load(Ordering::SeqCst) != generation;
        if raced && !last_attempt {
            return false;
        }
        if raced {
            tracing::warn!(
                "episodic keyword index built while writes kept landing; \
                 rows written during the build may be unsearchable until restart"
            );
        }
        *guard = Some(built);
        true
    }

    /// Number of events in the keyword index, or `None` if it has not
    /// been built yet. Diagnostics and tests.
    pub fn indexed_len(&self) -> Option<usize> {
        self.index.read(|idx| idx.map(KeywordIndex::len))
    }

    /// Look `id` up in hot, then warm, reporting the tier.
    async fn find_tiered(&self, id: EventId) -> Result<Option<TieredEvent>> {
        for (key, tier) in [(epi_key(&id), Tier::Hot), (warm_key(&id), Tier::Warm)] {
            if let Some(bytes) = self.storage.get(&key).await? {
                let event = postcard::from_bytes(&bytes)
                    .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent {id}: {e}")))?;
                return Ok(Some(TieredEvent { event, tier }));
            }
        }
        Ok(None)
    }

    /// Set the hot-to-warm threshold from `[consolidation]
    /// hot_to_warm_days` so [`WarmRead::Auto`] matches what the
    /// consolidation pass actually does. Defaults to
    /// [`crate::config::DEFAULT_HOT_TO_WARM_DAYS`].
    pub fn with_hot_to_warm_days(mut self, days: u32) -> Self {
        self.hot_to_warm_days = days;
        self
    }

    /// Resolve the warm policy of `filters` against the clock.
    fn reads_warm(&self, filters: &RecentFilters, now: DateTime<Utc>) -> bool {
        match filters.warm {
            WarmRead::Always => true,
            WarmRead::Never => false,
            WarmRead::Auto => match (filters.since, filters.until) {
                (Some(since), _) => {
                    since < now - chrono::Duration::days(i64::from(self.hot_to_warm_days))
                }
                (None, Some(_)) => true,
                (None, None) => false,
            },
        }
    }

    /// Hand out the shared activity counter (see field docs).
    pub fn activity_counter(&self) -> Arc<ActivityCounter> {
        Arc::clone(&self.activity)
    }

    /// Append a new episodic event with the default retrieval weight
    /// and no tags. `payload` is stored verbatim (no encoding/escaping);
    /// typically a JSON string the caller produced via
    /// `serde_json::to_string`.
    pub async fn record(&self, kind: &str, scope: &str, payload: &str) -> Result<EventId> {
        self.record_full(kind, scope, payload, vec![], DEFAULT_RETRIEVAL_WEIGHT)
            .await
    }

    /// Convenience wrapper: stringify a `serde_json::Value` and call
    /// [`record`](Self::record).
    pub async fn record_json(
        &self,
        kind: &str,
        scope: &str,
        payload: &serde_json::Value,
    ) -> Result<EventId> {
        let s = serde_json::to_string(payload)
            .map_err(|e| MnemeError::Storage(format!("encode payload as JSON: {e}")))?;
        self.record(kind, scope, &s).await
    }

    /// Append a new event with an explicit retrieval weight (no
    /// tags). Use this for events that should rank above or below
    /// their peers — for instance, agent reflections vs. raw tool
    /// outputs. Equivalent to
    /// `record_full(kind, scope, payload, vec![], retrieval_weight)`.
    pub async fn record_weighted(
        &self,
        kind: &str,
        scope: &str,
        payload: &str,
        retrieval_weight: f32,
    ) -> Result<EventId> {
        self.record_full(kind, scope, payload, vec![], retrieval_weight)
            .await
    }

    /// Most general append. Takes every populated field; the simpler
    /// `record` / `record_weighted` wrappers exist so the common
    /// cases stay readable.
    pub async fn record_full(
        &self,
        kind: &str,
        scope: &str,
        payload: &str,
        tags: Vec<String>,
        retrieval_weight: f32,
    ) -> Result<EventId> {
        if !(0.0..=1.0).contains(&retrieval_weight) {
            return Err(MnemeError::Storage(format!(
                "retrieval_weight {retrieval_weight} out of [0,1]"
            )));
        }
        let now = Utc::now();
        let event = EpisodicEvent {
            id: EventId::new(),
            kind: kind.to_owned(),
            scope: scope.to_owned(),
            payload: payload.to_owned(),
            tags,
            retrieval_weight,
            last_accessed: now,
            created_at: now,
        };
        let key = epi_key(&event.id);
        let value = postcard::to_allocvec(&event)
            .map_err(|e| MnemeError::Storage(format!("encode EpisodicEvent: {e}")))?;
        self.storage.put(&key, &value).await?;
        if self.is_indexed_kind(&event.kind) {
            self.index
                .mutate(|idx| idx.insert(event.id, &indexable_text(&event)));
        }
        self.activity.bump();
        Ok(event.id)
    }

    /// [`touch`](Self::touch), but only when the event is in the hot
    /// tier and its `last_accessed` is at least `min_age` old. Returns
    /// whether a write happened. A missing or warm id is `Ok(false)`:
    /// this is the best-effort refresh `recall` runs after it has
    /// answered, and a warm row is not pulled back to hot by a read.
    pub async fn touch_if_stale(&self, id: EventId, min_age: chrono::Duration) -> Result<bool> {
        let key = epi_key(&id);
        let Some(bytes) = self.storage.get(&key).await? else {
            return Ok(false);
        };
        let mut event: EpisodicEvent = postcard::from_bytes(&bytes)
            .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent {id}: {e}")))?;
        let now = Utc::now();
        if now - event.last_accessed < min_age {
            return Ok(false);
        }
        event.last_accessed = now;
        let value = postcard::to_allocvec(&event)
            .map_err(|e| MnemeError::Storage(format!("re-encode EpisodicEvent: {e}")))?;
        self.storage.put(&key, &value).await?;
        Ok(true)
    }

    /// Bump `last_accessed` to "now". Used by tools/resources that
    /// just *read* an event but want to keep it warm for ranking.
    pub async fn touch(&self, id: EventId) -> Result<()> {
        let key = epi_key(&id);
        let Some(bytes) = self.storage.get(&key).await? else {
            return Err(MnemeError::Storage(format!("event {id} not found")));
        };
        let mut event: EpisodicEvent = postcard::from_bytes(&bytes)
            .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent {id}: {e}")))?;
        event.last_accessed = Utc::now();
        let value = postcard::to_allocvec(&event)
            .map_err(|e| MnemeError::Storage(format!("re-encode EpisodicEvent: {e}")))?;
        self.storage.put(&key, &value).await
    }

    /// Lookup a single event by id.
    pub async fn get(&self, id: EventId) -> Result<Option<EpisodicEvent>> {
        let key = epi_key(&id);
        match self.storage.get(&key).await? {
            None => Ok(None),
            Some(bytes) => Ok(Some(postcard::from_bytes(&bytes).map_err(|e| {
                MnemeError::Storage(format!("decode EpisodicEvent {id}: {e}"))
            })?)),
        }
    }

    /// Lexical relevance of `query` to each of `ids`: IDF-weighted query
    /// coverage in `[0, 1]`. Ids that are not indexed (kind outside
    /// `[episodic] index_kinds`) or share no token with the query are
    /// absent from the map; callers treat absent as `0.0`.
    ///
    /// Coverage is measured against the whole index (a term is rare or
    /// common across all indexed events, not across `ids`), so the
    /// values agree with [`search`](Self::search).
    pub async fn lex_scores(&self, query: &str, ids: &[EventId]) -> Result<HashMap<EventId, f32>> {
        if ids.is_empty() || query.trim().is_empty() {
            return Ok(HashMap::new());
        }
        self.ensure_index().await?;
        Ok(self.index.read(|idx| match idx {
            Some(idx) => idx.coverage_of(query, ids),
            None => HashMap::new(),
        }))
    }

    /// Best lexical matches for `query` over hot + warm indexed events,
    /// with `filters` applied, best (BM25) first. The `f32` is the same
    /// coverage score [`lex_scores`](Self::lex_scores) reports.
    ///
    /// `filters.warm` is honoured as a tier filter: [`WarmRead::Never`]
    /// keeps hot rows only; `Auto` and `Always` search both tiers (a
    /// content search has no "recent" default to protect).
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        filters: &RecentFilters,
    ) -> Result<Vec<(EpisodicEvent, f32)>> {
        Ok(self
            .search_tiered(query, limit, filters)
            .await?
            .into_iter()
            .map(|h| (h.event, h.coverage))
            .collect())
    }

    /// [`search`](Self::search) with the tier and BM25 score of each
    /// hit.
    ///
    /// Every index hit is read back from storage before it is returned.
    /// A hit whose row is gone is dropped from the result and from the
    /// index, so drift heals on the next search that touches it.
    pub async fn search_tiered(
        &self,
        query: &str,
        limit: usize,
        filters: &RecentFilters,
    ) -> Result<Vec<LexicalHit>> {
        if limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_index().await?;
        let candidates = self.index.read(|idx| match idx {
            Some(idx) => idx.search(query, usize::MAX),
            None => Vec::new(),
        });
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for hit in candidates {
            if out.len() >= limit {
                break;
            }
            let Some(row) = self.find_tiered(hit.id).await? else {
                gone.push(hit.id);
                continue;
            };
            if filters.warm == WarmRead::Never && row.tier == Tier::Warm {
                continue;
            }
            if !filters.matches(&row.event) {
                continue;
            }
            out.push(LexicalHit {
                event: row.event,
                tier: row.tier,
                coverage: hit.coverage,
                bm25: hit.score,
            });
        }
        if !gone.is_empty() {
            self.index.mutate(|idx| {
                for id in gone {
                    idx.remove(id);
                }
            });
        }
        Ok(out)
    }

    /// Ranked recent events.
    ///
    /// Sorts by `(last_accessed DESC, retrieval_weight DESC,
    /// created_at DESC)` and returns the top `limit`.
    ///
    /// `limit == 0` returns an empty `Vec` (not an error).
    ///
    /// Reads the warm tier as well when `filters.warm` says so (see
    /// [`WarmRead`]). Use [`recall_recent_tiered`](Self::recall_recent_tiered)
    /// to learn which tier each row came from.
    pub async fn recall_recent(
        &self,
        filters: &RecentFilters,
        limit: usize,
    ) -> Result<Vec<EpisodicEvent>> {
        Ok(self
            .recall_recent_tiered(filters, limit)
            .await?
            .into_iter()
            .map(|t| t.event)
            .collect())
    }

    /// [`recall_recent`](Self::recall_recent) with each row tagged
    /// by tier. Same filters, ordering and `limit` semantics: hot and
    /// warm rows are merged before the sort, so the cap applies to
    /// the combined result.
    ///
    /// A row present under both prefixes (a crash between the two
    /// writes of `promote_to_warm`) is returned once, as hot.
    pub async fn recall_recent_tiered(
        &self,
        filters: &RecentFilters,
        limit: usize,
    ) -> Result<Vec<TieredEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let include_warm = self.reads_warm(filters, Utc::now());
        let mut events = self
            .scan_tiers(include_warm, |e| filters.matches(e))
            .await?;
        events.sort_by(|a, b| recency_order(&a.event, &b.event));
        events.truncate(limit);
        Ok(events)
    }

    /// Scan the hot prefix, and the warm prefix when `include_warm`,
    /// keeping rows that pass `keep`. Hot rows come first in the
    /// returned `Vec`; a warm row whose id is also in the hot tier is
    /// dropped (the hot copy is the one `touch` keeps current).
    async fn scan_tiers(
        &self,
        include_warm: bool,
        keep: impl Fn(&EpisodicEvent) -> bool,
    ) -> Result<Vec<TieredEvent>> {
        let raw = self.storage.scan_prefix(EPI_KEY_PREFIX).await?;
        let mut out: Vec<TieredEvent> = Vec::with_capacity(raw.len());
        let mut hot_ids: std::collections::HashSet<EventId> = std::collections::HashSet::new();
        for (_k, v) in raw {
            let event: EpisodicEvent = postcard::from_bytes(&v)
                .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent: {e}")))?;
            if include_warm {
                hot_ids.insert(event.id);
            }
            if keep(&event) {
                out.push(TieredEvent {
                    event,
                    tier: Tier::Hot,
                });
            }
        }
        if include_warm {
            for (_k, v) in self.storage.scan_prefix(WARM_KEY_PREFIX).await? {
                let event: EpisodicEvent = postcard::from_bytes(&v)
                    .map_err(|e| MnemeError::Storage(format!("decode warm EpisodicEvent: {e}")))?;
                if !hot_ids.contains(&event.id) && keep(&event) {
                    out.push(TieredEvent {
                        event,
                        tier: Tier::Warm,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Every event in the hot and warm tiers, newest first by
    /// `created_at` (ties broken by id, newest first, so the order
    /// does not depend on which tier a row sits in). Used by `export`:
    /// a dump that stopped at the hot tier dropped every event older
    /// than the hot-to-warm threshold.
    ///
    /// Pass `include_warm = false` for the hot tier alone. The cold
    /// archive is never read here.
    pub async fn list_hot_and_warm(&self, include_warm: bool) -> Result<Vec<TieredEvent>> {
        let mut out = self.scan_tiers(include_warm, |_| true).await?;
        out.sort_by(|a, b| newest_first(&a.event, &b.event));
        Ok(out)
    }

    /// Strict reverse-chronological iteration over the hot tier,
    /// no scoring. Used by the consolidation task to walk every
    /// event without the overhead of per-row score math.
    ///
    /// Sorted explicitly by `created_at DESC`. ULID lexical order is
    /// almost always equivalent, but two events created in the same
    /// millisecond carry different random tails and could appear
    /// out-of-order if we trusted the key sequence alone.
    pub async fn list_all(&self) -> Result<Vec<EpisodicEvent>> {
        let raw = self.storage.scan_prefix(EPI_KEY_PREFIX).await?;
        let mut out: Vec<EpisodicEvent> = Vec::with_capacity(raw.len());
        for (_k, v) in raw {
            let event: EpisodicEvent = postcard::from_bytes(&v)
                .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent: {e}")))?;
            out.push(event);
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        Ok(out)
    }

    /// Hard-delete an event from the hot tier. Used by the
    /// consolidation task once it has copied an event into a colder
    /// tier.
    pub async fn delete(&self, id: EventId) -> Result<()> {
        self.storage.delete(&epi_key(&id)).await?;
        self.unindex_unless_present(id, &warm_key(&id)).await
    }

    /// Drop `id` from the keyword index unless the row still exists
    /// under `other_key` (the same event in the other tier).
    async fn unindex_unless_present(&self, id: EventId, other_key: &[u8]) -> Result<()> {
        if self.storage.get(other_key).await?.is_none() {
            self.index.mutate(|idx| idx.remove(id));
        }
        Ok(())
    }

    /// Agent-driven point delete: remove an event from every live
    /// tier that holds it (hot and warm). Returns `true` if anything was
    /// deleted, `false` if the id wasn't present in either tier.
    ///
    /// Cold-archive entries are intentionally not reachable from here
    /// — the cold quarter files are append-only by design and the
    /// 180-day blast radius is a deliberate privacy floor (see the
    /// privacy discipline note on `record_event`). If an event has
    /// migrated past the warm tier the agent should treat it as
    /// out-of-scope until v1.1's daemon channel.
    pub async fn forget(&self, id: EventId) -> Result<bool> {
        // Check both tiers rather than stopping at the first hit: a
        // crash inside `promote_to_warm` can leave the row under both
        // prefixes, and warm rows are readable, so a leftover warm
        // copy would bring a forgotten event back.
        let mut found = false;
        for key in [epi_key(&id), warm_key(&id)] {
            if self.storage.get(&key).await?.is_some() {
                self.storage.delete(&key).await?;
                found = true;
            }
        }
        self.index.mutate(|idx| idx.remove(id));
        Ok(found)
    }

    /// Rewrite, in place, the payload of every hot- and warm-tier event
    /// of `kind` for which `rewrite` returns a replacement. Returns how
    /// many rows were rewritten.
    ///
    /// Only `payload` changes: the row keeps its id, tier, scope, tags,
    /// weight and both timestamps, so it sits where it sat on the
    /// timeline and consolidation treats it exactly as before. Each
    /// replacement goes through the same [`Storage::put`] as a new event
    /// (WAL first, then redb; sealed per record when the store is
    /// encrypted), so it is as durable as the row it replaces.
    ///
    /// This is the redaction primitive behind `forget`: the `tool_call`
    /// rows that mirrored a forgotten memory's text are overwritten
    /// rather than deleted, so the timeline still shows that a write
    /// happened. Cold-archive bundles are immutable and are not touched.
    /// It deliberately does not bump the activity counter: a redaction
    /// is not new user activity for the consolidation scheduler.
    pub async fn rewrite_payloads<F>(&self, kind: &str, mut rewrite: F) -> Result<usize>
    where
        F: FnMut(&EpisodicEvent) -> Option<String> + Send,
    {
        let mut rewritten = 0usize;
        for prefix in [EPI_KEY_PREFIX, WARM_KEY_PREFIX] {
            for (key, value) in self.storage.scan_prefix(prefix).await? {
                let mut event: EpisodicEvent = postcard::from_bytes(&value)
                    .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent: {e}")))?;
                if event.kind != kind {
                    continue;
                }
                let Some(payload) = rewrite(&event) else {
                    continue;
                };
                event.payload = payload;
                let value = postcard::to_allocvec(&event)
                    .map_err(|e| MnemeError::Storage(format!("re-encode EpisodicEvent: {e}")))?;
                self.storage.put(&key, &value).await?;
                if self.is_indexed_kind(&event.kind) {
                    // The old text must stop matching.
                    self.index
                        .mutate(|idx| idx.insert(event.id, &indexable_text(&event)));
                }
                rewritten += 1;
            }
        }
        Ok(rewritten)
    }

    /// Lookup an event in either the hot or warm tier. Returns `None`
    /// if it's not in redb (callers can fall back to the cold archive
    /// via `storage::archive::ColdArchive::find_anywhere`).
    pub async fn find_warm_or_hot(&self, id: EventId) -> Result<Option<EpisodicEvent>> {
        if let Some(e) = self.get(id).await? {
            return Ok(Some(e));
        }
        let warm_key = warm_key(&id);
        match self.storage.get(&warm_key).await? {
            None => Ok(None),
            Some(bytes) => Ok(Some(postcard::from_bytes(&bytes).map_err(|e| {
                MnemeError::Storage(format!("decode warm EpisodicEvent {id}: {e}"))
            })?)),
        }
    }

    /// Hot-tier event count — cheap scan of the `epi:` prefix range.
    /// Surfaced via the `mneme://stats` resource. The redb prefix
    /// scan is O(n) over the rows in the prefix, but for hot-tier
    /// cardinalities (≤thousands) that's microseconds.
    pub async fn count_hot(&self) -> Result<usize> {
        Ok(self.storage.scan_prefix(EPI_KEY_PREFIX).await?.len())
    }

    /// Age profile of the hot tier in one scan: row count, oldest
    /// `last_accessed`, and how many rows are at or past `cutoff`
    /// (`last_accessed <= cutoff`, the comparison consolidation uses).
    /// Replaces [`count_hot`](Self::count_hot) wherever the caller
    /// also wants the ages, so stats costs one scan, not two.
    pub async fn hot_age_summary(&self, cutoff: DateTime<Utc>) -> Result<HotAgeSummary> {
        let raw = self.storage.scan_prefix(EPI_KEY_PREFIX).await?;
        let mut oldest: Option<DateTime<Utc>> = None;
        let mut eligible = 0usize;
        for (_k, v) in &raw {
            let event: EpisodicEvent = postcard::from_bytes(v)
                .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent: {e}")))?;
            if event.last_accessed <= cutoff {
                eligible += 1;
            }
            oldest = Some(match oldest {
                Some(o) if o <= event.last_accessed => o,
                _ => event.last_accessed,
            });
        }
        Ok(HotAgeSummary {
            count: raw.len(),
            oldest_last_accessed: oldest,
            eligible,
        })
    }

    /// Warm-tier event count.
    pub async fn count_warm(&self) -> Result<usize> {
        Ok(self.storage.scan_prefix(WARM_KEY_PREFIX).await?.len())
    }

    /// All events currently in the warm tier, sorted by `created_at`
    /// ascending. Used by the consolidation task to evaluate
    /// warm→cold migrations.
    pub async fn list_warm(&self) -> Result<Vec<EpisodicEvent>> {
        let raw = self.storage.scan_prefix(WARM_KEY_PREFIX).await?;
        let mut out: Vec<EpisodicEvent> = Vec::with_capacity(raw.len());
        for (_k, v) in raw {
            let event: EpisodicEvent = postcard::from_bytes(&v)
                .map_err(|e| MnemeError::Storage(format!("decode warm EpisodicEvent: {e}")))?;
            out.push(event);
        }
        out.sort_by_key(|e| e.created_at);
        Ok(out)
    }

    /// Move an event from hot → warm. Idempotent on re-run. The keyword
    /// index is keyed by id and is left alone: a warm row stays
    /// searchable.
    pub async fn promote_to_warm(&self, id: EventId) -> Result<bool> {
        let hot_key = epi_key(&id);
        let Some(bytes) = self.storage.get(&hot_key).await? else {
            return Ok(false);
        };
        // Write under the new prefix BEFORE deleting the old — if we
        // crash between, replay leaves the row in both tiers, which
        // re-running consolidation cleans up.
        self.storage.put(&warm_key(&id), &bytes).await?;
        self.storage.delete(&hot_key).await?;
        Ok(true)
    }

    /// Drop a warm-tier row by id. Used by consolidation after the
    /// row has been folded into the cold archive.
    pub async fn delete_warm(&self, id: EventId) -> Result<()> {
        self.storage.delete(&warm_key(&id)).await?;
        self.unindex_unless_present(id, &epi_key(&id)).await
    }
}

fn epi_key(id: &EventId) -> Vec<u8> {
    let mut k = Vec::with_capacity(EPI_KEY_PREFIX.len() + 16);
    k.extend_from_slice(EPI_KEY_PREFIX);
    k.extend_from_slice(&id.0.to_bytes());
    k
}

fn warm_key(id: &EventId) -> Vec<u8> {
    let mut k = Vec::with_capacity(WARM_KEY_PREFIX.len() + 16);
    k.extend_from_slice(WARM_KEY_PREFIX);
    k.extend_from_slice(&id.0.to_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_impl::MemoryStorage;
    use serde_json::json;

    fn store() -> EpisodicStore {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        EpisodicStore::new(storage)
    }

    #[tokio::test]
    async fn record_then_get_round_trips() {
        let s = store();
        let id = s
            .record_json("tool_call", "personal", &json!({"tool": "remember"}))
            .await
            .unwrap();
        let got = s.get(id).await.unwrap().unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.kind, "tool_call");
        assert_eq!(got.scope, "personal");
        assert_eq!(got.payload_json().unwrap(), json!({"tool": "remember"}));
        assert_eq!(got.retrieval_weight, DEFAULT_RETRIEVAL_WEIGHT);
        // Sanity: created_at == last_accessed at record time.
        assert_eq!(got.created_at, got.last_accessed);
    }

    #[tokio::test]
    async fn record_weighted_rejects_out_of_range() {
        let s = store();
        let err = s.record_weighted("k", "p", "null", -0.1).await.unwrap_err();
        assert!(matches!(err, MnemeError::Storage(_)));
        let err = s.record_weighted("k", "p", "null", 1.5).await.unwrap_err();
        assert!(matches!(err, MnemeError::Storage(_)));
    }

    #[tokio::test]
    async fn recall_recent_orders_by_recency_then_weight() {
        let s = store();
        // Record three events; the second one we'll later touch so it
        // becomes "most recently accessed."
        let _a = s.record("k", "p", "\"a\"").await.unwrap();
        let b = s.record("k", "p", "\"b\"").await.unwrap();
        let _c = s.record("k", "p", "\"c\"").await.unwrap();
        s.touch(b).await.unwrap();

        let hits = s.recall_recent(&RecentFilters::default(), 5).await.unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].id, b, "touched event must rank first");
    }

    #[tokio::test]
    async fn recall_recent_filters_by_scope_and_kind() {
        let s = store();
        let target = s.record("decision", "work", "\"project x\"").await.unwrap();
        s.record("decision", "personal", "\"dinner\"")
            .await
            .unwrap();
        s.record("tool_call", "work", "\"git status\"")
            .await
            .unwrap();

        let hits = s
            .recall_recent(
                &RecentFilters {
                    scope: Some("work".into()),
                    kind: Some("decision".into()),
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, target);
    }

    #[tokio::test]
    async fn recall_recent_kinds_and_exclude_kinds() {
        let s = store();
        let d = s.record("decision", "p", "\"d\"").await.unwrap();
        let t = s.record("tool_call", "p", "\"t\"").await.unwrap();
        let n = s.record("note", "p", "\"n\"").await.unwrap();

        let sorted = |mut v: Vec<EventId>| {
            v.sort_by_key(|i| i.0);
            v
        };
        let ids = |hits: Vec<EpisodicEvent>| sorted(hits.into_iter().map(|e| e.id).collect());

        let only = s
            .recall_recent(
                &RecentFilters {
                    kinds: vec!["decision".into(), "note".into()],
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(ids(only), sorted(vec![d, n]));

        let without = s
            .recall_recent(
                &RecentFilters {
                    exclude_kinds: vec!["tool_call".into()],
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(ids(without), sorted(vec![d, n]));

        let both = s
            .recall_recent(
                &RecentFilters {
                    kinds: vec!["decision".into(), "tool_call".into()],
                    exclude_kinds: vec!["decision".into()],
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(ids(both), vec![t]);

        // Static exclusions stack with the owned list.
        let static_only = s
            .recall_recent(
                &RecentFilters {
                    exclude_kinds_static: &["tool_call"],
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(ids(static_only), sorted(vec![d, n]));
        let stacked = s
            .recall_recent(
                &RecentFilters {
                    exclude_kinds: vec!["decision".into()],
                    exclude_kinds_static: &["tool_call"],
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(ids(stacked), vec![n]);
    }

    #[tokio::test]
    async fn hot_age_summary_empty_tier() {
        let s = store();
        let sum = s.hot_age_summary(Utc::now()).await.unwrap();
        assert_eq!(sum.count, 0);
        assert_eq!(sum.oldest_last_accessed, None);
        assert_eq!(sum.eligible, 0);
    }

    #[tokio::test]
    async fn hot_age_summary_reports_oldest_and_eligible() {
        use crate::memory::consolidation::_record_backdated_for_tests;
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let s = EpisodicStore::new(Arc::clone(&backing));
        let now = Utc::now();
        let old = now - chrono::Duration::days(30);
        let mid = now - chrono::Duration::days(10);
        _record_backdated_for_tests(&backing, "k", "p", "\"a\"", old)
            .await
            .unwrap();
        _record_backdated_for_tests(&backing, "k", "p", "\"b\"", mid)
            .await
            .unwrap();
        s.record("k", "p", "\"fresh\"").await.unwrap();

        let cutoff = now - chrono::Duration::days(28);
        let sum = s.hot_age_summary(cutoff).await.unwrap();
        assert_eq!(sum.count, 3);
        assert_eq!(sum.oldest_last_accessed, Some(old));
        assert_eq!(sum.eligible, 1);
        assert_eq!(sum.count, s.count_hot().await.unwrap());
    }

    #[tokio::test]
    async fn recall_recent_since_bound_inclusive_lower() {
        let s = store();
        s.record("a", "p", "\"a\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let mid = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let later = s.record("b", "p", "\"b\"").await.unwrap();

        let hits = s
            .recall_recent(
                &RecentFilters {
                    since: Some(mid),
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, later);
    }

    #[tokio::test]
    async fn recall_recent_until_bound_exclusive_upper() {
        let s = store();
        let early = s.record("a", "p", "\"a\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let mid = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        s.record("b", "p", "\"b\"").await.unwrap();

        let hits = s
            .recall_recent(
                &RecentFilters {
                    until: Some(mid),
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, early);
    }

    #[tokio::test]
    async fn recall_recent_since_and_until_window() {
        let s = store();
        s.record("a", "p", "\"a\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let lo = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let mid = s.record("b", "p", "\"b\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let hi = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        s.record("c", "p", "\"c\"").await.unwrap();

        let hits = s
            .recall_recent(
                &RecentFilters {
                    since: Some(lo),
                    until: Some(hi),
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, mid);
    }

    #[tokio::test]
    async fn recall_recent_with_limit_truncates() {
        let s = store();
        for i in 0..5 {
            s.record("k", "p", &i.to_string()).await.unwrap();
        }
        let hits = s.recall_recent(&RecentFilters::default(), 2).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn recall_recent_zero_limit_is_empty() {
        let s = store();
        s.record("k", "p", "0").await.unwrap();
        let hits = s.recall_recent(&RecentFilters::default(), 0).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn retrieval_weight_downranks_low_signal_events() {
        // Two events recorded back-to-back. The high-weight one should
        // outrank the low-weight one even though they're equally
        // recent.
        let s = store();
        let low = s
            .record_weighted("noise", "p", "\"blah\"", 0.1)
            .await
            .unwrap();
        let high = s
            .record_weighted("decision", "p", "\"ship it\"", 0.9)
            .await
            .unwrap();
        let hits = s.recall_recent(&RecentFilters::default(), 5).await.unwrap();
        // ULID for `high` is created later, and it has higher weight
        // → must come first; `low` may still appear, just lower.
        assert_eq!(hits[0].id, high);
        assert!(hits.iter().any(|e| e.id == low));
    }

    #[tokio::test]
    async fn touch_unknown_id_errors() {
        let s = store();
        let stranger = EventId::new();
        let err = s.touch(stranger).await.unwrap_err();
        assert!(matches!(err, MnemeError::Storage(_)));
    }

    #[tokio::test]
    async fn list_all_returns_newest_first() {
        let s = store();
        let a = s.record("k", "p", "\"a\"").await.unwrap();
        let b = s.record("k", "p", "\"b\"").await.unwrap();
        let c = s.record("k", "p", "\"c\"").await.unwrap();
        let all = s.list_all().await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, c);
        assert_eq!(all[1].id, b);
        assert_eq!(all[2].id, a);
    }

    #[tokio::test]
    async fn delete_removes_from_get_and_recall() {
        let s = store();
        let id = s.record("k", "p", "\"doomed\"").await.unwrap();
        s.delete(id).await.unwrap();
        assert!(s.get(id).await.unwrap().is_none());
        let hits = s.recall_recent(&RecentFilters::default(), 5).await.unwrap();
        assert!(hits.iter().all(|e| e.id != id));
    }

    #[tokio::test]
    async fn forget_removes_from_hot_tier() {
        let s = store();
        let id = s.record("k", "p", "\"hot\"").await.unwrap();
        assert!(s.forget(id).await.unwrap(), "should report deletion");
        assert!(s.get(id).await.unwrap().is_none());
        assert!(!s.forget(id).await.unwrap(), "second forget is a no-op");
    }

    #[tokio::test]
    async fn forget_removes_from_warm_tier() {
        let s = store();
        let id = s.record("k", "p", "\"warm\"").await.unwrap();
        assert!(s.promote_to_warm(id).await.unwrap());
        assert!(s.get(id).await.unwrap().is_none(), "no longer hot");
        assert!(s.forget(id).await.unwrap(), "warm tier delete reported");
        assert_eq!(s.count_warm().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn forget_unknown_id_returns_false() {
        let s = store();
        let absent = EventId::new();
        assert!(!s.forget(absent).await.unwrap());
    }

    /// Exit-criterion preview from plan §3 Phase 4: backdated events
    /// rank below fresh ones.
    #[tokio::test]
    async fn aged_event_ranks_below_fresh_event() {
        let s = store();

        let aged = s.record("k", "p", "\"old\"").await.unwrap();
        // Manually backdate `aged` by overwriting it with an older
        // last_accessed (simulates a long-quiet event).
        let key = epi_key(&aged);
        let bytes = s.storage.get(&key).await.unwrap().unwrap();
        let mut e: EpisodicEvent = postcard::from_bytes(&bytes).unwrap();
        e.last_accessed = Utc::now() - chrono::Duration::days(7);
        let v = postcard::to_allocvec(&e).unwrap();
        s.storage.put(&key, &v).await.unwrap();

        let fresh = s.record("k", "p", "\"new\"").await.unwrap();
        let hits = s.recall_recent(&RecentFilters::default(), 5).await.unwrap();
        assert_eq!(hits[0].id, fresh, "fresh event must outrank aged one");
        assert!(hits.iter().any(|e| e.id == aged));
    }

    #[tokio::test]
    async fn rewrite_payloads_covers_hot_and_warm_and_keeps_everything_else() {
        let s = store();
        let hot = s.record("tool_call", "work", "hot secret").await.unwrap();
        let warm = s.record("tool_call", "work", "warm secret").await.unwrap();
        let other_kind = s.record("decision", "work", "hot secret").await.unwrap();
        let untouched = s.record("tool_call", "work", "keep me").await.unwrap();
        assert!(s.promote_to_warm(warm).await.unwrap());
        let before_hot = s.get(hot).await.unwrap().unwrap();
        let before_warm = s.find_warm_or_hot(warm).await.unwrap().unwrap();

        let n = s
            .rewrite_payloads("tool_call", |e| {
                e.payload.contains("secret").then(|| "gone".to_owned())
            })
            .await
            .unwrap();
        assert_eq!(n, 2);

        let after_hot = s.get(hot).await.unwrap().unwrap();
        assert_eq!(after_hot.payload, "gone");
        assert_eq!(
            EpisodicEvent {
                payload: before_hot.payload.clone(),
                ..after_hot
            },
            before_hot,
            "only the payload may change"
        );
        // The warm row is rewritten where it lives, not moved back to hot.
        assert!(s.get(warm).await.unwrap().is_none());
        let after_warm = s.find_warm_or_hot(warm).await.unwrap().unwrap();
        assert_eq!(after_warm.payload, "gone");
        assert_eq!(after_warm.created_at, before_warm.created_at);
        assert_eq!(s.count_hot().await.unwrap(), 3);
        assert_eq!(s.count_warm().await.unwrap(), 1);

        assert_eq!(
            s.get(other_kind).await.unwrap().unwrap().payload,
            "hot secret"
        );
        assert_eq!(s.get(untouched).await.unwrap().unwrap().payload, "keep me");
    }

    /// The rewrite goes through the WAL like any other write, so it is
    /// still there after the store is closed and reopened, and on an
    /// encrypted store nothing readable reaches the backend.
    #[test]
    fn rewrite_payloads_survives_reopen_plain_and_encrypted() {
        use crate::crypto::Dek;
        use crate::storage::encrypted::EncryptedStorage;
        use crate::storage::redb_impl::RedbStorage;

        let marker = "the launch code is swordfish-7741";
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dek = Dek::generate().unwrap();
        for encrypted in [false, true] {
            let tmp = tempfile::TempDir::new().unwrap();
            let open = || -> Arc<dyn Storage> {
                if encrypted {
                    EncryptedStorage::open_redb(tmp.path(), &dek).unwrap()
                } else {
                    RedbStorage::open(tmp.path()).unwrap()
                }
            };

            let (hot, warm) = rt.block_on(async {
                let s = EpisodicStore::new(open());
                let hot = s.record("tool_call", "work", marker).await.unwrap();
                let warm = s.record("tool_call", "work", marker).await.unwrap();
                assert!(s.promote_to_warm(warm).await.unwrap());
                let n = s
                    .rewrite_payloads("tool_call", |_| Some("redacted".to_owned()))
                    .await
                    .unwrap();
                assert_eq!(n, 2);
                (hot, warm)
            });

            rt.block_on(async {
                let backing = open();
                let s = EpisodicStore::new(Arc::clone(&backing));
                for id in [hot, warm] {
                    let e = s.find_warm_or_hot(id).await.unwrap().unwrap();
                    assert_eq!(e.payload, "redacted", "encrypted={encrypted}");
                }
                assert_eq!(s.count_warm().await.unwrap(), 1);
            });

            if encrypted {
                // Neither the original nor the replacement is readable
                // in what the encrypted stack hands the backend.
                rt.block_on(async {
                    let raw = RedbStorage::open_encrypted(
                        tmp.path(),
                        Arc::new(crate::crypto::Aead::new(&dek)),
                    )
                    .unwrap();
                    let rows = raw.scan_prefix(b"").await.unwrap();
                    assert!(!rows.is_empty());
                    for (_k, v) in rows {
                        let text = String::from_utf8_lossy(&v);
                        assert!(!text.contains("swordfish"));
                        assert!(!text.contains("redacted"));
                    }
                });
            }
        }
    }
}

/// Warm-tier reads (1.5.2). Kept apart from `tests` above so the two
/// can grow independently.
#[cfg(test)]
mod warm_read_tests {
    use super::*;
    use crate::storage::memory_impl::MemoryStorage;
    use chrono::Duration;

    fn store() -> EpisodicStore {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        EpisodicStore::new(storage)
    }

    /// Write a hot row whose `created_at` and `last_accessed` are
    /// both `at`.
    async fn seed_at(s: &EpisodicStore, payload: &str, at: DateTime<Utc>) -> EventId {
        let event = EpisodicEvent {
            id: EventId::new(),
            kind: "note".into(),
            scope: "p".into(),
            payload: payload.into(),
            tags: vec![],
            retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
            last_accessed: at,
            created_at: at,
        };
        let value = postcard::to_allocvec(&event).unwrap();
        s.storage.put(&epi_key(&event.id), &value).await.unwrap();
        event.id
    }

    /// Two hot rows (1 and 2 days old) and three warm rows (40, 50,
    /// 60 days old). Returns `(hot_ids, warm_ids)`, each newest first.
    async fn two_hot_three_warm(s: &EpisodicStore) -> (Vec<EventId>, Vec<EventId>) {
        let now = Utc::now();
        let mut hot = Vec::new();
        for d in [1, 2] {
            hot.push(seed_at(s, "\"hot\"", now - Duration::days(d)).await);
        }
        let mut warm = Vec::new();
        for d in [40, 50, 60] {
            let id = seed_at(s, "\"warm\"", now - Duration::days(d)).await;
            assert!(s.promote_to_warm(id).await.unwrap());
            warm.push(id);
        }
        (hot, warm)
    }

    fn ids(rows: &[TieredEvent]) -> Vec<EventId> {
        rows.iter().map(|t| t.event.id).collect()
    }

    #[tokio::test]
    async fn since_older_than_hot_window_returns_warm_rows() {
        let s = store();
        let (hot, warm) = two_hot_three_warm(&s).await;
        let filters = RecentFilters {
            since: Some(Utc::now() - Duration::days(90)),
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 100).await.unwrap();
        // Ordering is unchanged (last_accessed DESC), so hot rows
        // lead and warm rows follow, each newest first.
        let expected: Vec<EventId> = hot.iter().chain(warm.iter()).copied().collect();
        assert_eq!(ids(&rows), expected);
        assert!(rows[..2].iter().all(|t| t.tier == Tier::Hot));
        assert!(rows[2..].iter().all(|t| t.tier == Tier::Warm));

        // The untiered wrapper returns the same rows.
        let plain = s.recall_recent(&filters, 100).await.unwrap();
        assert_eq!(plain.iter().map(|e| e.id).collect::<Vec<_>>(), expected);
    }

    /// The q49 shape from the field report: a closed window that
    /// lies entirely in the warm tier.
    #[tokio::test]
    async fn closed_old_window_returns_only_the_warm_rows_inside_it() {
        let s = store();
        let (_hot, warm) = two_hot_three_warm(&s).await;
        let now = Utc::now();
        let filters = RecentFilters {
            since: Some(now - Duration::days(55)),
            until: Some(now - Duration::days(35)),
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 100).await.unwrap();
        assert_eq!(ids(&rows), vec![warm[0], warm[1]]);
    }

    #[tokio::test]
    async fn recent_window_and_unbounded_calls_stay_hot_only() {
        let s = store();
        let (hot, _warm) = two_hot_three_warm(&s).await;

        let recent = RecentFilters {
            since: Some(Utc::now() - Duration::days(7)),
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&recent, 100).await.unwrap();
        assert_eq!(ids(&rows), hot);

        let unbounded = s
            .recall_recent_tiered(&RecentFilters::default(), 100)
            .await
            .unwrap();
        assert_eq!(ids(&unbounded), hot);
    }

    #[tokio::test]
    async fn warm_never_overrides_an_old_since() {
        let s = store();
        let (hot, _warm) = two_hot_three_warm(&s).await;
        let filters = RecentFilters {
            since: Some(Utc::now() - Duration::days(90)),
            warm: WarmRead::Never,
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 100).await.unwrap();
        assert_eq!(ids(&rows), hot);
    }

    #[tokio::test]
    async fn warm_always_reads_warm_without_a_time_bound() {
        let s = store();
        let (hot, warm) = two_hot_three_warm(&s).await;
        let filters = RecentFilters {
            warm: WarmRead::Always,
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 100).await.unwrap();
        let expected: Vec<EventId> = hot.iter().chain(warm.iter()).copied().collect();
        assert_eq!(ids(&rows), expected);
    }

    #[tokio::test]
    async fn until_only_window_reads_warm() {
        let s = store();
        let (_hot, warm) = two_hot_three_warm(&s).await;
        let filters = RecentFilters {
            until: Some(Utc::now() - Duration::days(30)),
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 100).await.unwrap();
        assert_eq!(ids(&rows), warm);
    }

    #[tokio::test]
    async fn limit_caps_the_merged_result() {
        let s = store();
        let (hot, warm) = two_hot_three_warm(&s).await;
        let filters = RecentFilters {
            warm: WarmRead::Always,
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 3).await.unwrap();
        assert_eq!(ids(&rows), vec![hot[0], hot[1], warm[0]]);
    }

    #[tokio::test]
    async fn auto_follows_the_configured_threshold() {
        let s = store().with_hot_to_warm_days(100);
        let (hot, warm) = two_hot_three_warm(&s).await;
        // 90 days back is inside a 100-day hot window: no warm scan.
        let inside = RecentFilters {
            since: Some(Utc::now() - Duration::days(90)),
            ..Default::default()
        };
        assert_eq!(
            ids(&s.recall_recent_tiered(&inside, 100).await.unwrap()),
            hot
        );
        let outside = RecentFilters {
            since: Some(Utc::now() - Duration::days(101)),
            ..Default::default()
        };
        assert_eq!(
            s.recall_recent_tiered(&outside, 100).await.unwrap().len(),
            hot.len() + warm.len()
        );
    }

    /// `promote_to_warm` writes the warm key before deleting the hot
    /// one. A crash in between leaves the row under both prefixes;
    /// readers must return it once.
    #[tokio::test]
    async fn row_in_both_tiers_is_returned_once_as_hot() {
        let s = store();
        let id = seed_at(&s, "\"dup\"", Utc::now() - Duration::days(40)).await;
        let bytes = s.storage.get(&epi_key(&id)).await.unwrap().unwrap();
        s.storage.put(&warm_key(&id), &bytes).await.unwrap();

        let filters = RecentFilters {
            warm: WarmRead::Always,
            ..Default::default()
        };
        let rows = s.recall_recent_tiered(&filters, 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tier, Tier::Hot);

        let all = s.list_hot_and_warm(true).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].tier, Tier::Hot);
    }

    #[tokio::test]
    async fn forget_removes_a_row_left_in_both_tiers() {
        let s = store();
        let id = seed_at(&s, "\"dup\"", Utc::now() - Duration::days(40)).await;
        let bytes = s.storage.get(&epi_key(&id)).await.unwrap().unwrap();
        s.storage.put(&warm_key(&id), &bytes).await.unwrap();

        assert!(s.forget(id).await.unwrap());
        assert!(s.list_hot_and_warm(true).await.unwrap().is_empty());
        assert!(!s.forget(id).await.unwrap());
    }

    #[tokio::test]
    async fn list_hot_and_warm_is_newest_first_across_tiers() {
        let s = store();
        let (hot, warm) = two_hot_three_warm(&s).await;
        let all = s.list_hot_and_warm(true).await.unwrap();
        let expected: Vec<EventId> = hot.iter().chain(warm.iter()).copied().collect();
        assert_eq!(ids(&all), expected);
        assert_eq!(
            all.len(),
            s.count_hot().await.unwrap() + s.count_warm().await.unwrap()
        );

        // Same call twice gives the same order.
        assert_eq!(ids(&s.list_hot_and_warm(true).await.unwrap()), expected);

        let hot_only = s.list_hot_and_warm(false).await.unwrap();
        assert_eq!(ids(&hot_only), hot);
    }
}

/// Keyword index, `search` / `lex_scores`, and the debounced touch
/// (1.6).
#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::storage::memory_impl::MemoryStorage;
    use chrono::Duration;
    use serde_json::json;

    fn store() -> EpisodicStore {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        EpisodicStore::new(storage)
    }

    async fn record(s: &EpisodicStore, kind: &str, content: &str) -> EventId {
        s.record_json(kind, "p", &json!({ "content": content }))
            .await
            .unwrap()
    }

    async fn found(s: &EpisodicStore, query: &str) -> Vec<EventId> {
        s.search(query, 50, &RecentFilters::default())
            .await
            .unwrap()
            .into_iter()
            .map(|(e, _)| e.id)
            .collect()
    }

    /// Backdate a hot row's `last_accessed` in place.
    async fn age_hot_row(s: &EpisodicStore, id: EventId, days: i64) {
        let key = epi_key(&id);
        let bytes = s.storage.get(&key).await.unwrap().unwrap();
        let mut e: EpisodicEvent = postcard::from_bytes(&bytes).unwrap();
        e.last_accessed = Utc::now() - Duration::days(days);
        s.storage
            .put(&key, &postcard::to_allocvec(&e).unwrap())
            .await
            .unwrap();
    }

    #[test]
    fn indexable_text_prefers_content_then_text_then_strings() {
        let event = |payload: &str, tags: &[&str]| EpisodicEvent {
            id: EventId::new(),
            kind: "decision".into(),
            scope: "p".into(),
            payload: payload.into(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            retrieval_weight: 1.0,
            last_accessed: Utc::now(),
            created_at: Utc::now(),
        };
        assert_eq!(
            indexable_text(&event(r#"{"content":"use redb","text":"ignored"}"#, &[])),
            "use redb"
        );
        assert_eq!(
            indexable_text(&event(r#"{"text":"weekly summary","covers":["a"]}"#, &[])),
            "weekly summary"
        );
        // Neither field: every top-level string value.
        assert_eq!(
            indexable_text(&event(r#"{"choice":"redb","n":3}"#, &[])),
            "redb"
        );
        // A bare JSON string, and a payload that is not JSON at all.
        assert_eq!(indexable_text(&event(r#""plain note""#, &[])), "plain note");
        assert_eq!(indexable_text(&event("not json", &[])), "not json");
        // Tags are appended.
        assert_eq!(
            indexable_text(&event(r#"{"content":"use redb"}"#, &["storage", "adr"])),
            "use redb\nstorage\nadr"
        );
    }

    #[tokio::test]
    async fn only_index_kinds_are_indexed() {
        let s = store();
        let decision = record(&s, "decision", "switch the backtest to live_bt.py").await;
        // Same words in an auto-logged row: must not be searchable.
        record(&s, "tool_call", "remember live_bt.py backtest").await;
        record(&s, "user_message", "what about live_bt.py").await;

        assert_eq!(found(&s, "live_bt.py").await, vec![decision]);
        assert_eq!(s.indexed_len(), Some(1));
        assert!(s.is_indexed_kind("decision"));
        assert!(!s.is_indexed_kind("tool_call"));
    }

    #[tokio::test]
    async fn index_kinds_are_configurable() {
        let s = store().with_index_kinds(vec!["note".into()]);
        let note = record(&s, "note", "the gateway port is 4002").await;
        record(&s, "decision", "gateway port moved").await;
        assert_eq!(found(&s, "gateway port").await, vec![note]);
    }

    #[tokio::test]
    async fn index_is_built_lazily_from_hot_and_warm_rows() {
        let s = store();
        let hot = record(&s, "lesson", "never deploy on Friday").await;
        let warm = record(&s, "lesson", "deploy only after the smoke test").await;
        assert!(s.promote_to_warm(warm).await.unwrap());
        assert_eq!(
            s.indexed_len(),
            None,
            "nothing built before the first search"
        );

        // Ids minted in the same millisecond are not ordered; compare
        // as sets.
        let mut got = found(&s, "deploy").await;
        got.sort();
        let mut want = vec![hot, warm];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(s.indexed_len(), Some(2));

        // Rows recorded after the build are searchable at once.
        let later = record(&s, "milestone", "first deploy to production").await;
        assert!(found(&s, "deploy").await.contains(&later));
        assert_eq!(s.indexed_len(), Some(3));
    }

    #[tokio::test]
    async fn promote_to_warm_keeps_the_index_entry() {
        let s = store();
        let id = record(&s, "decision", "rotate the gateway token monthly").await;
        assert_eq!(found(&s, "gateway token").await, vec![id]);

        assert!(s.promote_to_warm(id).await.unwrap());
        let hits = s
            .search_tiered("gateway token", 5, &RecentFilters::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event.id, id);
        assert_eq!(hits[0].tier, Tier::Warm);
        assert_eq!(s.indexed_len(), Some(1));

        // `WarmRead::Never` is a tier filter for search.
        let hot_only = RecentFilters {
            warm: WarmRead::Never,
            ..Default::default()
        };
        assert!(
            s.search("gateway token", 5, &hot_only)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn forget_removes_the_index_entry() {
        let s = store();
        let hot = record(&s, "decision", "pin the compiler to 1.100").await;
        let warm = record(&s, "decision", "pin the compiler in CI too").await;
        s.promote_to_warm(warm).await.unwrap();
        assert_eq!(found(&s, "compiler").await.len(), 2);

        assert!(s.forget(hot).await.unwrap());
        assert_eq!(found(&s, "compiler").await, vec![warm]);
        assert!(s.forget(warm).await.unwrap());
        assert!(found(&s, "compiler").await.is_empty());
        assert_eq!(s.indexed_len(), Some(0));
    }

    #[tokio::test]
    async fn consolidation_deletes_unindex_but_not_while_the_other_tier_holds_the_row() {
        let s = store();
        let id = record(&s, "problem", "the importer drops rows with empty ids").await;
        assert_eq!(found(&s, "importer").await, vec![id]);

        // Mid-promotion: the row is under both prefixes. Deleting one
        // copy must not make the event unsearchable.
        let bytes = s.storage.get(&epi_key(&id)).await.unwrap().unwrap();
        s.storage.put(&warm_key(&id), &bytes).await.unwrap();
        s.delete(id).await.unwrap();
        assert_eq!(found(&s, "importer").await, vec![id]);

        // The last copy going (warm → cold) removes it.
        s.delete_warm(id).await.unwrap();
        assert_eq!(s.indexed_len(), Some(0));
        assert!(found(&s, "importer").await.is_empty());
    }

    #[tokio::test]
    async fn clones_share_one_index() {
        let s = store();
        let clone = s.clone();
        let id = record(&s, "decision", "ship the indexer behind a flag").await;
        assert_eq!(found(&clone, "indexer").await, vec![id]);
        let second = record(&clone, "decision", "remove the indexer flag").await;
        let mut got = found(&s, "indexer").await;
        got.sort();
        let mut want = vec![id, second];
        want.sort();
        assert_eq!(got, want);
    }

    /// A row removed behind the index's back (another store over the
    /// same storage) is not returned, and the stale entry is dropped.
    #[tokio::test]
    async fn search_drops_hits_whose_row_is_gone() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let s = EpisodicStore::new(Arc::clone(&storage));
        let kept = record(&s, "lesson", "cache the embedder between runs").await;
        let gone = record(&s, "lesson", "cache the tokenizer too").await;
        assert_eq!(found(&s, "cache").await.len(), 2);

        // A private store, as a consolidation pass without the shared
        // handle would use: `s`'s index is not told.
        let other = EpisodicStore::new(Arc::clone(&storage));
        assert!(other.forget(gone).await.unwrap());
        assert_eq!(s.indexed_len(), Some(2), "index has drifted");

        assert_eq!(found(&s, "cache").await, vec![kept]);
        assert_eq!(s.indexed_len(), Some(1), "and healed itself");
    }

    #[tokio::test]
    async fn rewritten_payloads_are_reindexed() {
        let s = store();
        let id = record(&s, "observation", "the token is hunter2").await;
        assert_eq!(found(&s, "hunter2").await, vec![id]);
        let n = s
            .rewrite_payloads("observation", |_| {
                Some(json!({"content": "[redacted]"}).to_string())
            })
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(found(&s, "hunter2").await.is_empty());
        assert_eq!(found(&s, "redacted").await, vec![id]);
    }

    #[tokio::test]
    async fn tags_are_searchable_and_filters_apply() {
        let s = store();
        let tagged = s
            .record_full(
                "decision",
                "work",
                &json!({"content": "use the new allocator"}).to_string(),
                vec!["perf-2026".into()],
                1.0,
            )
            .await
            .unwrap();
        let other = s
            .record_full(
                "milestone",
                "home",
                &json!({"content": "allocator benchmark done"}).to_string(),
                vec![],
                1.0,
            )
            .await
            .unwrap();
        assert_eq!(found(&s, "perf-2026").await, vec![tagged]);

        let search = |filters: RecentFilters| {
            let s = &s;
            async move {
                s.search("allocator", 10, &filters)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|(e, _)| e.id)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            search(RecentFilters {
                scope: Some("home".into()),
                ..Default::default()
            })
            .await,
            vec![other]
        );
        assert_eq!(
            search(RecentFilters {
                scopes: vec!["work".into(), "nowhere".into()],
                ..Default::default()
            })
            .await,
            vec![tagged]
        );
        assert_eq!(
            search(RecentFilters {
                tags: vec!["perf-2026".into()],
                ..Default::default()
            })
            .await,
            vec![tagged]
        );
        assert_eq!(
            search(RecentFilters {
                kind: Some("milestone".into()),
                ..Default::default()
            })
            .await,
            vec![other]
        );
        assert!(
            search(RecentFilters {
                until: Some(Utc::now() - Duration::days(1)),
                ..Default::default()
            })
            .await
            .is_empty()
        );
        // The limit is applied after the filters.
        assert_eq!(
            s.search("allocator", 1, &RecentFilters::default())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            s.search("allocator", 0, &RecentFilters::default())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            s.search("  ", 5, &RecentFilters::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn lex_scores_are_coverage_in_unit_range() {
        let s = store();
        let full = record(&s, "decision", "rotate the gateway token every month").await;
        let partial = record(&s, "decision", "the gateway listens on 4002").await;
        let unrelated = record(&s, "lesson", "label the cables").await;
        let unindexed = record(&s, "tool_call", "rotate gateway token").await;
        let missing = EventId::new();
        let ids = [full, partial, unrelated, unindexed, missing];

        let scores = s.lex_scores("rotate gateway token", &ids).await.unwrap();
        assert_eq!(scores.len(), 2, "{scores:?}");
        assert_eq!(scores[&full], 1.0);
        assert!(scores[&partial] > 0.0 && scores[&partial] < 1.0);
        assert!(scores.values().all(|v| (0.0..=1.0).contains(v)));
        assert!(!scores.contains_key(&unrelated));
        assert!(!scores.contains_key(&unindexed));
        assert!(!scores.contains_key(&missing));

        // The same numbers `search` reports.
        for (event, coverage) in s
            .search("rotate gateway token", 10, &RecentFilters::default())
            .await
            .unwrap()
        {
            assert_eq!(scores[&event.id], coverage);
        }

        // A word only the phrasing shares scores nothing.
        let stop = s.lex_scores("what is the plan", &ids).await.unwrap();
        assert!(stop.is_empty(), "{stop:?}");
        assert!(s.lex_scores("", &ids).await.unwrap().is_empty());
        assert!(s.lex_scores("gateway", &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn touch_if_stale_is_debounced_and_hot_only() {
        let s = store();
        let day = Duration::hours(RECALL_TOUCH_DEBOUNCE_HOURS);

        // Fresh: last_accessed is seconds old, nothing is written.
        let fresh = record(&s, "decision", "fresh").await;
        let before = s.get(fresh).await.unwrap().unwrap().last_accessed;
        assert!(!s.touch_if_stale(fresh, day).await.unwrap());
        assert_eq!(s.get(fresh).await.unwrap().unwrap().last_accessed, before);

        // Three days stale: one write, then debounced.
        let stale = record(&s, "decision", "stale").await;
        age_hot_row(&s, stale, 3).await;
        assert!(s.touch_if_stale(stale, day).await.unwrap());
        let touched = s.get(stale).await.unwrap().unwrap();
        assert!(Utc::now() - touched.last_accessed < Duration::minutes(1));
        assert!(!s.touch_if_stale(stale, day).await.unwrap());
        assert_eq!(
            s.get(stale).await.unwrap().unwrap().last_accessed,
            touched.last_accessed
        );
        // created_at is not a touch's business.
        assert_eq!(
            touched.created_at,
            s.get(stale).await.unwrap().unwrap().created_at
        );

        // Warm rows are not written, however stale.
        let warm = record(&s, "decision", "warm").await;
        age_hot_row(&s, warm, 60).await;
        s.promote_to_warm(warm).await.unwrap();
        let warm_before = s
            .find_warm_or_hot(warm)
            .await
            .unwrap()
            .unwrap()
            .last_accessed;
        assert!(!s.touch_if_stale(warm, day).await.unwrap());
        assert_eq!(
            s.find_warm_or_hot(warm)
                .await
                .unwrap()
                .unwrap()
                .last_accessed,
            warm_before
        );
        assert_eq!(s.count_hot().await.unwrap(), 2, "not pulled back to hot");

        // Unknown id: not an error.
        assert!(!s.touch_if_stale(EventId::new(), day).await.unwrap());
    }

    #[test]
    fn touch_on_recall_follows_the_config_default_and_the_builder() {
        assert_eq!(
            store().touch_on_recall(),
            crate::config::EpisodicConfig::default().touch_on_recall
        );
        assert!(!store().with_touch_on_recall(false).touch_on_recall());
    }
}
