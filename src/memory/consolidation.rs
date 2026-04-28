//! Phase 4 §3 — consolidation between memory tiers.
//!
//! Migrates aged events through the hot → warm → cold pipeline:
//!
//! ```text
//!  hot  (redb, prefix `epi:`)
//!    │  age ≥ hot_to_warm_days
//!    ▼
//!  warm (redb, prefix `wepi:`)
//!    │  age ≥ warm_to_cold_days
//!    ▼
//!  cold (zstd quarterly bundles in <root>/cold/)
//! ```
//!
//! `run` is the single entry point. It's idempotent: re-running it
//! against an already-consolidated state is a no-op. Crash-safe at
//! every step:
//!
//! * The hot→warm move writes the new key first, then deletes the
//!   old; `kill -9` mid-move leaves both rows on disk, and the next
//!   `run` cleans up the duplicate by overwriting the warm row with
//!   the same content (postcard payload is byte-identical) and
//!   re-deleting the hot row.
//!
//! * The warm→cold move rewrites the destination quarterly bundle
//!   (atomic temp+rename) before deleting the warm rows. A crash
//!   between leaves rows in both warm and cold; the next `run` sees
//!   the cold copy as authoritative, drops the warm rows it already
//!   migrated, and continues.
//!
//! Spec §13 doesn't dictate a consolidation latency budget — the
//! task is meant to be background-paced. We serialise the work via
//! `EpisodicStore::promote_to_warm` (which itself is async + fast)
//! and `ColdArchive::append` (which compresses). On a 100-event
//! consolidation pass, the whole `run` completes in ~tens of ms.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::Result;
use crate::config::ConsolidationConfig;
use crate::memory::episodic::{EpisodicEvent, EpisodicStore};
use crate::storage::Storage;
use crate::storage::archive::ColdArchive;

/// Tunables for [`run`]. Constructed from
/// [`crate::config::ConsolidationConfig`] in production, hand-built
/// in tests.
#[derive(Debug, Clone, Copy)]
pub struct ConsolidationParams {
    pub hot_to_warm_days: u32,
    pub warm_to_cold_days: u32,
}

impl ConsolidationParams {
    pub fn from_config(c: &ConsolidationConfig) -> Self {
        Self {
            hot_to_warm_days: c.hot_to_warm_days,
            warm_to_cold_days: c.warm_to_cold_days,
        }
    }
}

/// Counts surfaced after a `run` for diagnostics + the
/// `mneme://stats` resource.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsolidationReport {
    pub promoted_to_warm: usize,
    pub archived_to_cold: usize,
}

/// Move aged events through the tier pipeline once.
///
/// Pass the same `Arc<dyn Storage>` the rest of the process uses so
/// hot/warm reads see the same redb. The `archive` is the cold-tier
/// destination; one `archive.append(...)` call collapses all
/// migrated events into the right quarterly bundles.
pub async fn run(
    storage: Arc<dyn Storage>,
    archive: &ColdArchive,
    params: ConsolidationParams,
) -> Result<ConsolidationReport> {
    let now = Utc::now();
    let store = EpisodicStore::new(storage);

    // ---- Step 1: hot → warm ----------------------------------------
    let hot_cutoff = now - Duration::days(params.hot_to_warm_days as i64);
    let hot = store.list_all().await?;
    let mut promoted = 0usize;
    for ev in hot {
        if ev.last_accessed <= hot_cutoff && store.promote_to_warm(ev.id).await? {
            promoted += 1;
        }
    }

    // ---- Step 2: warm → cold ---------------------------------------
    let warm_cutoff = now - Duration::days(params.warm_to_cold_days as i64);
    let warm = store.list_warm().await?;
    let to_archive: Vec<EpisodicEvent> = warm
        .into_iter()
        .filter(|e| e.last_accessed <= warm_cutoff)
        .collect();
    let archived = to_archive.len();
    if !to_archive.is_empty() {
        archive.append(&to_archive)?;
        // Cold write is durable (atomic rename + fsync), so it's
        // safe to drop the warm rows.
        for ev in &to_archive {
            store.delete_warm(ev.id).await?;
        }
    }

    Ok(ConsolidationReport {
        promoted_to_warm: promoted,
        archived_to_cold: archived,
    })
}

/// Backdate-then-record helper used by tests + future `mneme inspect`
/// fixtures. Writes a row directly under the hot prefix with a
/// caller-supplied timestamp so the consolidation pass has something
/// to migrate without waiting wall-clock days.
#[doc(hidden)]
pub async fn _record_backdated_for_tests(
    storage: &Arc<dyn Storage>,
    kind: &str,
    scope: &str,
    payload: &str,
    created_at: DateTime<Utc>,
) -> Result<crate::ids::EventId> {
    use crate::ids::EventId;
    use crate::memory::episodic::DEFAULT_RETRIEVAL_WEIGHT;
    let event = EpisodicEvent {
        id: EventId::new(),
        kind: kind.to_owned(),
        scope: scope.to_owned(),
        payload: payload.to_owned(),
        tags: vec![],
        retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
        last_accessed: created_at,
        created_at,
    };
    let mut key = Vec::with_capacity(crate::memory::episodic::EPI_KEY_PREFIX.len() + 16);
    key.extend_from_slice(crate::memory::episodic::EPI_KEY_PREFIX);
    key.extend_from_slice(&event.id.0.to_bytes());
    let value = postcard::to_allocvec(&event)
        .map_err(|e| crate::MnemeError::Storage(format!("encode EpisodicEvent: {e}")))?;
    storage.put(&key, &value).await?;
    Ok(event.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::EventId;
    use crate::storage::memory_impl::MemoryStorage;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn cfg(hot: u32, warm: u32) -> ConsolidationParams {
        ConsolidationParams {
            hot_to_warm_days: hot,
            warm_to_cold_days: warm,
        }
    }

    /// Phase 4 exit gate: 100 backdated memories migrate correctly
    /// across tiers; queries find them.
    #[tokio::test]
    async fn hundred_backdated_events_distribute_across_three_tiers() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let now = Utc::now();

        // 30 events: fresh (1 day old) — stay hot.
        // 30 events: 60 days old — move hot → warm.
        // 40 events: 365 days old — move warm → cold.
        let mut fresh_ids = Vec::new();
        let mut middle_ids = Vec::new();
        let mut aged_ids = Vec::new();
        for _ in 0..30 {
            let id = _record_backdated_for_tests(
                &storage,
                "k",
                "p",
                "\"fresh\"",
                now - Duration::days(1),
            )
            .await
            .unwrap();
            fresh_ids.push(id);
        }
        for _ in 0..30 {
            let id = _record_backdated_for_tests(
                &storage,
                "k",
                "p",
                "\"middle\"",
                now - Duration::days(60),
            )
            .await
            .unwrap();
            middle_ids.push(id);
        }
        for _ in 0..40 {
            let id = _record_backdated_for_tests(
                &storage,
                "k",
                "p",
                "\"aged\"",
                now - Duration::days(365),
            )
            .await
            .unwrap();
            aged_ids.push(id);
        }

        let report = run(Arc::clone(&storage), &archive, cfg(28, 180))
            .await
            .unwrap();
        // 30 middle + 40 aged = 70 promoted to warm; the aged 40
        // are then promoted to cold in the same run.
        assert_eq!(report.promoted_to_warm, 70);
        assert_eq!(report.archived_to_cold, 40);

        let store = EpisodicStore::new(Arc::clone(&storage));

        // Hot tier: only the 30 fresh events.
        let hot = store.list_all().await.unwrap();
        assert_eq!(hot.len(), 30);
        for id in &fresh_ids {
            assert!(hot.iter().any(|e| e.id == *id));
        }

        // Warm tier: the 30 middle events.
        let warm = store.list_warm().await.unwrap();
        assert_eq!(warm.len(), 30);
        for id in &middle_ids {
            assert!(warm.iter().any(|e| e.id == *id));
        }

        // Cold tier: the 40 aged events. find_anywhere has to walk
        // every bundle since we don't pass the timestamp.
        for id in &aged_ids {
            let got = archive.find_anywhere(*id).unwrap();
            assert!(got.is_some(), "aged event {id} missing from cold");
        }

        // The exit gate also requires "queries find them" — confirm
        // every id is reachable via the appropriate tier API.
        for id in &fresh_ids {
            assert!(store.get(*id).await.unwrap().is_some());
        }
        for id in &middle_ids {
            // get() looks at hot only; find_warm_or_hot covers warm.
            assert!(store.get(*id).await.unwrap().is_none());
            assert!(store.find_warm_or_hot(*id).await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn run_is_idempotent() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let now = Utc::now();

        for _ in 0..10 {
            _record_backdated_for_tests(&storage, "k", "p", "\"x\"", now - Duration::days(200))
                .await
                .unwrap();
        }

        let r1 = run(Arc::clone(&storage), &archive, cfg(28, 180))
            .await
            .unwrap();
        // First pass: hot→warm 10, then warm→cold 10.
        assert_eq!(r1.promoted_to_warm, 10);
        assert_eq!(r1.archived_to_cold, 10);

        let r2 = run(Arc::clone(&storage), &archive, cfg(28, 180))
            .await
            .unwrap();
        // Nothing left to move.
        assert_eq!(r2.promoted_to_warm, 0);
        assert_eq!(r2.archived_to_cold, 0);
    }

    #[tokio::test]
    async fn no_migration_when_everything_is_fresh() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let now = Utc::now();
        for _ in 0..5 {
            _record_backdated_for_tests(&storage, "k", "p", "\"x\"", now - Duration::hours(1))
                .await
                .unwrap();
        }
        let report = run(Arc::clone(&storage), &archive, cfg(28, 180))
            .await
            .unwrap();
        assert_eq!(report.promoted_to_warm, 0);
        assert_eq!(report.archived_to_cold, 0);
        assert_eq!(
            EpisodicStore::new(storage).list_all().await.unwrap().len(),
            5
        );
    }

    #[tokio::test]
    async fn cold_archive_groups_by_quarter_after_consolidation() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());

        // Two events backdated to different quarters of 2025.
        let q1 = Utc.with_ymd_and_hms(2025, 2, 14, 12, 0, 0).unwrap();
        let q3 = Utc.with_ymd_and_hms(2025, 8, 14, 12, 0, 0).unwrap();
        let _id_q1 = _record_backdated_for_tests(&storage, "k", "p", "\"q1\"", q1)
            .await
            .unwrap();
        let _id_q3 = _record_backdated_for_tests(&storage, "k", "p", "\"q3\"", q3)
            .await
            .unwrap();
        let _ = run(Arc::clone(&storage), &archive, cfg(28, 180))
            .await
            .unwrap();

        let qs = archive.list_quarters().unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs.iter().any(|q| q.year == 2025 && q.quarter == 1));
        assert!(qs.iter().any(|q| q.year == 2025 && q.quarter == 3));
    }

    #[tokio::test]
    async fn find_warm_or_hot_returns_none_for_unknown_id() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let store = EpisodicStore::new(storage);
        assert!(
            store
                .find_warm_or_hot(EventId::new())
                .await
                .unwrap()
                .is_none()
        );
    }
}
