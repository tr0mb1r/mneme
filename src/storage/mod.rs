//! Storage seam for the persistence layer.
//!
//! Concrete implementations (redb, in-memory for tests) live in submodules.
//! The trait protects spec principle #10 ("build for ten years"): swapping
//! the backend should stay local to this module.
//!
//! Async because the durable backend (Phase 2) routes writes through a
//! dedicated WAL writer task with group-commit semantics — a sync facade
//! would force `block_on` and lose backpressure.

use crate::Result;
use async_trait::async_trait;

pub mod archive;
pub mod encrypted;
pub mod layout;
pub mod lockfile;
pub mod memory_impl;
pub mod redb_impl;
pub mod wal;

pub use encrypted::EncryptedStorage;

/// Key prefix for semantic memory metadata in the KV store.
/// Memory items live at `MEM_KEY_PREFIX || ulid_bytes`.
pub(crate) const MEM_KEY_PREFIX: &[u8] = b"mem:";

/// Supersession links: `SUP_KEY_PREFIX || old_ulid_bytes` holds the
/// 16-byte ULID of the memory that replaced it. Kept beside the item
/// rather than inside [`MemoryItem`](crate::memory::semantic::MemoryItem)
/// because that struct is postcard-encoded (not self-describing), so a
/// new field would make every existing record undecodable.
pub(crate) const SUP_KEY_PREFIX: &[u8] = b"sup:";

/// Last content edit: `MTS_KEY_PREFIX || ulid_bytes` holds an `i64`
/// big-endian count of milliseconds since the Unix epoch. Written only
/// when `update` changes a memory's content; an absent row means the
/// memory was never edited, so its `updated_at` is its `created_at`.
/// A side table for the same reason as [`SUP_KEY_PREFIX`].
pub(crate) const MTS_KEY_PREFIX: &[u8] = b"mts:";

/// Lifecycle status: `ST_KEY_PREFIX || ulid_bytes` holds one byte and
/// exists only for an archived memory. An absent row means active.
pub(crate) const ST_KEY_PREFIX: &[u8] = b"st:";

/// Typed relations from a memory:
/// `REL_KEY_PREFIX || from_ulid || type || tag || to_key`. Layout and
/// value encoding are documented in
/// [`crate::memory::relations`].
pub(crate) const REL_KEY_PREFIX: &[u8] = b"rel:";

/// The same relations keyed from the target:
/// `RREL_KEY_PREFIX || tag || to_key || type || from_ulid`.
pub(crate) const RREL_KEY_PREFIX: &[u8] = b"rrel:";

/// Provenance: `PROV_KEY_PREFIX || ulid_bytes` holds a version byte and
/// a postcard [`ProvenanceRecord`](crate::memory::relations::ProvenanceRecord).
/// Absent for a memory stored without one.
pub(crate) const PROV_KEY_PREFIX: &[u8] = b"prov:";

#[async_trait]
pub trait Storage: Send + Sync {
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, key: &[u8]) -> Result<()>;
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    async fn flush(&self) -> Result<()>;
}
