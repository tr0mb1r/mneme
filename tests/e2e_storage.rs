//! End-to-end smoke for the Phase 2 storage stack: scaffold via the
//! init code path, write through `RedbStorage`, drop, reopen, and verify
//! every key survives.
//!
//! This isn't redundant with `storage_proptest`: this test exercises the
//! same `~/.mneme` layout (`episodic/{data,wal}/`) that `mneme init` and
//! `mneme run` produce, so it doubles as a regression test for the layout
//! contract.

use mneme::cli;
use mneme::storage::{Storage, redb_impl::RedbStorage};
use tempfile::TempDir;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn init_then_write_then_reopen() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    // 1. Scaffold the directory (same code path as `mneme init`).
    cli::init::init_at(&root).unwrap();

    // The expected tree exists.
    assert!(root.join("episodic").is_dir());
    assert!(root.join("episodic/data").is_dir());
    assert!(root.join("episodic/wal").is_dir());
    assert!(root.join("schema_version").is_file());
    assert!(root.join("config.toml").is_file());

    let rt = rt();
    let episodic = root.join("episodic");

    // 2. Open storage and write 100 keys.
    rt.block_on(async {
        let s = RedbStorage::open(&episodic).unwrap();
        for i in 0u32..100 {
            let k = format!("k{i:04}").into_bytes();
            let v = format!("v{i:04}").into_bytes();
            s.put(&k, &v).await.unwrap();
        }
    });

    // 3. Reopen and verify all 100 keys are recoverable.
    rt.block_on(async {
        let s = RedbStorage::open(&episodic).unwrap();
        let scan = s.scan_prefix(b"k").await.unwrap();
        assert_eq!(scan.len(), 100, "expected 100 keys, got {}", scan.len());
        for i in 0u32..100 {
            let k = format!("k{i:04}").into_bytes();
            let want = format!("v{i:04}").into_bytes();
            let got = s.get(&k).await.unwrap();
            assert_eq!(got.as_deref(), Some(want.as_slice()), "key {i}");
        }
    });
}
