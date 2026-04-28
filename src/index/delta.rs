//! HNSW delta log integration with the Phase 2 WAL machinery.
//!
//! The semantic-index WAL lives at `~/.mneme/semantic/wal/` (parallel
//! to the redb WAL at `~/.mneme/episodic/wal/`) and carries
//! [`WalOp::VectorInsert`] / [`WalOp::VectorDelete`] records — nothing
//! else. Every successful `append().await` has been fsynced to disk
//! and applied to the in-memory [`HnswIndex`] before the caller's
//! future resolves, matching spec §8.1's "disk first, RAM second"
//! contract.
//!
//! This module supplies the [`HnswApplier`] that the WAL writer
//! invokes after each fsync batch. The orchestrator (Phase 3 §7,
//! `memory::semantic`) wires it up: it owns both the HNSW (behind
//! `Arc<RwLock<HnswIndex>>`) and the [`WalWriter`] configured with
//! this applier.
//!
//! # Recovery on startup
//!
//! `replay_into` walks the WAL records past `applied_lsn` (the LSN
//! recorded in the most recent `hnsw.idx` snapshot) and re-applies
//! them to the loaded index. After that, the orchestrator opens a
//! fresh `WalWriter` at `max_observed_lsn + 1`, identical to the redb
//! recovery path.

use crate::index::hnsw::HnswIndex;
use crate::storage::wal::{Applier, ReplayRecord, WalOp};
use crate::{MnemeError, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// `Applier` impl used when constructing a `WalWriter` for the
/// semantic-index WAL. Holds the index behind a write-mostly RwLock
/// shared with the orchestrator's read-only search path.
///
/// `applied_lsn` advances monotonically as batches are folded into
/// the in-memory HNSW. Snapshot scheduling reads this so the
/// `hnsw.idx` file can record exactly which WAL records the
/// snapshot covers.
pub struct HnswApplier {
    index: Arc<RwLock<HnswIndex>>,
    applied_lsn: Arc<AtomicU64>,
}

impl HnswApplier {
    /// Construct an applier sharing `applied_lsn` with the orchestrator.
    /// Both ends use `fetch_max` / `load(SeqCst)` so concurrent reads
    /// (e.g. by the snapshot scheduler peeking at progress) always see
    /// a non-decreasing value.
    pub fn new(index: Arc<RwLock<HnswIndex>>, applied_lsn: Arc<AtomicU64>) -> Self {
        Self { index, applied_lsn }
    }
}

impl Applier for HnswApplier {
    fn apply_batch(&mut self, records: &[ReplayRecord]) -> Result<()> {
        let mut idx = self
            .index
            .write()
            .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
        let mut max_in_batch = 0u64;
        for rec in records {
            apply_one(&mut idx, &rec.op)?;
            if rec.lsn > max_in_batch {
                max_in_batch = rec.lsn;
            }
        }
        // Advance applied_lsn ONLY after every record applied
        // successfully — a partial apply (Err mid-batch) leaves the
        // counter at its previous value so replay can retry from there.
        if max_in_batch > 0 {
            self.applied_lsn.fetch_max(max_in_batch, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Replay every WAL record into `index` (typically called once at
/// startup, after [`crate::index::snapshot::load`]).
///
/// `iter` is the [`crate::storage::wal::replay`] iterator. Records
/// with `lsn <= applied_lsn` are skipped — they were already
/// folded into the snapshot. Returns the highest LSN observed so the
/// orchestrator can open the `WalWriter` at the right next-LSN.
pub fn replay_into<I>(index: &mut HnswIndex, iter: I, applied_lsn: u64) -> Result<u64>
where
    I: IntoIterator<Item = Result<ReplayRecord>>,
{
    let mut max_lsn = applied_lsn;
    for r in iter {
        let rec = r?;
        if rec.lsn > max_lsn {
            max_lsn = rec.lsn;
        }
        if rec.lsn <= applied_lsn {
            continue;
        }
        apply_one(index, &rec.op)?;
    }
    Ok(max_lsn)
}

fn apply_one(index: &mut HnswIndex, op: &WalOp) -> Result<()> {
    match op {
        WalOp::VectorInsert { id, vec } => index.insert(*id, vec),
        WalOp::VectorDelete { id } => index.delete(*id),
        WalOp::Put { .. } | WalOp::Delete { .. } => {
            // Surface clearly: a redb-WAL record landed in the
            // semantic-index WAL, which means a misconfigured
            // WalWriter is routing kv ops to the wrong file. Fail
            // the apply so the caller sees the integrity error.
            Err(MnemeError::Index(
                "kv WalOp routed to HnswApplier; semantic WAL is mis-targeted".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MemoryId;
    use crate::storage::wal::ReplayRecord;

    fn vec_for(seed: f32) -> Vec<f32> {
        let raw = [
            (seed * 0.91).sin(),
            (seed * 0.91).cos(),
            (seed * 1.73).sin(),
            (seed * 1.73).cos(),
        ];
        let n: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        raw.iter().map(|x| x / n).collect()
    }

    fn ins(id: MemoryId, seed: f32, lsn: u64) -> ReplayRecord {
        ReplayRecord {
            lsn,
            tx_id: lsn,
            op: WalOp::VectorInsert {
                id,
                vec: vec_for(seed),
            },
        }
    }
    fn del(id: MemoryId, lsn: u64) -> ReplayRecord {
        ReplayRecord {
            lsn,
            tx_id: lsn,
            op: WalOp::VectorDelete { id },
        }
    }

    fn fresh_applier() -> (Arc<RwLock<HnswIndex>>, Arc<AtomicU64>, HnswApplier) {
        let idx = Arc::new(RwLock::new(HnswIndex::new(4)));
        let lsn = Arc::new(AtomicU64::new(0));
        let applier = HnswApplier::new(Arc::clone(&idx), Arc::clone(&lsn));
        (idx, lsn, applier)
    }

    #[test]
    fn applier_inserts_and_deletes_round_trip() {
        let (idx, _lsn, mut applier) = fresh_applier();
        let id = MemoryId::new();
        applier
            .apply_batch(&[ins(id, 1.0, 1), ins(MemoryId::new(), 2.0, 2)])
            .unwrap();
        assert_eq!(idx.read().unwrap().len(), 2);
        applier.apply_batch(&[del(id, 3)]).unwrap();
        assert_eq!(idx.read().unwrap().len(), 1);
    }

    #[test]
    fn applier_advances_applied_lsn_to_batch_max() {
        let (_idx, lsn, mut applier) = fresh_applier();
        applier
            .apply_batch(&[ins(MemoryId::new(), 1.0, 5), ins(MemoryId::new(), 2.0, 9)])
            .unwrap();
        assert_eq!(lsn.load(Ordering::SeqCst), 9);
        // A subsequent batch with smaller LSNs (shouldn't happen in
        // practice, but guard anyway) must not regress the counter.
        applier
            .apply_batch(&[ins(MemoryId::new(), 3.0, 7)])
            .unwrap();
        assert_eq!(lsn.load(Ordering::SeqCst), 9);
        applier
            .apply_batch(&[ins(MemoryId::new(), 4.0, 11)])
            .unwrap();
        assert_eq!(lsn.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn applier_does_not_advance_on_error() {
        let (_idx, lsn, mut applier) = fresh_applier();
        applier
            .apply_batch(&[ins(MemoryId::new(), 1.0, 3)])
            .unwrap();
        assert_eq!(lsn.load(Ordering::SeqCst), 3);
        let bad = ReplayRecord {
            lsn: 4,
            tx_id: 4,
            op: WalOp::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
        };
        let _ = applier.apply_batch(&[bad]);
        assert_eq!(
            lsn.load(Ordering::SeqCst),
            3,
            "applied_lsn must not advance when apply_batch fails"
        );
    }

    #[test]
    fn applier_rejects_kv_ops() {
        let (_idx, _lsn, mut applier) = fresh_applier();
        let bad = ReplayRecord {
            lsn: 1,
            tx_id: 1,
            op: WalOp::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
        };
        let err = applier.apply_batch(&[bad]).unwrap_err();
        assert!(matches!(err, MnemeError::Index(_)));
    }

    #[test]
    fn replay_into_skips_records_at_or_below_applied_lsn() {
        let mut idx = HnswIndex::new(4);
        let id_old = MemoryId::new();
        let id_new = MemoryId::new();
        let recs: Vec<Result<ReplayRecord>> =
            vec![Ok(ins(id_old, 1.0, 1)), Ok(ins(id_new, 2.0, 2))];
        // Snapshot already contains lsn 1; replay should only re-apply lsn 2.
        let max = replay_into(&mut idx, recs, 1).unwrap();
        assert_eq!(max, 2);
        assert_eq!(idx.len(), 1, "only the post-snapshot record applies");
        // The applied one is id_new.
        let hits = idx.search(&vec_for(2.0), 1).unwrap();
        assert_eq!(hits[0].0, id_new);
    }

    #[test]
    fn replay_into_empty_iter_returns_applied_lsn() {
        let mut idx = HnswIndex::new(4);
        let recs: Vec<Result<ReplayRecord>> = Vec::new();
        let max = replay_into(&mut idx, recs, 7).unwrap();
        assert_eq!(max, 7);
    }

    #[test]
    fn replay_into_then_applier_continues_from_max_lsn() {
        // End-to-end: replay restores state, applier handles new writes.
        let idx = Arc::new(RwLock::new(HnswIndex::new(4)));
        let lsn = Arc::new(AtomicU64::new(0));
        let id_a = MemoryId::new();
        let id_b = MemoryId::new();
        {
            let mut g = idx.write().unwrap();
            let recs: Vec<Result<ReplayRecord>> = vec![Ok(ins(id_a, 1.0, 1))];
            let max = replay_into(&mut g, recs, 0).unwrap();
            assert_eq!(max, 1);
        }
        // Mirror SemanticStore::open's contract: seed applied_lsn from
        // replay's max return.
        lsn.store(1, Ordering::SeqCst);
        let mut applier = HnswApplier::new(Arc::clone(&idx), Arc::clone(&lsn));
        applier.apply_batch(&[ins(id_b, 2.0, 2)]).unwrap();
        assert_eq!(idx.read().unwrap().len(), 2);
        assert_eq!(lsn.load(Ordering::SeqCst), 2);
    }
}
