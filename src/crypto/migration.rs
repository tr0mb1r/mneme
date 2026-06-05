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

use crate::crypto::{AadDomain, Aead, Dek, MAGIC, file_envelope};
use crate::storage::{EncryptedStorage, Storage, redb_impl::RedbStorage};
use crate::{MnemeError, Result};
use std::path::Path;
use std::sync::Arc;

/// Whole-file AAD position bytes for procedural pinned.jsonl.
/// Mirrors the constant in `memory::procedural`.
const PINNED_AAD_POSITION: &[u8] = b"pinned.jsonl/v1";

/// AAD position bytes for HNSW snapshot files.
const HNSW_AAD_POSITION: &[u8] = b"hnsw.idx/v1";

/// Filename of the procedural pinned-items JSONL.
const PINNED_FILENAME: &str = "pinned.jsonl";

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
    /// `true` if the procedural pinned.jsonl was re-encoded (or was
    /// missing/empty, which counts as a no-op success).
    pub procedural_migrated: bool,
    /// Number of cold-archive bundles re-encoded.
    pub cold_bundles_migrated: usize,
    /// HNSW snapshot files removed so the next daemon boot rebuilds
    /// (and snapshots) under the new format.
    pub hnsw_snapshots_wiped: usize,
    /// Session snapshot files removed.
    pub session_snapshots_wiped: usize,
}

/// Migrate `<root>/episodic` from plaintext into the encrypted format
/// keyed by `dek`. Single-shot; not crash-resumable (see module-level
/// "Crash recovery"). Per-row idempotency on the MNE1 magic means the
/// inner loop is safe under a `mneme.put` racing the migration, but
/// the migration itself assumes the data dir starts plaintext.
pub fn migrate_to_encrypted(root: &Path, dek: &Dek) -> Result<MigrationReport> {
    let episodic = root.join("episodic");
    let mut report = MigrationReport::default();

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

    // Step 5: re-encode the procedural pinned-items JSONL as a
    // whole-file AEAD envelope (P5b).
    let aead = Arc::new(Aead::new(dek));
    report.procedural_migrated = migrate_procedural_to_encrypted(root, &aead)?;

    // Step 6: encrypt every cold-archive bundle in place (P5c). The
    // bundles are already zstd-compressed; we seal the compressed
    // bytes whole.
    report.cold_bundles_migrated = migrate_cold_to_encrypted(root, &aead)?;

    // Step 7: HNSW + sessions — encrypt the existing plaintext files
    // in place using the same DEK-derived AEAD (ADR-0013 P5d). Both
    // surfaces now support open_with_crypto so the daemon will read
    // the encrypted files on next boot. If a file is already
    // encrypted (MNE1 magic) the helper is a no-op for that entry.
    report.hnsw_snapshots_wiped = migrate_hnsw_to_encrypted(root, &aead)?;
    report.session_snapshots_wiped = migrate_sessions_to_encrypted(root, &aead)?;

    Ok(report)
}

/// Symmetric reverse: migrate `<root>/episodic` from encrypted back
/// to plaintext. Used by `mneme decrypt --yes-i-really-mean-it` so
/// the user can roll back without losing their data.
pub fn migrate_to_plaintext(root: &Path, dek: &Dek) -> Result<MigrationReport> {
    let episodic = root.join("episodic");
    let mut report = MigrationReport::default();

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

    let aead = Arc::new(Aead::new(dek));
    report.procedural_migrated = migrate_procedural_to_plaintext(root, &aead)?;
    report.cold_bundles_migrated = migrate_cold_to_plaintext(root, &aead)?;
    report.hnsw_snapshots_wiped = migrate_hnsw_to_plaintext(root, &aead)?;
    report.session_snapshots_wiped = migrate_sessions_to_plaintext(root, &aead)?;

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

/// Encrypt `<root>/procedural/pinned.jsonl` whole-file (P5b).
///
/// Returns `Ok(true)` if the file existed and was touched; `Ok(false)`
/// if it was missing or empty (nothing to do). Idempotent: a file
/// already starting with the MNE1 envelope magic is left as-is.
fn migrate_procedural_to_encrypted(root: &Path, aead: &Aead) -> Result<bool> {
    let path = root.join("procedural").join(PINNED_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(MnemeError::Io(e)),
    };
    if bytes.is_empty() {
        return Ok(false);
    }
    if bytes.starts_with(&MAGIC) {
        return Ok(false); // already encrypted
    }
    file_envelope::seal_to_path(&path, AadDomain::Pinned, PINNED_AAD_POSITION, aead, &bytes)?;
    Ok(true)
}

/// Reverse of [`migrate_procedural_to_encrypted`]: open the envelope
/// and write the plaintext JSONL back to disk.
fn migrate_procedural_to_plaintext(root: &Path, aead: &Aead) -> Result<bool> {
    let path = root.join("procedural").join(PINNED_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(MnemeError::Io(e)),
    };
    if bytes.is_empty() {
        return Ok(false);
    }
    if !bytes.starts_with(&MAGIC) {
        return Ok(false); // already plaintext
    }
    let plaintext = aead.open(AadDomain::Pinned, PINNED_AAD_POSITION, &bytes)?;
    write_atomic_simple(&path, &plaintext)?;
    Ok(true)
}

/// Encrypt every cold-archive bundle under `<root>/cold/` in place
/// (P5c). Each `.zst` file becomes a whole-file AEAD envelope whose
/// AAD position is the bundle's bare filename (the quarter label,
/// e.g. `2026-Q2.zst`).
fn migrate_cold_to_encrypted(root: &Path, aead: &Aead) -> Result<usize> {
    let cold = root.join("cold");
    if !cold.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(&cold)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("zst") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        if bytes.starts_with(&MAGIC) {
            continue; // already encrypted
        }
        let position = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .as_bytes()
            .to_vec();
        file_envelope::seal_to_path(&path, AadDomain::Cold, &position, aead, &bytes)?;
        count += 1;
    }
    Ok(count)
}

/// Reverse of [`migrate_cold_to_encrypted`].
fn migrate_cold_to_plaintext(root: &Path, aead: &Aead) -> Result<usize> {
    let cold = root.join("cold");
    if !cold.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(&cold)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("zst") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        if !bytes.starts_with(&MAGIC) {
            continue; // already plaintext
        }
        let position = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .as_bytes()
            .to_vec();
        let plaintext = aead.open(AadDomain::Cold, &position, &bytes)?;
        write_atomic_simple(&path, &plaintext)?;
        count += 1;
    }
    Ok(count)
}

/// Encrypt the HNSW snapshot file at `<root>/semantic/hnsw.idx`
/// (note: the actual file in the v1.0 layout is under `semantic/`,
/// not `index/`; the path here matches `memory::semantic::SNAPSHOT_FILE`).
/// Idempotent on the MNE1 magic.
fn migrate_hnsw_to_encrypted(root: &Path, aead: &Aead) -> Result<usize> {
    encrypt_files_under(
        root.join("semantic"),
        AadDomain::Hnsw,
        aead,
        true,
        |fname| fname == crate::memory::semantic::SNAPSHOT_FILE,
    )
}

fn migrate_hnsw_to_plaintext(root: &Path, aead: &Aead) -> Result<usize> {
    encrypt_files_under(
        root.join("semantic"),
        AadDomain::Hnsw,
        aead,
        false,
        |fname| fname == crate::memory::semantic::SNAPSHOT_FILE,
    )
}

/// Encrypt every session snapshot in `<root>/sessions/`. Idempotent
/// on the MNE1 magic.
fn migrate_sessions_to_encrypted(root: &Path, aead: &Aead) -> Result<usize> {
    encrypt_files_under(
        root.join("sessions"),
        AadDomain::Session,
        aead,
        true,
        |fname| fname.ends_with(".snapshot"),
    )
}

fn migrate_sessions_to_plaintext(root: &Path, aead: &Aead) -> Result<usize> {
    encrypt_files_under(
        root.join("sessions"),
        AadDomain::Session,
        aead,
        false,
        |fname| fname.ends_with(".snapshot"),
    )
}

/// Walk `dir` non-recursively, and for each file matching
/// `filter(filename)`:
///
/// - if `to_encrypted` and the file is plaintext, seal it (AAD =
///   `domain || file_name_bytes`).
/// - if `!to_encrypted` and the file is encrypted, open it.
/// - idempotent otherwise.
///
/// Returns the count of files actually re-encoded. AAD position is
/// the filename bytes, which is the obvious per-file identifier for
/// session snapshots (UUID) and HNSW (single fixed name); both bind
/// the ciphertext to its slot.
fn encrypt_files_under<F: Fn(&str) -> bool>(
    dir: std::path::PathBuf,
    domain: AadDomain,
    aead: &Aead,
    to_encrypted: bool,
    filter: F,
) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let fname = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !filter(fname) {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let position = fname.as_bytes();
        if to_encrypted {
            if bytes.starts_with(&MAGIC) {
                continue;
            }
            file_envelope::seal_to_path(&path, domain, position, aead, &bytes)?;
        } else {
            if !bytes.starts_with(&MAGIC) {
                continue;
            }
            let plain = aead.open(domain, position, &bytes)?;
            write_atomic_simple(&path, &plain)?;
        }
        count += 1;
    }
    Ok(count)
}

/// Write `bytes` atomically to `path` via temp+rename, with 0o600
/// permissions on unix. Mirrors `file_envelope::seal_to_path` but
/// without the envelope step — used when we need a plain bytes
/// write (e.g. the plaintext output of a reverse migration).
fn write_atomic_simple(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_buf = path.as_os_str().to_owned();
    tmp_buf.push(".tmp");
    let tmp: std::path::PathBuf = tmp_buf.into();
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// Silence: HNSW_AAD_POSITION is reserved for the P5d wiring of
// `index/snapshot.rs` — keep the constant available so the AAD
// surface stays domain-separated when that lands.
#[allow(dead_code)]
fn _hnsw_aad_unused() -> &'static [u8] {
    HNSW_AAD_POSITION
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
