//! Vector index seam.
//!
//! Phase 3 §5 ships [`hnsw::HnswIndex`], an `instant-distance`-backed
//! HNSW with a pending-buffer + tombstone overlay (the library is
//! immutable post-build, so we layer mutation on top). Phase 3 §6 adds
//! `snapshot` + `delta` files for crash-safe persistence.
//!
//! The [`VectorIndex`] trait still defines the abstract surface used
//! by [`crate::memory::semantic`] in §7. `HnswIndex` will grow to
//! satisfy it once §6 wires snapshot/load.

use crate::Result;
use crate::ids::MemoryId;
use std::path::Path;

pub mod delta;
pub mod hnsw;
pub mod snapshot;

pub trait VectorIndex {
    fn insert(&mut self, id: MemoryId, vec: &[f32]) -> Result<()>;
    fn delete(&mut self, id: MemoryId) -> Result<()>;
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(MemoryId, f32)>>;
    fn snapshot(&self, path: &Path) -> Result<()>;
    fn load(path: &Path) -> Result<Self>
    where
        Self: Sized;
}
