//! `instant-distance`-backed [`VectorIndex`] for L4 semantic search.
//!
//! # The immutability problem
//!
//! `instant-distance::HnswMap` is **immutable post-build**. There is no
//! `insert` or `delete` method — the only way to change an HNSW is to
//! rebuild it from a fresh point list. That doesn't fit our write
//! pattern (memories trickle in one at a time), so this module wraps
//! the immutable HNSW with two side-tables:
//!
//! * **Pending buffer** — vectors added since the last rebuild. Search
//!   brute-forces these in addition to the HNSW.
//! * **Tombstone set** — IDs whose **committed** entry should be hidden
//!   from search until the next rebuild. Pending entries are removed
//!   in-place by [`HnswIndex::delete`] / [`HnswIndex::replace`] so the
//!   tombstone set never has to worry about them.
//!
//! [`HnswIndex::rebuild_snapshot`] consolidates: it drops tombstoned
//! entries, builds a fresh `HnswMap` from what remains, and clears
//! both side-tables. The orchestrator in §6 calls it on the spec §8.2
//! cadence (every 1000 inserts or 60 minutes).
//!
//! # Distance: cosine on L2-normalized vectors
//!
//! Both [`crate::embed::candle_minilm`] and [`crate::embed::candle_bge`]
//! L2-normalize their outputs, so cosine similarity reduces to a dot
//! product. We use `distance = 1 - dot` so identical vectors yield 0
//! and instant-distance's nearest-first ordering matches our intuition.
//!
//! # Threading
//!
//! `HnswIndex` is `Send` but not `Sync` — concurrent insert + search
//! against the pending buffer would race. The orchestrator wraps it in
//! a `Mutex` (or, more usefully, an `RwLock`) and serializes writes
//! per spec §8.1.

use crate::ids::MemoryId;
use crate::{MnemeError, Result};
use instant_distance::{Builder, HnswMap, Point, Search};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Multiplier applied to the requested `k` when querying the committed
/// HNSW. We then filter tombstones, merge with the brute-forced
/// pending buffer, and trim back to `k`. 4× overshoot is enough that a
/// realistic tombstone rate (<25 %) doesn't underfill the result set,
/// while still leaving the bulk of search work to the HNSW.
const OVERSHOOT_FACTOR: usize = 4;

/// Newtype wrapper that gives `Vec<f32>` an `instant_distance::Point`
/// impl. Cosine distance on L2-normalized vectors.
///
/// Public so [`crate::index::snapshot`] (Phase 3 §6) can serialise the
/// `HnswMap<VectorPoint, MemoryId>` directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorPoint(pub Vec<f32>);

impl Point for VectorPoint {
    fn distance(&self, other: &Self) -> f32 {
        // Caller's contract: vectors are L2-normalized by the embedder.
        // Under that invariant, cosine_similarity = dot(a, b) and
        // cosine_distance = 1 - dot.
        let dot: f32 = self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum();
        1.0 - dot
    }
}

/// In-memory HNSW with a pending-buffer + tombstone overlay. Becomes
/// crash-safe in §6 once snapshot+delta persistence lands.
///
/// Serialize/Deserialize is provided so [`crate::index::snapshot`] can
/// persist the entire state (including the built `HnswMap`) in one
/// shot. instant-distance's `with-serde` feature handles the HnswMap
/// internals via serde-big-array.
#[derive(Serialize, Deserialize)]
pub struct HnswIndex {
    dim: usize,

    /// Every (id, vec) we currently know about, post-tombstone. The
    /// first `committed_len` entries are mirrored in `committed`; the
    /// rest are pending.
    corpus: Vec<(MemoryId, Vec<f32>)>,

    /// How many `corpus` entries the committed HNSW knows about.
    committed_len: usize,

    /// IDs whose **committed** entry should be hidden from search.
    /// Pending entries are removed in-place by `delete`/`replace`, so
    /// this set only ever masks rows that live inside `committed`.
    /// `rebuild_snapshot` drops the masked rows from `corpus`.
    tombstones: HashSet<MemoryId>,

    /// `None` until the first [`rebuild_snapshot`] call. While `None`,
    /// every search is a brute-force scan over `corpus`.
    committed: Option<HnswMap<VectorPoint, MemoryId>>,
}

impl HnswIndex {
    /// Empty index that produces `dim`-dimensional vectors. Caller
    /// must keep `dim` consistent with the embedder; mismatch is
    /// caught at `insert` time.
    pub fn new(dim: usize) -> Self {
        assert!(dim > 0, "vector dim must be positive");
        Self {
            dim,
            corpus: Vec::new(),
            committed_len: 0,
            tombstones: HashSet::new(),
            committed: None,
        }
    }

    /// Vector dimensionality the index was constructed for.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Total live entries (committed + pending − tombstoned).
    pub fn len(&self) -> usize {
        self.corpus.len().saturating_sub(self.tombstones.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many vectors have been inserted since the last rebuild.
    /// The orchestrator uses this to decide when to snapshot.
    pub fn pending_inserts(&self) -> usize {
        self.corpus.len() - self.committed_len
    }

    /// Append a vector. Idempotent on `(id, vec)` only at the corpus
    /// level — calling `insert` twice with the same `id` yields two
    /// rows, which means the higher layer (`memory::semantic`) is
    /// responsible for de-duplication via the KV store.
    pub fn insert(&mut self, id: MemoryId, vec: &[f32]) -> Result<()> {
        if vec.len() != self.dim {
            return Err(MnemeError::Index(format!(
                "vector dim {} does not match index dim {}",
                vec.len(),
                self.dim
            )));
        }
        // If this id was previously tombstoned, un-tombstone it. The
        // user is presumably re-adding the memory; let the new vector
        // take effect.
        self.tombstones.remove(&id);
        self.corpus.push((id, vec.to_vec()));
        Ok(())
    }

    /// Delete a memory by id.
    ///
    /// If the id lives in the pending buffer, it's removed in place —
    /// pending entries are always live, never tombstoned. Otherwise we
    /// add the id to `tombstones` so search hides any committed entry
    /// until the next [`rebuild_snapshot`] drops it.
    ///
    /// O(pending_size) — bounded by the snapshot interval (§8.2).
    pub fn delete(&mut self, id: MemoryId) -> Result<()> {
        if self.remove_from_pending(id) {
            // Found and removed from the pending buffer; committed is
            // guaranteed not to also hold this id under the fresh-ULID
            // discipline of `memory::semantic`.
            return Ok(());
        }
        self.tombstones.insert(id);
        Ok(())
    }

    /// Atomic re-vector for an existing id (Phase 6 `update` tool).
    ///
    /// Used when the caller has updated a memory's `content` and the
    /// embedding must change. Distinct from `delete` + `insert`: that
    /// pair leaves both vectors live in `corpus` and lets search dedup
    /// pick the closer one, which is incorrect if the new content has
    /// drifted away from the old query.
    ///
    /// Semantics:
    /// 1. If `id` exists in the pending buffer, swap-remove the old
    ///    pending entry (we have a fresh vector for it).
    /// 2. Otherwise, mark the committed entry tombstoned so search
    ///    hides it until the next rebuild.
    /// 3. Push `(id, vec)` to the pending buffer; the new vector is
    ///    immediately searchable.
    ///
    /// Caller is responsible for ensuring `id` exists at the storage
    /// layer (else this acts as a tombstoning insert and `len()`
    /// accounting will be off by one until the next rebuild).
    pub fn replace(&mut self, id: MemoryId, vec: &[f32]) -> Result<()> {
        if vec.len() != self.dim {
            return Err(MnemeError::Index(format!(
                "vector dim {} does not match index dim {}",
                vec.len(),
                self.dim
            )));
        }
        let was_in_pending = self.remove_from_pending(id);
        if !was_in_pending {
            // Hide the committed entry (if any) until rebuild collapses
            // the new pending row into a clean committed HnswMap.
            self.tombstones.insert(id);
        }
        self.corpus.push((id, vec.to_vec()));
        Ok(())
    }

    /// Remove every pending entry whose id matches `id`. Returns
    /// whether any was removed. Pending duplicates shouldn't happen
    /// under normal flow but the loop is defensive against replay
    /// quirks.
    fn remove_from_pending(&mut self, id: MemoryId) -> bool {
        let mut removed = false;
        let mut i = self.committed_len;
        while i < self.corpus.len() {
            if self.corpus[i].0 == id {
                self.corpus.swap_remove(i);
                removed = true;
                // Don't increment i — what was at the end is now at i.
            } else {
                i += 1;
            }
        }
        removed
    }

    /// Top-k cosine-nearest neighbours.
    ///
    /// 1. Ask the committed HNSW for `k * OVERSHOOT_FACTOR` candidates,
    ///    filter tombstones.
    /// 2. Brute-force the pending buffer (also filtering tombstones).
    /// 3. Merge by distance, return top-k.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(MemoryId, f32)>> {
        if query.len() != self.dim {
            return Err(MnemeError::Index(format!(
                "query dim {} does not match index dim {}",
                query.len(),
                self.dim
            )));
        }
        if k == 0 {
            return Ok(Vec::new());
        }

        let q = VectorPoint(query.to_vec());
        let mut hits: Vec<(MemoryId, f32)> = Vec::new();

        if let Some(map) = self.committed.as_ref() {
            let mut search = Search::default();
            let want = k.saturating_mul(OVERSHOOT_FACTOR);
            for item in map.search(&q, &mut search).take(want) {
                let id = *item.value;
                if self.tombstones.contains(&id) {
                    continue;
                }
                hits.push((id, item.distance));
            }
        }

        // Brute-force the pending tail. Pending entries are always
        // live — `delete`/`replace` clean them in place — so we don't
        // consult `tombstones` here.
        for (id, vec) in self.corpus[self.committed_len..].iter() {
            // distance() takes &Self, so wrap once and reuse.
            let d = q.distance(&VectorPoint(vec.clone()));
            hits.push((*id, d));
        }

        // Stable nearest-first, then dedupe (an id may surface from both
        // the HNSW and the pending buffer if memory::semantic ever
        // double-inserts; keep the smaller distance).
        hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen: HashSet<MemoryId> = HashSet::new();
        hits.retain(|(id, _)| seen.insert(*id));
        hits.truncate(k);
        Ok(hits)
    }

    /// Rebuild the HNSW from the live corpus and reset both side-tables.
    /// Called by the snapshot scheduler in §6.
    ///
    /// Tombstones only mask **committed** rows: an entry from the
    /// pending range with the same id as a tombstoned committed entry
    /// is the live successor (e.g., from `replace`) and must survive
    /// the rebuild. We split the filter accordingly.
    ///
    /// Cost is roughly O(N · log N · ef_construction); for 100 K
    /// vectors at ef=200 this is a few hundred milliseconds on a
    /// laptop — acceptable as a periodic background task.
    pub fn rebuild_snapshot(&mut self) -> Result<()> {
        let mut live: Vec<(MemoryId, Vec<f32>)> = Vec::with_capacity(self.corpus.len());
        for (i, (id, vec)) in self.corpus.iter().enumerate() {
            if i < self.committed_len && self.tombstones.contains(id) {
                continue;
            }
            live.push((*id, vec.clone()));
        }
        // Help the borrow checker; we no longer need the original.
        let _ = ();

        if live.is_empty() {
            self.corpus.clear();
            self.committed_len = 0;
            self.tombstones.clear();
            self.committed = None;
            return Ok(());
        }

        let (points, ids): (Vec<VectorPoint>, Vec<MemoryId>) = live
            .iter()
            .map(|(id, v)| (VectorPoint(v.clone()), *id))
            .unzip();

        let map = Builder::default()
            .ef_construction(200)
            .ef_search(100)
            .seed(0xC0FFEE)
            .build(points, ids);

        self.corpus = live;
        self.committed_len = self.corpus.len();
        self.tombstones.clear();
        self.committed = Some(map);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic 4-dim **L2-normalized** test vector keyed
    /// by `seed`. Uses sin/cos so different seeds give different
    /// angles — a magnitude-only family would collapse to the same
    /// direction after normalization and ruin the recall tests.
    fn vec_for(seed: f32) -> Vec<f32> {
        let raw = [
            (seed * 0.91).sin(),
            (seed * 0.91).cos(),
            (seed * 1.73).sin(),
            (seed * 1.73).cos(),
        ];
        let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        raw.iter().map(|x| x / norm).collect()
    }

    #[test]
    fn insert_then_search_finds_pending_vector() {
        let mut idx = HnswIndex::new(4);
        let id = MemoryId::new();
        idx.insert(id, &vec_for(1.0)).unwrap();
        let hits = idx.search(&vec_for(1.0), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, id);
        assert!(hits[0].1 < 0.001, "self-distance should be ~0");
    }

    #[test]
    fn rebuild_promotes_pending_into_hnsw() {
        let mut idx = HnswIndex::new(4);
        for s in 0..50 {
            idx.insert(MemoryId::new(), &vec_for(s as f32)).unwrap();
        }
        assert_eq!(idx.pending_inserts(), 50);
        idx.rebuild_snapshot().unwrap();
        assert_eq!(idx.pending_inserts(), 0);
        assert_eq!(idx.len(), 50);
    }

    #[test]
    fn search_after_rebuild_finds_committed_vector() {
        let mut idx = HnswIndex::new(4);
        let target = MemoryId::new();
        // Use a seed disjoint from the noise loop below so the target
        // vector is unique in the corpus (otherwise HNSW arbitrarily
        // picks one of the duplicates as the top hit).
        idx.insert(target, &vec_for(100.0)).unwrap();
        for s in 0..40 {
            idx.insert(MemoryId::new(), &vec_for(s as f32)).unwrap();
        }
        idx.rebuild_snapshot().unwrap();
        let hits = idx.search(&vec_for(100.0), 3).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, target, "self should be top hit");
    }

    #[test]
    fn delete_filters_committed_results() {
        let mut idx = HnswIndex::new(4);
        let target = MemoryId::new();
        idx.insert(target, &vec_for(100.0)).unwrap();
        for s in 0..40 {
            idx.insert(MemoryId::new(), &vec_for(s as f32)).unwrap();
        }
        idx.rebuild_snapshot().unwrap();

        idx.delete(target).unwrap();
        let hits = idx.search(&vec_for(100.0), 5).unwrap();
        assert!(
            hits.iter().all(|(id, _)| *id != target),
            "tombstoned id must not appear: {hits:?}"
        );
    }

    #[test]
    fn delete_filters_pending_results() {
        let mut idx = HnswIndex::new(4);
        let target = MemoryId::new();
        idx.insert(target, &vec_for(2.0)).unwrap();
        idx.delete(target).unwrap();
        let hits = idx.search(&vec_for(2.0), 5).unwrap();
        assert!(hits.iter().all(|(id, _)| *id != target));
    }

    #[test]
    fn rebuild_drops_tombstoned_entries() {
        let mut idx = HnswIndex::new(4);
        let dead = MemoryId::new();
        idx.insert(dead, &vec_for(1.0)).unwrap();
        for s in 0..10 {
            idx.insert(MemoryId::new(), &vec_for(s as f32 + 10.0))
                .unwrap();
        }
        idx.delete(dead).unwrap();
        assert_eq!(idx.len(), 10, "len excludes tombstoned entries");
        idx.rebuild_snapshot().unwrap();
        assert_eq!(idx.tombstones.len(), 0);
        assert_eq!(idx.corpus.len(), 10, "corpus pruned by rebuild");
    }

    #[test]
    fn re_insert_after_delete_un_tombstones() {
        let mut idx = HnswIndex::new(4);
        let id = MemoryId::new();
        idx.insert(id, &vec_for(1.0)).unwrap();
        idx.delete(id).unwrap();
        // Re-inserting the same id (even with a new vector) makes it
        // visible again.
        idx.insert(id, &vec_for(2.0)).unwrap();
        let hits = idx.search(&vec_for(2.0), 5).unwrap();
        assert!(hits.iter().any(|(hid, _)| *hid == id));
    }

    #[test]
    fn replace_swaps_pending_vector_in_place() {
        let mut idx = HnswIndex::new(4);
        let id = MemoryId::new();
        idx.insert(id, &vec_for(1.0)).unwrap();
        // Same id, very different vector — replace should make the new
        // one the only result for queries near the new vector.
        idx.replace(id, &vec_for(50.0)).unwrap();
        assert_eq!(idx.len(), 1, "len unchanged after pending replace");

        let near_new = idx.search(&vec_for(50.0), 5).unwrap();
        assert_eq!(near_new.len(), 1);
        assert_eq!(near_new[0].0, id);
        assert!(
            near_new[0].1 < 0.001,
            "post-replace vector should self-match"
        );

        // The old pending vector must NOT be returned. id may still
        // surface (only one live row remains) but its distance from
        // the original query reflects the NEW vector, not 0.
        let near_old = idx.search(&vec_for(1.0), 5).unwrap();
        if let Some(hit) = near_old.iter().find(|(hid, _)| *hid == id) {
            assert!(
                hit.1 > 0.05,
                "old pending vector must be gone, got distance {}",
                hit.1
            );
        }
    }

    #[test]
    fn replace_masks_committed_with_new_pending() {
        let mut idx = HnswIndex::new(4);
        let target = MemoryId::new();
        idx.insert(target, &vec_for(3.0)).unwrap();
        for s in 0..20 {
            idx.insert(MemoryId::new(), &vec_for(s as f32 + 100.0))
                .unwrap();
        }
        idx.rebuild_snapshot().unwrap();
        let pre = idx.len();

        idx.replace(target, &vec_for(50.0)).unwrap();
        assert_eq!(
            idx.len(),
            pre,
            "len unchanged: replace masks one committed, adds one pending"
        );

        // Query near the new vector — should hit target with distance ~0.
        let hits = idx.search(&vec_for(50.0), 3).unwrap();
        let target_hit = hits.iter().find(|(id, _)| *id == target).expect("target");
        assert!(
            target_hit.1 < 0.001,
            "post-replace self-distance, got {}",
            target_hit.1
        );

        // Query near the OLD vector — target may still appear (the new
        // pending vector is the only live row for `target`), but it
        // must NOT surface at distance ~0 (which would mean the
        // tombstoned committed `vec_for(3.0)` was returned).
        let hits_old = idx.search(&vec_for(3.0), 5).unwrap();
        if let Some(hit) = hits_old.iter().find(|(id, _)| *id == target) {
            assert!(
                hit.1 > 0.05,
                "old committed vector must be hidden; saw distance {}",
                hit.1
            );
        }
    }

    #[test]
    fn replace_then_rebuild_drops_committed_old_vector() {
        let mut idx = HnswIndex::new(4);
        let target = MemoryId::new();
        idx.insert(target, &vec_for(3.0)).unwrap();
        idx.rebuild_snapshot().unwrap();

        idx.replace(target, &vec_for(50.0)).unwrap();
        idx.rebuild_snapshot().unwrap();

        assert_eq!(idx.len(), 1);
        assert_eq!(idx.tombstones.len(), 0);
        let hits = idx.search(&vec_for(50.0), 1).unwrap();
        assert_eq!(hits[0].0, target);
        assert!(hits[0].1 < 0.001);
    }

    #[test]
    fn replace_dim_mismatch_errors() {
        let mut idx = HnswIndex::new(4);
        let id = MemoryId::new();
        idx.insert(id, &vec_for(1.0)).unwrap();
        let err = idx.replace(id, &[0.0; 3]).unwrap_err();
        assert!(matches!(err, MnemeError::Index(_)));
    }

    #[test]
    fn search_mixed_committed_and_pending() {
        let mut idx = HnswIndex::new(4);
        let committed_target = MemoryId::new();
        let pending_target = MemoryId::new();

        idx.insert(committed_target, &vec_for(3.0)).unwrap();
        for s in 0..20 {
            idx.insert(MemoryId::new(), &vec_for(s as f32 + 100.0))
                .unwrap();
        }
        idx.rebuild_snapshot().unwrap();

        // Now insert another vector after rebuild — lives in pending.
        idx.insert(pending_target, &vec_for(3.05)).unwrap();

        let hits = idx.search(&vec_for(3.0), 2).unwrap();
        assert_eq!(hits.len(), 2);
        let returned: HashSet<_> = hits.iter().map(|(id, _)| *id).collect();
        assert!(returned.contains(&committed_target));
        assert!(returned.contains(&pending_target));
    }

    #[test]
    fn dim_mismatch_errors() {
        let mut idx = HnswIndex::new(4);
        let err = idx.insert(MemoryId::new(), &[0.0; 3]).unwrap_err();
        assert!(matches!(err, MnemeError::Index(_)));
        let err = idx.search(&[0.0; 5], 3).unwrap_err();
        assert!(matches!(err, MnemeError::Index(_)));
    }

    #[test]
    fn search_with_zero_k_returns_empty() {
        let mut idx = HnswIndex::new(4);
        idx.insert(MemoryId::new(), &vec_for(1.0)).unwrap();
        assert!(idx.search(&vec_for(1.0), 0).unwrap().is_empty());
    }

    #[test]
    fn empty_index_search_is_empty_not_error() {
        let idx = HnswIndex::new(4);
        assert!(idx.search(&vec_for(1.0), 5).unwrap().is_empty());
    }
}
