//! Shared bench helpers: tokio runtime + populated `SemanticStore`.
//!
//! Each bench file `mod`-includes this so we don't duplicate the
//! prefill loop across `recall.rs`, `remember.rs`, and `cold_start.rs`.
//!
//! All benches use [`StubEmbedder`] so they measure the storage +
//! HNSW + WAL path in isolation. Real-model latency is a separate,
//! manual pre-release exercise (BGE-M3 forward passes dominate the
//! per-call budget; that's a model question, not an architecture
//! question, and the spec §13 numbers reserve roughly half of each
//! budget for the embedder).
//!
//! `MNEME_BENCH_N` overrides the corpus size for the prefill — keep
//! it small in CI (default 1_000), larger locally to validate the
//! 100K target.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use mneme::embed::Embedder;
use mneme::embed::stub::StubEmbedder;
use mneme::memory::semantic::{MemoryKind, SemanticStore, SnapshotConfig};
use mneme::storage::Storage;
use mneme::storage::memory_impl::MemoryStorage;
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Vector dim used by every bench. 64 is large enough that cosine
/// distance computations match real-world cache pressure (per-vector
/// dot product is no longer L1-resident) without paying the full
/// 1024-d BGE-M3 cost.
pub const BENCH_DIM: usize = 64;

/// Default prefill corpus size when `MNEME_BENCH_N` is unset. 1_000
/// keeps `cargo bench` finish-time under a minute on a laptop.
pub const DEFAULT_CORPUS: usize = 1_000;

/// Read the prefill corpus size from the environment.
pub fn corpus_size() -> usize {
    std::env::var("MNEME_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CORPUS)
}

/// Build a multi-thread tokio runtime for benches. Workers default to
/// the host's logical CPU count, matching `mneme run`'s production
/// runtime shape.
pub fn runtime() -> Runtime {
    Runtime::new().expect("create tokio runtime")
}

/// Build a populated `SemanticStore` with the snapshot scheduler
/// disabled — benches don't want a background task firing
/// snapshots mid-measurement.
///
/// Returns the store plus the `TempDir` it lives in; tests must hold
/// onto the `TempDir` so the WAL files don't get cleaned up while the
/// store is still using them.
pub async fn build_store(root: &Path, storage: Arc<dyn Storage>, n: usize) -> Arc<SemanticStore> {
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(BENCH_DIM));
    let s = SemanticStore::open(root, storage, embedder, SnapshotConfig::disabled())
        .expect("open SemanticStore");
    for i in 0..n {
        s.remember(
            &format!("memory-{i}"),
            MemoryKind::Fact,
            vec!["bench".into()],
            "personal".into(),
        )
        .await
        .expect("remember failed");
    }
    s
}

/// Convenience: full setup in one call. Returns the populated store
/// and the on-disk fixtures (TempDir + the storage Arc) so the caller
/// can pass them to the bench body.
pub async fn fresh_populated(n: usize) -> (Arc<SemanticStore>, Arc<dyn Storage>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let storage: Arc<dyn Storage> = MemoryStorage::new();
    let store = build_store(tmp.path(), Arc::clone(&storage), n).await;
    (store, storage, tmp)
}
