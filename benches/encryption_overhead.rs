//! Encryption-at-rest overhead bench (ADR-0013 P10).
//!
//! Measures the per-call AEAD cost in isolation: EncryptedStorage
//! wrapping the in-memory backend (no redb, no WAL, no HNSW) vs the
//! raw in-memory backend. The delta is the bound on the steady-state
//! encryption overhead for record-level operations.
//!
//! Separately measures WAL frame encryption overhead by running the
//! WAL writer with and without an AEAD codec attached.
//!
//! The P10 gate per the encryption-at-rest plan is "≤15% p95
//! regression on recall / remember / cold-start / auto-context". The
//! more comprehensive bench against the full RedbStorage stack with
//! the StubEmbedder is part of the existing recall/remember/cold-start
//! benches — those continue to work in plaintext mode unchanged; this
//! file adds a focused bound on the *pure* crypto layer so a
//! regression in chacha20poly1305 or the envelope plumbing surfaces
//! immediately.

mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use mneme::crypto::Dek;
use mneme::storage::Storage;
use mneme::storage::memory_impl::MemoryStorage;
use mneme::storage::wal::{WalOp, WalWriter, replay, replay_encrypted};
use std::sync::Arc;
use tempfile::TempDir;

fn bench_storage_put(c: &mut Criterion) {
    let runtime = common::runtime();

    let mut group = c.benchmark_group("encrypted_storage_put");
    group.sample_size(50);

    // Baseline: plain MemoryStorage.
    group.bench_function("plain", |b| {
        let inner = MemoryStorage::new();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let key = format!("k:{i}").into_bytes();
            runtime.block_on(async {
                inner
                    .put(&key, b"bench-value-32-bytes-of-content!")
                    .await
                    .unwrap();
            });
        });
    });

    // Encrypted: same backend wrapped in EncryptedStorage.
    group.bench_function("encrypted", |b| {
        let inner = MemoryStorage::new();
        let dek = Dek::generate().unwrap();
        let wrapped = mneme::storage::EncryptedStorage::new(inner, &dek);
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let key = format!("k:{i}").into_bytes();
            runtime.block_on(async {
                wrapped
                    .put(&key, b"bench-value-32-bytes-of-content!")
                    .await
                    .unwrap();
            });
        });
    });

    group.finish();
}

fn bench_storage_get(c: &mut Criterion) {
    let runtime = common::runtime();
    let mut group = c.benchmark_group("encrypted_storage_get");
    group.sample_size(50);

    // Prefill enough rows that the get path actually hits the
    // wrapper's decrypt branch.
    const N: u64 = 1_000;

    group.bench_function("plain", |b| {
        let inner = MemoryStorage::new();
        runtime.block_on(async {
            for i in 0..N {
                inner
                    .put(format!("k:{i}").as_bytes(), b"plain-bench-value")
                    .await
                    .unwrap();
            }
        });
        let mut i = 0u64;
        b.iter(|| {
            i = (i + 1) % N;
            runtime.block_on(async {
                inner.get(format!("k:{i}").as_bytes()).await.unwrap();
            });
        });
    });

    group.bench_function("encrypted", |b| {
        let inner = MemoryStorage::new();
        let dek = Dek::generate().unwrap();
        let wrapped = mneme::storage::EncryptedStorage::new(inner, &dek);
        runtime.block_on(async {
            for i in 0..N {
                wrapped
                    .put(format!("k:{i}").as_bytes(), b"encrypted-bench-value")
                    .await
                    .unwrap();
            }
        });
        let mut i = 0u64;
        b.iter(|| {
            i = (i + 1) % N;
            runtime.block_on(async {
                wrapped.get(format!("k:{i}").as_bytes()).await.unwrap();
            });
        });
    });

    group.finish();
}

fn bench_wal_append(c: &mut Criterion) {
    let runtime = common::runtime();
    let mut group = c.benchmark_group("encrypted_wal_append");
    group.sample_size(50);

    group.bench_function("plain", |b| {
        let tmp = TempDir::new().unwrap();
        let writer = WalWriter::open(tmp.path(), 1).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            runtime.block_on(async {
                writer
                    .append(WalOp::Put {
                        key: format!("k:{i}").into_bytes(),
                        value: b"bench-value-32-bytes-of-content!".to_vec(),
                    })
                    .await
                    .unwrap();
            });
        });
    });

    group.bench_function("encrypted", |b| {
        let tmp = TempDir::new().unwrap();
        let aead = Arc::new(mneme::crypto::Aead::new(&Dek::generate().unwrap()));
        let writer = WalWriter::open_encrypted(tmp.path(), 1, aead).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            runtime.block_on(async {
                writer
                    .append(WalOp::Put {
                        key: format!("k:{i}").into_bytes(),
                        value: b"bench-value-32-bytes-of-content!".to_vec(),
                    })
                    .await
                    .unwrap();
            });
        });
    });

    group.finish();
}

fn bench_wal_replay(c: &mut Criterion) {
    let runtime = common::runtime();
    let mut group = c.benchmark_group("encrypted_wal_replay");
    group.sample_size(20);
    const N: u64 = 1_000;

    group.bench_function("plain", |b| {
        let tmp = TempDir::new().unwrap();
        let writer = WalWriter::open(tmp.path(), 1).unwrap();
        runtime.block_on(async {
            for i in 0..N {
                writer
                    .append(WalOp::Put {
                        key: format!("k:{i}").into_bytes(),
                        value: b"v".to_vec(),
                    })
                    .await
                    .unwrap();
            }
        });
        writer.shutdown().unwrap();
        b.iter(|| {
            let count = replay(tmp.path()).unwrap().count();
            assert_eq!(count as u64, N);
        });
    });

    group.bench_function("encrypted", |b| {
        let tmp = TempDir::new().unwrap();
        let aead = Arc::new(mneme::crypto::Aead::new(&Dek::generate().unwrap()));
        let writer = WalWriter::open_encrypted(tmp.path(), 1, Arc::clone(&aead)).unwrap();
        runtime.block_on(async {
            for i in 0..N {
                writer
                    .append(WalOp::Put {
                        key: format!("k:{i}").into_bytes(),
                        value: b"v".to_vec(),
                    })
                    .await
                    .unwrap();
            }
        });
        writer.shutdown().unwrap();
        b.iter(|| {
            let count = replay_encrypted(tmp.path(), Arc::clone(&aead))
                .unwrap()
                .count();
            assert_eq!(count as u64, N);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_storage_put,
    bench_storage_get,
    bench_wal_append,
    bench_wal_replay,
);
criterion_main!(benches);
