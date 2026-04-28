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
//! `EpisodicStore` is the *hot* tier only. The consolidation task
//! (Phase 4 §3, lands in `memory::consolidation`) walks this prefix
//! periodically and migrates aged-out events to a warm SQLite-style
//! flat file or a cold zstd archive (see `storage::archive`). This
//! module exposes only the writer/reader against the hot tier; the
//! migration logic owns its own scan loop.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::EventId;
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

/// Default `retrieval_weight` when the caller doesn't specify one.
/// Equal-weight events sort purely by recency.
pub const DEFAULT_RETRIEVAL_WEIGHT: f32 = 1.0;

/// One row in the episodic hot tier.
///
/// `kind` is a free-form string (e.g. `"tool_call"`, `"user_message"`,
/// `"checkpoint"`) — keeping it un-enumerated avoids forcing a schema
/// migration every time an agent invents a new event class.
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
#[derive(Debug, Clone, Default)]
pub struct RecentFilters {
    pub scope: Option<String>,
    pub kind: Option<String>,
}

/// Hot-tier reader/writer against the redb-backed [`Storage`] handle.
///
/// Cheap to construct (just clones the `Arc<dyn Storage>`) — call
/// sites can hold one per request if that simplifies threading.
#[derive(Clone)]
pub struct EpisodicStore {
    storage: Arc<dyn Storage>,
}

impl EpisodicStore {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    /// Append a new episodic event with the default retrieval weight.
    /// `payload` is stored verbatim (no encoding/escaping); typically
    /// a JSON string the caller produced via `serde_json::to_string`.
    pub async fn record(&self, kind: &str, scope: &str, payload: &str) -> Result<EventId> {
        self.record_weighted(kind, scope, payload, DEFAULT_RETRIEVAL_WEIGHT)
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

    /// Append a new event with an explicit retrieval weight. Use this
    /// for events that should rank above or below their peers — for
    /// instance, agent reflections vs. raw tool outputs.
    pub async fn record_weighted(
        &self,
        kind: &str,
        scope: &str,
        payload: &str,
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
            retrieval_weight,
            last_accessed: now,
            created_at: now,
        };
        let key = epi_key(&event.id);
        let value = postcard::to_allocvec(&event)
            .map_err(|e| MnemeError::Storage(format!("encode EpisodicEvent: {e}")))?;
        self.storage.put(&key, &value).await?;
        Ok(event.id)
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

    /// Ranked recent events.
    ///
    /// Sorts by `(last_accessed DESC, retrieval_weight DESC,
    /// created_at DESC)` and returns the top `limit`.
    ///
    /// `limit == 0` returns an empty `Vec` (not an error).
    pub async fn recall_recent(
        &self,
        filters: &RecentFilters,
        limit: usize,
    ) -> Result<Vec<EpisodicEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let raw = self.storage.scan_prefix(EPI_KEY_PREFIX).await?;
        let mut events: Vec<EpisodicEvent> = Vec::with_capacity(raw.len());
        for (_k, v) in raw {
            let event: EpisodicEvent = postcard::from_bytes(&v)
                .map_err(|e| MnemeError::Storage(format!("decode EpisodicEvent: {e}")))?;
            if let Some(ref s) = filters.scope
                && &event.scope != s
            {
                continue;
            }
            if let Some(ref k) = filters.kind
                && &event.kind != k
            {
                continue;
            }
            events.push(event);
        }
        events.sort_by(|a, b| {
            // Primary: most-recently-touched wins.
            b.last_accessed
                .cmp(&a.last_accessed)
                // Secondary: higher retrieval_weight wins among
                // equally-recent peers. f32 comparison can produce
                // NaN; treat NaN as ::Equal so we don't panic on
                // pathological inputs (record_weighted already
                // rejects out-of-range values, so this is defensive).
                .then_with(|| {
                    b.retrieval_weight
                        .partial_cmp(&a.retrieval_weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                // Final deterministic tiebreaker.
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        Ok(events.into_iter().take(limit).collect())
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
        self.storage.delete(&epi_key(&id)).await
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

    /// Move an event from hot → warm. Idempotent on re-run.
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
        self.storage.delete(&warm_key(&id)).await
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
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, target);
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
}
