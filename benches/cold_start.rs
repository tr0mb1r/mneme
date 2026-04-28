//! Cold-start benchmark — Phase 3 exit criterion: < 5s p95 on a
//! 100K-memory index (spec §13).
//!
//! The fixture is built once outside the timing loop:
//!
//!   1. Open a fresh `SemanticStore` with the scheduler disabled.
//!   2. Insert N memories (so the WAL has N records).
//!   3. Force a `snapshot_now()` so the on-disk state is "snapshot +
//!      empty WAL tail" — the production steady state once the
//!      background scheduler has caught up.
//!   4. Drop the store; Drop joins the WAL writer thread + fsyncs.
//!
//! Each iteration then calls `SemanticStore::open(...)`: load the
//! snapshot, replay any WAL tail (zero records here by construction),
//! open the WAL writer for new writes. That's the path `mneme run`
//! takes on every boot, which is what the budget covers.
//!
//! Two variants surface regressions in different code paths:
//!
//! * `from_snapshot`   — boots from a snapshot covering the full corpus.
//! * `from_wal_replay` — no snapshot present; full WAL replay path.
//!   At small `n` the two should be similar; at 100K the WAL replay
//!   variant exercises the `replay_into` worst case.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use mneme::embed::Embedder;
use mneme::embed::stub::StubEmbedder;
use mneme::memory::semantic::{SemanticStore, SnapshotConfig};
use mneme::storage::Storage;
use mneme::storage::memory_impl::MemoryStorage;
use tempfile::TempDir;

/// Hold the on-disk fixture between iterations. Dropping the
/// `TempDir` would delete the WAL+snapshot files; keep it alive.
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    storage: Arc<dyn Storage>,
}

async fn populate_then_snapshot(n: usize) -> Fixture {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let storage: Arc<dyn Storage> = MemoryStorage::new();
    let store = common::build_store(&root, Arc::clone(&storage), n).await;
    store.snapshot_now().await.expect("snapshot_now");
    drop(store);
    Fixture {
        _tmp: tmp,
        root,
        storage,
    }
}

async fn populate_no_snapshot(n: usize) -> Fixture {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let storage: Arc<dyn Storage> = MemoryStorage::new();
    let store = common::build_store(&root, Arc::clone(&storage), n).await;
    drop(store);
    Fixture {
        _tmp: tmp,
        root,
        storage,
    }
}

fn open_once(fix: &Fixture) {
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(common::BENCH_DIM));
    let store = SemanticStore::open(
        &fix.root,
        Arc::clone(&fix.storage),
        embedder,
        SnapshotConfig::disabled(),
    )
    .expect("open SemanticStore");
    // Drop synchronously inside the bench body so the per-iteration
    // cost includes the WAL writer thread join — that's part of any
    // real reopen sequence (e.g. `mneme stop` followed by a restart).
    drop(store);
}

fn bench_cold_start(c: &mut Criterion) {
    let runtime = common::runtime();
    let n = common::corpus_size();

    let snap_fix = runtime.block_on(populate_then_snapshot(n));
    let wal_fix = runtime.block_on(populate_no_snapshot(n));

    let mut group = c.benchmark_group("cold_start");
    // Snapshot path is fast — keep sample_size at criterion's default.
    // WAL replay can be slower at large N; criterion auto-adjusts.
    group.sample_size(20);

    group.bench_function(format!("from_snapshot/n={n}"), |b| {
        b.iter(|| open_once(&snap_fix));
    });
    group.bench_function(format!("from_wal_replay/n={n}"), |b| {
        b.iter(|| open_once(&wal_fix));
    });
    group.finish();
}

criterion_group!(benches, bench_cold_start);
criterion_main!(benches);
