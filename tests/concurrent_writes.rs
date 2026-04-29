//! Concurrent-write smoke for the WAL group-commit pipeline.
//!
//! 32 concurrent tokio tasks each issue 1000 puts against the same
//! [`RedbStorage`]. The test asserts:
//!
//! * Every put returns `Ok` (no `WalCommand` is dropped under load).
//! * After all tasks join, every key written is present and matches.
//! * The total wall-clock duration is well under the per-write fsync
//!   bound — implicit proof that group-commit is amortizing fsync across
//!   peers. Without group-commit, 32 000 individual `fdatasync` calls
//!   would take tens of seconds even on SSD; we cap the test at 60s.

use mneme::storage::{Storage, redb_impl::RedbStorage};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const WORKERS: usize = 32;
const OPS_PER_WORKER: usize = 1000;

#[test]
fn group_commit_under_load() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });

    let started = Instant::now();
    rt.block_on(async {
        let mut handles = Vec::with_capacity(WORKERS);
        for w in 0..WORKERS {
            let s: Arc<RedbStorage> = Arc::clone(&storage);
            let handle = tokio::spawn(async move {
                for i in 0..OPS_PER_WORKER {
                    let key = format!("w{w:02}-k{i:04}").into_bytes();
                    let value = format!("v-{w}-{i}").into_bytes();
                    s.put(&key, &value).await.expect("put");
                }
            });
            handles.push(handle);
        }
        for h in handles {
            h.await.expect("worker join");
        }
    });
    let elapsed = started.elapsed();

    // The point of this assertion is "group-commit amortizes fsync" —
    // we cap at a generous bound so the test fails loudly if the
    // pipeline regressed to per-put fsync. Naive per-put on a typical
    // SSD would be ~10ms × 32k ops ≈ 320s; we're well under that on
    // any platform that's not catastrophically broken.
    //
    // Windows runners (NTFS + flush_and_close on every commit) are
    // ~3× slower than ext4/APFS; bumping the bound there avoids a
    // flake that's about platform fsync, not about whether
    // group-commit is working.
    #[cfg(windows)]
    let budget = Duration::from_secs(240);
    #[cfg(not(windows))]
    let budget = Duration::from_secs(60);
    assert!(
        elapsed < budget,
        "{} concurrent puts took {:?} (budget {:?}) — group-commit appears not to be amortizing fsync",
        WORKERS * OPS_PER_WORKER,
        elapsed,
        budget,
    );

    // Every key written must read back identical.
    rt.block_on(async {
        for w in 0..WORKERS {
            for i in 0..OPS_PER_WORKER {
                let key = format!("w{w:02}-k{i:04}").into_bytes();
                let expected = format!("v-{w}-{i}").into_bytes();
                let got = storage.get(&key).await.unwrap();
                assert_eq!(got.as_deref(), Some(expected.as_slice()), "key {key:?}");
            }
        }
    });

    // Per-worker prefix scan should return exactly OPS_PER_WORKER items.
    rt.block_on(async {
        for w in 0..WORKERS {
            let prefix = format!("w{w:02}-").into_bytes();
            let entries = storage.scan_prefix(&prefix).await.unwrap();
            assert_eq!(entries.len(), OPS_PER_WORKER, "worker {w} prefix scan");
        }
    });

    drop(storage);

    // Reopen and confirm counts survive close.
    let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });
    rt.block_on(async {
        let total = storage.scan_prefix(b"").await.unwrap().len();
        assert_eq!(total, WORKERS * OPS_PER_WORKER);
    });
}
