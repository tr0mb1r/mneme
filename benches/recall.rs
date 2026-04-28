//! Recall latency benchmark — Phase 3 exit criterion: p95 < 50ms on
//! 100K memories (spec §13).
//!
//! The corpus is built once outside the timing loop; every iteration
//! issues one `recall(query, k=10)` against the populated store. The
//! HNSW corpus stays in `pending` (not committed) since
//! `SnapshotConfig::disabled` keeps the scheduler from running
//! `rebuild_snapshot` — that intentionally measures the worst-case
//! search path: HNSW + brute-force pending tail. We separately bench
//! the post-rebuild "all-committed" path with a `_committed` variant
//! so regressions in either branch surface independently.

mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use mneme::memory::semantic::RecallFilters;

fn bench_recall(c: &mut Criterion) {
    let runtime = common::runtime();
    let n = common::corpus_size();
    let (store, _storage, _tmp) = runtime.block_on(common::fresh_populated(n));

    let mut group = c.benchmark_group("recall");
    // Sample size is criterion's per-bench iteration count; 50 keeps
    // total bench time bounded at the 100K corpus (~50 × tens-of-ms).
    group.sample_size(50);

    group.bench_function(format!("k=10/n={n}/pending"), |b| {
        b.iter(|| {
            runtime.block_on(async {
                store
                    .recall("memory-500", 10, &RecallFilters::default())
                    .await
                    .expect("recall")
            })
        });
    });

    // After a rebuild, search no longer brute-forces the pending tail
    // — the HNSW alone serves results. This is the steady-state once
    // the snapshot scheduler has caught up.
    runtime.block_on(async {
        store.snapshot_now().await.ok();
    });
    group.bench_function(format!("k=10/n={n}/committed"), |b| {
        b.iter(|| {
            runtime.block_on(async {
                store
                    .recall("memory-500", 10, &RecallFilters::default())
                    .await
                    .expect("recall")
            })
        });
    });

    group.finish();
}

criterion_group!(benches, bench_recall);
criterion_main!(benches);
