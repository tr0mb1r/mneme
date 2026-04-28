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

pub mod layout;
pub mod lockfile;
pub mod memory_impl;
pub mod redb_impl;
pub mod wal;

#[async_trait]
pub trait Storage: Send + Sync {
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, key: &[u8]) -> Result<()>;
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    async fn flush(&self) -> Result<()>;
}
