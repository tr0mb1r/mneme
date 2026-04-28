//! Auto-context latency benchmark — Phase 5 exit criterion:
//! `mneme://context` p95 < 200 ms, p99 < 400 ms (spec §13).
//!
//! Builds a populated SemanticStore + ProceduralStore +
//! EpisodicStore at `MNEME_BENCH_N` corpus size, then times one
//! `Orchestrator::build_context` call per iteration. Two variants:
//!
//! * `no_query` — procedural + episodic only. Mirrors the
//!   `mneme://context` resource path.
//! * `with_query` — folds in L4 semantic recall against a fixed
//!   query. Worst-case latency for any tool that builds context with
//!   a user query attached.

mod common;

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use mneme::memory::episodic::EpisodicStore;
use mneme::memory::procedural::ProceduralStore;
use mneme::memory::semantic::MemoryKind;
use mneme::orchestrator::{Orchestrator, TokenBudget};

fn bench_auto_context(c: &mut Criterion) {
    let runtime = common::runtime();
    let n = common::corpus_size();
    let (semantic, _storage, _tmp) = runtime.block_on(common::fresh_populated(n));

    // Wrap the same on-disk state with an EpisodicStore + ProceduralStore.
    // The episodic store shares the redb the SemanticStore hangs off; the
    // procedural store sits next to it.
    // Note: common::fresh_populated created the SemanticStore, but we need
    // direct access to the storage Arc to spin up an EpisodicStore. Build
    // a fresh harness here so the Arcs are visible.
    drop(semantic);
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let storage: Arc<dyn mneme::storage::Storage> =
        mneme::storage::memory_impl::MemoryStorage::new();
    let embedder: Arc<dyn mneme::embed::Embedder> = Arc::new(
        mneme::embed::stub::StubEmbedder::with_dim(common::BENCH_DIM),
    );
    let semantic = mneme::memory::semantic::SemanticStore::open(
        tmp.path(),
        Arc::clone(&storage),
        embedder,
        mneme::memory::semantic::SnapshotConfig::disabled(),
    )
    .expect("open SemanticStore");
    let procedural = Arc::new(ProceduralStore::open(tmp.path()).expect("open ProceduralStore"));
    let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));

    // Prefill: N semantic memories, N/4 episodic events, 8 procedural pins.
    runtime.block_on(async {
        for i in 0..n {
            semantic
                .remember(
                    &format!("memory-{i}"),
                    MemoryKind::Fact,
                    vec![],
                    "personal".into(),
                )
                .await
                .unwrap();
        }
        for i in 0..(n / 4).max(1) {
            episodic
                .record("tool_call", "personal", &format!("\"call-{i}\""))
                .await
                .unwrap();
        }
        for i in 0..8 {
            procedural
                .pin(format!("pin-{i}"), vec![], "personal".into())
                .await
                .unwrap();
        }
    });

    let orch = Orchestrator::new(
        Arc::clone(&semantic),
        Arc::clone(&procedural),
        Arc::clone(&episodic),
    );

    let mut group = c.benchmark_group("auto_context");
    group.sample_size(50);

    let budget = TokenBudget::production();
    group.bench_function(format!("no_query/n={n}"), |b| {
        b.iter(|| {
            runtime.block_on(async {
                orch.build_context(None, None, budget)
                    .await
                    .expect("build_context")
            })
        });
    });
    group.bench_function(format!("with_query/n={n}"), |b| {
        b.iter(|| {
            runtime.block_on(async {
                orch.build_context(Some("memory-500"), None, budget)
                    .await
                    .expect("build_context")
            })
        });
    });
    group.finish();
}

criterion_group!(benches, bench_auto_context);
criterion_main!(benches);
