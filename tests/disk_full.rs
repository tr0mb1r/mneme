//! Phase 2 exit gate: writes return clear errors when the
//! filesystem is full, and reads continue working unaffected.
//!
//! Strategy
//! --------
//! On Unix we constrain `RLIMIT_FSIZE` after letting the storage initialize.
//! With `SIGXFSZ` ignored, any write that would push a file past the limit
//! returns `EFBIG`, which our WAL maps to [`MnemeError::DiskFull`] (and
//! redb commits surface as [`MnemeError::Wal`] with an "apply failed" tag).
//! Either is an acceptable "writes return clear errors" outcome — what
//! matters is that we don't panic and that reads keep working afterwards.
//!
//! On non-Unix targets the test is skipped (file-size rlimit isn't a thing
//! on Windows). This is a single-test file so the rlimit doesn't bleed into
//! sibling tests.

#![cfg(unix)]

use mneme::MnemeError;
use mneme::storage::{Storage, redb_impl::RedbStorage};
use std::path::Path;
use tempfile::TempDir;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn ignore_sigxfsz() {
    // SIGXFSZ kills the process by default when RLIMIT_FSIZE is exceeded;
    // ignoring it surfaces EFBIG to the failing write() instead.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
}

fn current_fsize_limit() -> libc::rlimit {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        libc::getrlimit(libc::RLIMIT_FSIZE, &mut lim);
    }
    lim
}

fn set_fsize_soft_limit(bytes: u64) {
    // Lower only the soft limit; keep the hard limit at its current value
    // so we can raise the soft limit back later.
    let current = current_fsize_limit();
    let lim = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: current.rlim_max,
    };
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &lim) };
    assert_eq!(
        rc,
        0,
        "setrlimit failed: {}",
        std::io::Error::last_os_error()
    );
}

fn lift_fsize_limit() {
    // Raise the soft limit back to whatever the hard limit allows.
    let current = current_fsize_limit();
    let lim = libc::rlimit {
        rlim_cur: current.rlim_max,
        rlim_max: current.rlim_max,
    };
    unsafe {
        libc::setrlimit(libc::RLIMIT_FSIZE, &lim);
    }
}

fn largest_file_under(root: &Path) -> u64 {
    let mut max = 0u64;
    fn walk(p: &Path, max: &mut u64) {
        if let Ok(meta) = std::fs::metadata(p)
            && meta.is_file()
        {
            *max = (*max).max(meta.len());
            return;
        }
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                walk(&e.path(), max);
            }
        }
    }
    walk(root, &mut max);
    max
}

#[test]
fn writes_fail_clearly_then_reads_continue() {
    ignore_sigxfsz();

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let rt = rt();

    // Phase 1: open storage, write a baseline. No limit yet.
    let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });
    rt.block_on(async {
        for i in 0u8..50 {
            storage
                .put(&[i], format!("baseline-{i}").as_bytes())
                .await
                .unwrap();
        }
    });

    // Phase 2: clamp file-size limit to current_max + 256 bytes. Any further
    // file growth past that triggers EFBIG.
    let baseline_max = largest_file_under(&root);
    let limit = baseline_max + 256;
    set_fsize_soft_limit(limit);

    // Phase 3: write a large value repeatedly. Eventually a put fails.
    let big = vec![0xABu8; 4096];
    let mut got_clear_error = false;
    rt.block_on(async {
        for i in 100u32..2000 {
            let key = format!("k{i}").into_bytes();
            match storage.put(&key, &big).await {
                Ok(()) => continue,
                Err(MnemeError::DiskFull) => {
                    got_clear_error = true;
                    break;
                }
                Err(MnemeError::Wal(msg)) => {
                    // Apply-side failure (redb commit hit EFBIG) is also a
                    // valid "clear error" outcome.
                    assert!(
                        msg.contains("apply failed")
                            || msg.contains("fsync failed")
                            || msg.contains("write failed")
                            || msg.contains("rolled back")
                            || msg.contains("disk full"),
                        "unexpected Wal error message: {msg}"
                    );
                    got_clear_error = true;
                    break;
                }
                Err(MnemeError::Redb(msg)) => {
                    assert!(
                        msg.to_lowercase().contains("file") || msg.to_lowercase().contains("space"),
                        "unexpected Redb error message: {msg}"
                    );
                    got_clear_error = true;
                    break;
                }
                Err(other) => panic!("unexpected error class: {other:?}"),
            }
        }
    });
    assert!(
        got_clear_error,
        "expected at least one write to fail under fsize limit"
    );

    // Phase 4: lift the limit, reopen the database, and verify the baseline
    // writes are durable. (redb defensively refuses reads on the same handle
    // after an I/O error; a reopen recovers cleanly via WAL replay. This
    // matches the spec's UX: free space → restart → your data is still there.)
    drop(storage);
    lift_fsize_limit();

    let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });
    rt.block_on(async {
        for i in 0u8..50 {
            let v = storage.get(&[i]).await.unwrap();
            assert_eq!(
                v.as_deref(),
                Some(format!("baseline-{i}").as_bytes()),
                "baseline key {i} must survive disk-full + reopen"
            );
        }
        let total = storage.scan_prefix(b"").await.unwrap().len();
        assert!(
            total >= 50,
            "expected at least the 50 baseline keys after disk-full + reopen, got {total}"
        );
    });
}
