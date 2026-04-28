//! Remember (write) latency benchmark — Phase 3 exit criterion:
//! p95 < 150ms on cold cache (spec §13).
//!
//! Like the recall bench, this isolates storage + WAL + HNSW insert
//! cost from embedding cost (`StubEmbedder` is the embedder). The
//! prefill makes sure we're measuring steady-state insert rather
//! than empty-store overhead. Snapshot scheduler is disabled so
//! HNSW's `rebuild_snapshot` doesn't fire mid-measurement.

mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use mneme::memory::semantic::MemoryKind;
use std::sync::atomic::{AtomicU64, Ordering};

fn bench_remember(c: &mut Criterion) {
    let runtime = common::runtime();
    let n = common::corpus_size();
    let (store, _storage, _tmp) = runtime.block_on(common::fresh_populated(n));

    // Each iteration writes a unique content string so we don't
    // collapse onto a single HNSW row. The atomic keeps that working
    // across criterion's repeated batches.
    let counter = AtomicU64::new(0);

    let mut group = c.benchmark_group("remember");
    group.sample_size(50);
    group.bench_function(format!("after_prefill/n={n}"), |b| {
        b.iter(|| {
            let i = counter.fetch_add(1, Ordering::SeqCst);
            runtime.block_on(async {
                store
                    .remember(
                        &format!("bench-{i}"),
                        MemoryKind::Fact,
                        vec![],
                        "personal".into(),
                    )
                    .await
                    .expect("remember")
            })
        });
    });
    group.finish();
}

criterion_group!(benches, bench_remember);
criterion_main!(benches);
