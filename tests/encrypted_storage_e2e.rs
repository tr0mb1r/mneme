//! End-to-end of [`EncryptedStorage::open_redb`] over a real backend.
//!
//! Exercises the full P3+P4 stack: record-level AEAD over WAL-frame AEAD
//! over redb. Invariants:
//!
//! 1. Reopen with the *same* DEK recovers every value via WAL replay
//!    (both layers decrypt successfully).
//! 2. Reopen with a *different* DEK fails at open time — the WAL
//!    replay can't decrypt a single frame, so the daemon refuses to
//!    serve cleanly rather than start with a half-restored store.
//! 3. The on-disk `episodic/data/mneme.redb` does not contain any of
//!    the plaintext values — storage tier opaque.
//! 4. The on-disk WAL segments contain neither plaintext values nor
//!    plaintext keys — WAL metadata is opaque too (P4 win over P3).

use mneme::cli;
use mneme::crypto::Dek;
use mneme::storage::{EncryptedStorage, Storage};
use tempfile::TempDir;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

const PLAINTEXTS: &[(&[u8], &[u8])] = &[
    (b"mem:00000000000000000000000001", b"alpha-secret-marker"),
    (b"mem:00000000000000000000000002", b"bravo-secret-marker"),
    (b"epi:00000000000000000000000003", b"charlie-secret-marker"),
    (b"epi:00000000000000000000000004", b"delta-secret-marker"),
];

fn scaffold(tmp: &TempDir) -> std::path::PathBuf {
    let root = tmp.path().to_path_buf();
    cli::init::init_at(&root).unwrap();
    root.join("episodic")
}

#[test]
fn same_dek_recovers_all_values_after_reopen() {
    let tmp = TempDir::new().unwrap();
    let episodic = scaffold(&tmp);
    let dek = Dek::generate().unwrap();
    let dek_bytes = *dek.as_bytes();
    let rt = rt();

    // Write phase
    rt.block_on(async {
        let s = EncryptedStorage::open_redb(&episodic, &dek).unwrap();
        for (k, v) in PLAINTEXTS {
            s.put(k, v).await.unwrap();
        }
        s.flush().await.unwrap();
    });

    // Reopen phase — same DEK (reconstructed from raw bytes to prove
    // the recovery path doesn't require sharing the original handle).
    rt.block_on(async {
        let same_dek = Dek::from_bytes(dek_bytes);
        let s = EncryptedStorage::open_redb(&episodic, &same_dek).unwrap();
        for (k, v) in PLAINTEXTS {
            let got = s.get(k).await.unwrap();
            assert_eq!(got.as_deref(), Some(*v), "missing or wrong: {:?}", k);
        }
        // scan_prefix also decrypts every value
        let mut all = s.scan_prefix(b"").await.unwrap();
        all.sort();
        assert_eq!(all.len(), PLAINTEXTS.len());
    });
}

#[test]
fn wrong_dek_fails_at_open_via_wal_replay() {
    let tmp = TempDir::new().unwrap();
    let episodic = scaffold(&tmp);
    let dek = Dek::generate().unwrap();
    let rt = rt();

    rt.block_on(async {
        let s = EncryptedStorage::open_redb(&episodic, &dek).unwrap();
        s.put(b"mem:01XYZ", b"protected").await.unwrap();
        s.flush().await.unwrap();
    });

    // Reopen with a brand-new DEK — WAL replay decrypts every frame,
    // so the wrong key surfaces immediately as an open-time error
    // rather than waiting until a read miss-matches.
    let other = Dek::generate().unwrap();
    let result = EncryptedStorage::open_redb(&episodic, &other);
    assert!(
        result.is_err(),
        "wrong DEK must fail at open (WAL replay must reject)",
    );
}

#[test]
fn redb_file_contains_no_plaintext() {
    let tmp = TempDir::new().unwrap();
    let episodic = scaffold(&tmp);
    let dek = Dek::generate().unwrap();
    let rt = rt();

    rt.block_on(async {
        let s = EncryptedStorage::open_redb(&episodic, &dek).unwrap();
        for (k, v) in PLAINTEXTS {
            s.put(k, v).await.unwrap();
        }
        s.flush().await.unwrap();
    });

    // Drop the storage handle so the file is closed.
    drop(rt);

    let redb_file = episodic.join("data").join("mneme.redb");
    assert!(redb_file.is_file(), "{:?} missing", redb_file);
    let on_disk = std::fs::read(&redb_file).unwrap();

    for (_, v) in PLAINTEXTS {
        let found = on_disk.windows(v.len()).any(|w| &w == v);
        assert!(
            !found,
            "plaintext value {:?} leaked into the redb file",
            std::str::from_utf8(v).unwrap_or("<binary>")
        );
    }
}

#[test]
fn wal_segments_contain_no_plaintext() {
    let tmp = TempDir::new().unwrap();
    let episodic = scaffold(&tmp);
    let dek = Dek::generate().unwrap();
    let rt = rt();

    rt.block_on(async {
        let s = EncryptedStorage::open_redb(&episodic, &dek).unwrap();
        for (k, v) in PLAINTEXTS {
            s.put(k, v).await.unwrap();
        }
        s.flush().await.unwrap();
    });

    drop(rt);

    let wal_dir = episodic.join("wal");
    let entries: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .flat_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "log")
                .unwrap_or(false)
        })
        .collect();
    assert!(!entries.is_empty(), "no WAL segments found");

    for entry in entries {
        let bytes = std::fs::read(entry.path()).unwrap();
        for (k, v) in PLAINTEXTS {
            // P4 wins over P3: WAL frame payload is fully encrypted,
            // so neither the value nor the *key* bytes appear on disk.
            for (label, plain) in [("value", *v), ("key", *k)] {
                let found = bytes.windows(plain.len()).any(|w| w == plain);
                assert!(
                    !found,
                    "plaintext {} {:?} leaked into WAL segment {:?}",
                    label,
                    std::str::from_utf8(plain).unwrap_or("<binary>"),
                    entry.path()
                );
            }
        }
    }
}

#[test]
fn delete_then_get_returns_none_under_encryption() {
    let tmp = TempDir::new().unwrap();
    let episodic = scaffold(&tmp);
    let dek = Dek::generate().unwrap();
    let rt = rt();

    rt.block_on(async {
        let s = EncryptedStorage::open_redb(&episodic, &dek).unwrap();
        s.put(b"mem:DEL", b"delete-me").await.unwrap();
        s.delete(b"mem:DEL").await.unwrap();
        assert_eq!(s.get(b"mem:DEL").await.unwrap(), None);
    });
}
