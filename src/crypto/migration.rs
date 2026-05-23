//! In-place data-dir migration between plaintext and encrypted formats
//! (ADR-0013 §D8, P5a).
//!
//! Today's scope: **redb only**. Walks every key in `episodic/data/mneme.redb`
//! and re-encodes the value through (or out of) the encrypted Storage stack.
//! Existing WAL segments are dropped at the start — they're already applied
//! to redb (the migration triggers a final WAL replay first), so deletion is
//! safe and gives the new encrypted-mode boot a clean WAL to start from.
//!
//! Deferred to subsequent commits:
//!
//! - `procedural/pinned.jsonl` (P5b) — needs the procedural module rewired
//!   to read/write whole-file encrypted format. Pinned items remain
//!   plaintext after `mneme encrypt`; users should re-pin to record them
//!   encrypted, or wait for the P5b commit.
//! - `sessions/<id>.snapshot` (P5c) — short-lived working-session state.
//! - `index/hnsw.idx` (P5d) — regenerable from records.
//! - `cold/<YYYY-Qn>.zst` (P5e) — quarterly archives.
//!
//! ## Crash recovery
//!
//! The migration is single-shot, not crash-resumable. The walk runs
//! at memory-bus speed (no embedder, no fsync per row except via
//! WAL group commit) so the failure window is small. If `mneme encrypt`
//! does crash mid-walk, the data dir is left in a mixed-state — the
//! safest recovery is to restore from the `mneme backup` taken before
//! the upgrade, then re-run `mneme encrypt`. A future commit adds a
//! `mneme resume-encrypt` flow with a `migrating.journal` checkpoint
//! file for very large stores; v1.2 ships single-shot.
//!
//! ## Memory
//!
//! Scans collect every (key, value) pair into a single `Vec` before
//! the rewrite phase, because the original Storage handle must be
//! closed before the new one opens against the same redb file. For
//! ~100K records of ~1 KiB each this is ~100 MiB; acceptable. Larger
//! stores will need a chunked walker — that's a v1.3 improvement.

use crate::crypto::{Aead, Dek, MAGIC};
use crate::storage::{EncryptedStorage, Storage, redb_impl::RedbStorage};
use crate::{MnemeError, Result};
use std::path::Path;
use std::sync::Arc;

/// One-shot summary returned by the migration functions. Plumbs all
/// the way out to the CLI's stderr output so operators see what
/// happened.
#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationReport {
    /// Number of redb rows whose value was re-encoded.
    pub redb_records_migrated: usize,
    /// Number of redb rows we skipped because they were already in the
    /// target format (idempotent re-run).
    pub redb_records_skipped: usize,
    /// WAL segment files removed during the WAL-drain step.
    pub wal_segments_dropped: usize,
    /// `true` if the migration left some surfaces in the source
    /// format. Callers should print a warning when this is set.
    pub deferred_surfaces: bool,
}

/// Migrate `<root>/episodic` from plaintext into the encrypted format
/// keyed by `dek`. Single-shot; not crash-resumable (see module-level
/// "Crash recovery"). Per-row idempotency on the MNE1 magic means the
/// inner loop is safe under a `mneme.put` racing the migration, but
/// the migration itself assumes the data dir starts plaintext.
pub fn migrate_to_encrypted(root: &Path, dek: &Dek) -> Result<MigrationReport> {
    let episodic = root.join("episodic");
    let mut report = MigrationReport {
        deferred_surfaces: true, // procedural / sessions / hnsw / cold still pending
        ..Default::default()
    };

    // Step 1: drain WAL via plaintext replay + snapshot every (k, v).
    let collected = drain_plaintext(&episodic)?;

    // Step 2: drop the stale WAL segments. Safe because step 1's
    // replay folded everything into the redb materialized view; the
    // WAL files are now pure recovery overhead and the encrypted-mode
    // boot needs a clean WAL to write new (encrypted) frames into.
    report.wal_segments_dropped = drop_wal_segments(&episodic.join("wal"))?;

    // Step 3: open the encrypted stack and re-put every value that's
    // not already encrypted. Idempotent on MNE1 magic.
    let s = EncryptedStorage::open_redb(&episodic, dek)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    rt.block_on(async {
        for (key, value) in &collected {
            if value.starts_with(&MAGIC) {
                report.redb_records_skipped += 1;
                continue;
            }
            s.put(key, value).await?;
            report.redb_records_migrated += 1;
        }
        s.flush().await?;
        Ok::<(), MnemeError>(())
    })?;
    drop(s);

    // Step 4: compact the redb file so the old plaintext pages are
    // reclaimed rather than left as garbage. Without this, redb's
    // copy-on-write writes the new encrypted rows to fresh pages but
    // leaves the original plaintext bytes recoverable by forensic
    // tools — defeats the purpose of the migration.
    compact_redb(&episodic)?;

    Ok(report)
}

/// Symmetric reverse: migrate `<root>/episodic` from encrypted back
/// to plaintext. Used by `mneme decrypt --yes-i-really-mean-it` so
/// the user can roll back without losing their data.
pub fn migrate_to_plaintext(root: &Path, dek: &Dek) -> Result<MigrationReport> {
    let episodic = root.join("episodic");
    let mut report = MigrationReport {
        deferred_surfaces: true,
        ..Default::default()
    };

    // Step 1: drain WAL via encrypted replay + snapshot every (k, v).
    // The wrapper decrypts each value before handing it back.
    let collected = drain_encrypted(&episodic, dek)?;

    // Step 2: drop the stale WAL segments.
    report.wal_segments_dropped = drop_wal_segments(&episodic.join("wal"))?;

    // Step 3: open the plain stack and re-put every value that's not
    // already plaintext. Idempotent: values lacking the MNE1 prefix
    // are already plaintext.
    let s = RedbStorage::open(&episodic)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    rt.block_on(async {
        for (key, value) in &collected {
            // After drain_encrypted, every value has already been
            // through Aead::open, so it's already plaintext. We just
            // need to write it through the plaintext writer so the
            // WAL frames are plaintext too.
            s.put(key, value).await?;
            report.redb_records_migrated += 1;
        }
        s.flush().await?;
        Ok::<(), MnemeError>(())
    })?;
    drop(s);

    // Compact to reclaim COW pages that still hold the encrypted
    // ciphertext from the previous live state. (Symmetric concern to
    // the encrypt direction — without this, the file still contains
    // recoverable ciphertext after decrypt.)
    compact_redb(&episodic)?;

    Ok(report)
}

fn compact_redb(episodic: &Path) -> Result<()> {
    let db_path = episodic.join("data").join("mneme.redb");
    if !db_path.is_file() {
        return Ok(());
    }
    // `Database::compact` takes &mut self, so we open a fresh handle
    // after every other Storage instance has been dropped.
    let mut db = redb::Database::create(&db_path).map_err(crate::MnemeError::from)?;
    let _changed = db
        .compact()
        .map_err(|e| MnemeError::Storage(format!("redb compact: {e}")))?;
    drop(db);
    Ok(())
}

/// Open a plaintext `RedbStorage` long enough to replay the WAL into
/// redb and read out every value pair. The Storage handle is dropped
/// before returning so the WAL writer thread joins cleanly.
fn drain_plaintext(episodic: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    let s = RedbStorage::open(episodic)?;
    let collected = rt.block_on(async { s.scan_prefix(b"").await })?;
    drop(s);
    Ok(collected)
}

/// Like [`drain_plaintext`] but opens the encrypted backend. Used by
/// the reverse migration to read existing encrypted values back into
/// plaintext form for re-write.
fn drain_encrypted(episodic: &Path, dek: &Dek) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    let aead = Arc::new(Aead::new(dek));
    let backend = RedbStorage::open_encrypted(episodic, aead)?;
    let s = EncryptedStorage::new(backend, dek);
    let collected = rt.block_on(async { s.scan_prefix(b"").await })?;
    drop(s);
    Ok(collected)
}

fn drop_wal_segments(wal_dir: &Path) -> Result<usize> {
    if !wal_dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(wal_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("log") {
            std::fs::remove_file(entry.path())?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;
    use tempfile::TempDir;

    fn populate_plaintext(tmp: &TempDir, n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        let root = tmp.path();
        cli::init::init_at(root).unwrap();
        let episodic = root.join("episodic");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut written = Vec::new();
        rt.block_on(async {
            let s = RedbStorage::open(&episodic).unwrap();
            for i in 0..n {
                let k = format!("mem:{i:020}").into_bytes();
                let v = format!("plaintext-value-{i}").into_bytes();
                s.put(&k, &v).await.unwrap();
                written.push((k, v));
            }
            s.flush().await.unwrap();
        });
        written
    }

    #[test]
    fn migrate_to_encrypted_walks_existing_redb_and_redb_holds_only_ciphertext() {
        let tmp = TempDir::new().unwrap();
        let written = populate_plaintext(&tmp, 25);

        let dek = Dek::generate().unwrap();
        let report = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert_eq!(report.redb_records_migrated, 25);
        assert_eq!(report.redb_records_skipped, 0);
        assert!(report.deferred_surfaces, "procedural et al. still pending");

        // Reads through the encrypted stack return the original plaintext.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let s = EncryptedStorage::open_redb(&tmp.path().join("episodic"), &dek).unwrap();
            for (k, v) in &written {
                assert_eq!(s.get(k).await.unwrap().as_deref(), Some(v.as_slice()));
            }
        });

        // The redb file on disk no longer contains any of the
        // plaintext markers.
        let redb_path = tmp.path().join("episodic").join("data").join("mneme.redb");
        let on_disk = std::fs::read(&redb_path).unwrap();
        for (_, v) in &written {
            let found = on_disk.windows(v.len()).any(|w| w == v.as_slice());
            assert!(
                !found,
                "plaintext value {:?} still present in redb after migration",
                std::str::from_utf8(v).unwrap_or("<binary>")
            );
        }
    }

    #[test]
    fn migrate_handles_empty_dir() {
        let tmp = TempDir::new().unwrap();
        cli::init::init_at(tmp.path()).unwrap();
        let dek = Dek::generate().unwrap();
        let report = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert_eq!(report.redb_records_migrated, 0);
        assert_eq!(report.redb_records_skipped, 0);
    }

    #[test]
    fn migrate_to_plaintext_round_trips_data() {
        let tmp = TempDir::new().unwrap();
        let written = populate_plaintext(&tmp, 15);
        let dek = Dek::generate().unwrap();

        migrate_to_encrypted(tmp.path(), &dek).unwrap();
        let back = migrate_to_plaintext(tmp.path(), &dek).unwrap();
        assert_eq!(back.redb_records_migrated, 15);

        // The data dir is now back to plaintext-readable through the
        // raw RedbStorage.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let s = RedbStorage::open(&tmp.path().join("episodic")).unwrap();
            for (k, v) in &written {
                assert_eq!(s.get(k).await.unwrap().as_deref(), Some(v.as_slice()));
            }
        });
    }

    #[test]
    fn migrate_drops_existing_wal_segments() {
        let tmp = TempDir::new().unwrap();
        let _written = populate_plaintext(&tmp, 5);
        let wal_dir = tmp.path().join("episodic").join("wal");
        let segments_before: Vec<_> = std::fs::read_dir(&wal_dir)
            .unwrap()
            .flat_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("log"))
            .collect();
        assert!(
            !segments_before.is_empty(),
            "test setup must produce a WAL segment"
        );

        let dek = Dek::generate().unwrap();
        let report = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert!(report.wal_segments_dropped >= 1);
    }
}
