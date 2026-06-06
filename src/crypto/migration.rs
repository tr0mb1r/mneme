//! In-place data-dir migration between plaintext and encrypted formats
//! (ADR-0013 §D8, P5a–P5e).
//!
//! Covers every data surface, in step order:
//!
//! - `episodic/data/mneme.redb` (P5a) — drained through the source
//!   Storage stack, the file rebuilt from empty, every value re-encoded
//!   through the target stack. Episodic WAL segments are dropped after
//!   the drain (already applied to redb).
//! - `procedural/pinned.jsonl` (P5b) — whole-file envelope.
//! - `cold/<YYYY-Qn>.zst` (P5e) — sealed compressed bundles.
//! - `semantic/hnsw.idx` + `semantic/wal/` (P5d) — outstanding WAL
//!   records are folded into the snapshot, the snapshot re-encoded
//!   under the same AAD position the runtime loader uses, and the
//!   segments dropped. Snapshot and WAL move together: the boot path
//!   replays the WAL in the same mode it opens the snapshot, so
//!   leaving segments behind in the source format kills the next boot.
//! - `sessions/<id>.snapshot` (P5c) — sealed with the bare session id
//!   as AAD position, matching `Session::load_with_crypto`.
//!
//! Re-runs are repairing: rows/files already in the target format are
//! preserved, and artifacts sealed under the **legacy v1.2.0 AAD
//! positions** (bare `hnsw.idx` filename, full `<id>.snapshot`
//! filename) are detected and re-sealed under the positions the
//! runtime actually opens with.
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
use crate::index::hnsw::HnswIndex;
use crate::storage::wal::{self, ReplayRecord, WalOp};
use crate::storage::{EncryptedStorage, Storage, redb_impl::RedbStorage};
use crate::{MnemeError, Result};
use std::path::Path;
use std::sync::Arc;

/// Whole-file AAD position bytes for procedural pinned.jsonl.
/// Mirrors the constant in `memory::procedural`.
const PINNED_AAD_POSITION: &[u8] = b"pinned.jsonl/v1";

/// AAD position the **broken v1.2.0 migration** sealed `hnsw.idx`
/// under: the bare filename, instead of the
/// [`crate::index::snapshot::SNAPSHOT_AAD_POSITION`] (`hnsw.idx/v1`)
/// the runtime loader opens with. Kept only so re-runs of
/// `mneme encrypt` can detect and repair snapshots sealed under it.
const HNSW_LEGACY_AAD_POSITION: &[u8] = b"hnsw.idx";

/// Filename of the procedural pinned-items JSONL.
const PINNED_FILENAME: &str = "pinned.jsonl";

/// One-shot summary returned by the migration functions. Plumbs all
/// the way out to the CLI's stderr output so operators see what
/// happened.
#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationReport {
    /// Number of redb rows whose value was re-encoded.
    pub redb_records_migrated: usize,
    /// Number of redb rows already in the target format, preserved
    /// byte-for-byte instead of re-encoded (idempotent re-run).
    pub redb_records_skipped: usize,
    /// WAL segment files removed during the WAL-drain step.
    pub wal_segments_dropped: usize,
    /// `true` if the procedural pinned.jsonl was re-encoded (or was
    /// missing/empty, which counts as a no-op success).
    pub procedural_migrated: bool,
    /// Number of cold-archive bundles re-encoded.
    pub cold_bundles_migrated: usize,
    /// `true` if the HNSW snapshot was re-encoded into the target
    /// format (folding any outstanding semantic-WAL records first).
    pub hnsw_snapshot_migrated: bool,
    /// Semantic-WAL segment files dropped after their records were
    /// folded into the re-encoded snapshot.
    pub semantic_wal_segments_dropped: usize,
    /// Session snapshot files re-encoded into the target format.
    pub session_snapshots_migrated: usize,
}

/// Migrate `<root>/episodic` from plaintext into the encrypted format
/// keyed by `dek`. Single-shot; not crash-resumable (see module-level
/// "Crash recovery"). Per-row idempotency on the MNE1 magic means the
/// inner loop is safe under a `mneme.put` racing the migration, but
/// the migration itself assumes the data dir starts plaintext.
pub fn migrate_to_encrypted(root: &Path, dek: &Dek) -> Result<MigrationReport> {
    let episodic = root.join("episodic");
    let mut report = MigrationReport::default();

    // Step 1: drain WAL + snapshot every (k, v). On a fresh encrypt
    // the WAL frames are plaintext; on a re-run over an already (or
    // partially) encrypted dir they are sealed and must be replayed
    // through the encrypted reader — a plaintext replay would die on
    // the postcard decode. Either way the *values* come back exactly
    // as stored (sealed values stay sealed); step 3 decides per row
    // whether to seal or preserve.
    let collected = match wal::segments_look_encrypted(&episodic.join("wal"))? {
        Some(true) => drain_raw_encrypted(&episodic, dek)?,
        _ => drain_plaintext(&episodic)?,
    };

    // Step 2: drop the stale WAL segments. Safe because step 1's
    // replay folded everything into the redb materialized view; the
    // WAL files are now pure recovery overhead and the encrypted-mode
    // boot needs a clean WAL to write new (encrypted) frames into.
    report.wal_segments_dropped = drop_wal_segments(&episodic.join("wal"))?;

    // Step 2.5: delete the plaintext redb file so the encrypted store
    // is rebuilt from an empty file. Re-encoding values *in place*
    // leaves the original plaintext in redb's freed copy-on-write
    // pages, and `Database::compact()` does NOT zero them — a forensic
    // scan of the live file would recover pre-encryption plaintext.
    // Rebuilding from the drained snapshot guarantees no plaintext page
    // ever exists in the encrypted database.
    remove_redb_file(&episodic)?;

    // Step 3: open the (now empty) encrypted stack and write every
    // value sealed. Values that already carry the MNE1 magic (a
    // re-run over a partially/previously encrypted dir) MUST still be
    // written — through the *raw* backend, byte-for-byte: re-putting
    // them through the encrypting wrapper would double-seal, and
    // skipping them would silently drop the row from the
    // rebuilt-from-empty redb (step 2.5 deleted the old file).
    let raw = RedbStorage::open_encrypted(&episodic, Arc::new(Aead::new(dek)))?;
    let s = EncryptedStorage::new(Arc::clone(&raw), dek);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    rt.block_on(async {
        for (key, value) in &collected {
            if value.starts_with(&MAGIC) {
                raw.put(key, value).await?;
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
    drop(raw);

    // Step 5: re-encode the procedural pinned-items JSONL as a
    // whole-file AEAD envelope (P5b).
    let aead = Arc::new(Aead::new(dek));
    report.procedural_migrated = migrate_procedural_to_encrypted(root, &aead)?;

    // Step 6: encrypt every cold-archive bundle in place (P5c). The
    // bundles are already zstd-compressed; we seal the compressed
    // bytes whole.
    report.cold_bundles_migrated = migrate_cold_to_encrypted(root, &aead)?;

    // Step 7: HNSW + sessions — re-encode the semantic surface
    // (snapshot *and* WAL) and every session snapshot into the
    // encrypted format (ADR-0013 P5d). Idempotent: files already
    // sealed under the correct AAD are left alone; files sealed under
    // the legacy v1.2.0 AAD are repaired in place; outstanding
    // semantic-WAL records are folded into the snapshot and the
    // segments dropped, so the encrypted boot never replays a
    // plaintext WAL.
    let (hnsw_migrated, wal_dropped) = migrate_semantic(root, &aead, true)?;
    report.hnsw_snapshot_migrated = hnsw_migrated;
    report.semantic_wal_segments_dropped = wal_dropped;
    report.session_snapshots_migrated = migrate_sessions_to_encrypted(root, &aead)?;

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

    // Step 2.5: delete the encrypted redb file so the plaintext store
    // is rebuilt from an empty file — the symmetric concern to the
    // encrypt direction. Re-writing in place would leave the previous
    // ciphertext in redb's freed COW pages.
    remove_redb_file(&episodic)?;

    // Step 3: open the (now empty) plain stack and re-put every value.
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

    let aead = Arc::new(Aead::new(dek));
    report.procedural_migrated = migrate_procedural_to_plaintext(root, &aead)?;
    report.cold_bundles_migrated = migrate_cold_to_plaintext(root, &aead)?;
    let (hnsw_migrated, wal_dropped) = migrate_semantic(root, &aead, false)?;
    report.hnsw_snapshot_migrated = hnsw_migrated;
    report.semantic_wal_segments_dropped = wal_dropped;
    report.session_snapshots_migrated = migrate_sessions_to_plaintext(root, &aead)?;

    Ok(report)
}

/// Delete the redb file so a migration rebuilds it from an empty file
/// instead of mutating in place. `Database::compact()` reclaims free
/// pages but does NOT zero their bytes, so an in-place re-encode leaves
/// the pre-migration values (plaintext on encrypt, ciphertext on
/// decrypt) recoverable from the live file's slack space. Starting from
/// an empty file is the only way to guarantee the live db holds no
/// pre-migration bytes. Callers must drop every Storage handle (so the
/// WAL writer thread has joined) before calling this, and the caller's
/// drained snapshot is the source of truth for the rebuild.
fn remove_redb_file(episodic: &Path) -> Result<()> {
    let db_path = episodic.join("data").join("mneme.redb");
    if db_path.is_file() {
        std::fs::remove_file(&db_path)?;
    }
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

/// Like [`drain_plaintext`] but with the encrypted *backend* only —
/// WAL frames are AEAD-opened during replay, while the values come
/// back raw (still sealed). Used by encrypt re-runs, where the rows
/// must be preserved byte-for-byte rather than decrypted.
fn drain_raw_encrypted(episodic: &Path, dek: &Dek) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    let aead = Arc::new(Aead::new(dek));
    let s = RedbStorage::open_encrypted(episodic, aead)?;
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

/// How the HNSW snapshot currently sits on disk. The encrypt
/// migration has to accept all three non-missing states because a
/// re-run may find the output of a v1.0/v1.1 daemon (plaintext), a
/// fixed migration (sealed, correct AAD), or the broken v1.2.0
/// migration (sealed, legacy filename AAD).
enum SnapshotOnDisk {
    Missing,
    Plaintext(HnswIndex, u64),
    /// Sealed under [`crate::index::snapshot::SNAPSHOT_AAD_POSITION`].
    Sealed(HnswIndex, u64),
    /// Sealed under [`HNSW_LEGACY_AAD_POSITION`] — needs repair.
    SealedLegacy(HnswIndex, u64),
}

/// Migrate the semantic surface — `<root>/semantic/hnsw.idx` *and*
/// `<root>/semantic/wal/` — into the target format. The two move
/// together: the boot path replays the WAL in the same mode it opens
/// the snapshot, so leaving WAL segments behind in the *source*
/// format kills the next daemon boot (the v1.2.0 `-32000` bug).
///
/// Mirrors the episodic drain: fold every outstanding WAL record into
/// the snapshot, write the snapshot in the target format (AAD =
/// [`crate::index::snapshot::SNAPSHOT_AAD_POSITION`], matching the
/// runtime loader), then drop the now-redundant segments.
///
/// Returns `(snapshot_migrated, wal_segments_dropped)`. Idempotent: a
/// snapshot already in the target format with no outstanding WAL is
/// left untouched.
fn migrate_semantic(root: &Path, aead: &Aead, to_encrypted: bool) -> Result<(bool, usize)> {
    use crate::index::{delta, snapshot};

    let semantic = root.join("semantic");
    let snapshot_path = semantic.join(crate::memory::semantic::SNAPSHOT_FILE);
    let wal_dir = semantic.join("wal");

    let records = collect_semantic_wal(&wal_dir, aead)?;
    let state = load_snapshot_any(&snapshot_path, aead)?;

    let already_target = match &state {
        SnapshotOnDisk::Sealed(..) => to_encrypted,
        SnapshotOnDisk::Plaintext(..) => !to_encrypted,
        SnapshotOnDisk::SealedLegacy(..) | SnapshotOnDisk::Missing => false,
    };

    let (mut idx, applied_lsn) = match state {
        SnapshotOnDisk::Plaintext(i, l)
        | SnapshotOnDisk::Sealed(i, l)
        | SnapshotOnDisk::SealedLegacy(i, l) => (i, l),
        SnapshotOnDisk::Missing => {
            // No snapshot. If the WAL carries vector records we can
            // still preserve them — infer the dim from the first one.
            // A WAL with no vector ops (or no WAL at all) has nothing
            // worth folding; just clear the segments.
            let dim = records.iter().find_map(|r| match &r.op {
                WalOp::VectorInsert { vec, .. } | WalOp::VectorReplace { vec, .. } => {
                    Some(vec.len())
                }
                _ => None,
            });
            match dim {
                Some(d) => (HnswIndex::new(d), 0u64),
                None => return Ok((false, drop_wal_segments(&wal_dir)?)),
            }
        }
    };

    if already_target && records.is_empty() {
        return Ok((false, 0));
    }

    let max_lsn = delta::replay_into(&mut idx, records.into_iter().map(Ok), applied_lsn)?;
    snapshot::save_with_crypto(
        &idx,
        max_lsn.max(applied_lsn),
        &snapshot_path,
        to_encrypted.then_some(aead),
    )?;
    let dropped = drop_wal_segments(&wal_dir)?;
    Ok((true, dropped))
}

/// Read the HNSW snapshot in whatever state it is on disk. Sealed
/// files are tried under the correct AAD first, then the legacy
/// v1.2.0 filename AAD; failing both is a hard error (wrong DEK or
/// tampered file) — we never silently wipe the index.
fn load_snapshot_any(path: &Path, aead: &Aead) -> Result<SnapshotOnDisk> {
    use crate::index::snapshot;

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SnapshotOnDisk::Missing),
        Err(e) => return Err(MnemeError::Io(e)),
    };
    if !bytes.starts_with(&MAGIC) {
        let (idx, lsn) = snapshot::decode(&bytes, path)?;
        return Ok(SnapshotOnDisk::Plaintext(idx, lsn));
    }
    if let Ok(plain) = aead.open(AadDomain::Hnsw, snapshot::SNAPSHOT_AAD_POSITION, &bytes) {
        let (idx, lsn) = snapshot::decode(&plain, path)?;
        return Ok(SnapshotOnDisk::Sealed(idx, lsn));
    }
    let plain = aead
        .open(AadDomain::Hnsw, HNSW_LEGACY_AAD_POSITION, &bytes)
        .map_err(|e| {
            MnemeError::Crypto(format!(
                "snapshot {path:?} is sealed but opens under neither the current \
                 nor the legacy v1.2.0 AAD — wrong DEK or corrupted file: {e}"
            ))
        })?;
    let (idx, lsn) = snapshot::decode(&plain, path)?;
    Ok(SnapshotOnDisk::SealedLegacy(idx, lsn))
}

/// Collect every semantic-WAL record, sniffing whether the segments
/// carry sealed or plaintext frame payloads. A torn tail terminates
/// the collection cleanly (same semantics as boot replay); any other
/// error aborts the migration loudly.
fn collect_semantic_wal(wal_dir: &Path, aead: &Aead) -> Result<Vec<ReplayRecord>> {
    let iter = match wal::segments_look_encrypted(wal_dir)? {
        Some(true) => wal::replay_encrypted(wal_dir, Arc::new(aead.clone()))?,
        _ => wal::replay(wal_dir)?,
    };
    iter.collect()
}

/// Encrypt every session snapshot in `<root>/sessions/`. The AAD
/// position is the **bare session id** — the same bytes
/// `Session::load_with_crypto` opens with. (The v1.2.0 migration used
/// the full `<id>.snapshot` filename, so every migrated session
/// failed to load; files found in that state are re-sealed here.)
fn migrate_sessions_to_encrypted(root: &Path, aead: &Aead) -> Result<usize> {
    let mut count = 0;
    for (path, id, bytes) in session_snapshots(root)? {
        if bytes.starts_with(&MAGIC) {
            if aead.open(AadDomain::Session, id.as_bytes(), &bytes).is_ok() {
                continue; // already sealed under the correct AAD
            }
            let plain = open_session_legacy(aead, &path, &id, &bytes)?;
            file_envelope::seal_to_path(&path, AadDomain::Session, id.as_bytes(), aead, &plain)?;
        } else {
            file_envelope::seal_to_path(&path, AadDomain::Session, id.as_bytes(), aead, &bytes)?;
        }
        count += 1;
    }
    Ok(count)
}

fn migrate_sessions_to_plaintext(root: &Path, aead: &Aead) -> Result<usize> {
    let mut count = 0;
    for (path, id, bytes) in session_snapshots(root)? {
        if !bytes.starts_with(&MAGIC) {
            continue; // already plaintext
        }
        let plain = match aead.open(AadDomain::Session, id.as_bytes(), &bytes) {
            Ok(p) => p,
            Err(_) => open_session_legacy(aead, &path, &id, &bytes)?,
        };
        write_atomic_simple(&path, &plain)?;
        count += 1;
    }
    Ok(count)
}

/// Open a sealed session snapshot under the legacy v1.2.0 AAD (the
/// full filename). Failing this too is a hard error.
fn open_session_legacy(aead: &Aead, path: &Path, id: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let legacy_position = format!("{id}.snapshot");
    aead.open(AadDomain::Session, legacy_position.as_bytes(), bytes)
        .map_err(|e| {
            MnemeError::Crypto(format!(
                "session snapshot {path:?} is sealed but opens under neither the \
                 current nor the legacy v1.2.0 AAD — wrong DEK or corrupted file: {e}"
            ))
        })
}

/// Yield `(path, bare session id, file bytes)` for every
/// `<root>/sessions/<id>.snapshot`.
fn session_snapshots(root: &Path) -> Result<Vec<(std::path::PathBuf, String, Vec<u8>)>> {
    let dir = root.join("sessions");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let id = match path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|n| n.strip_suffix(".snapshot"))
        {
            Some(id) => id.to_string(),
            None => continue,
        };
        let bytes = std::fs::read(&path)?;
        out.push((path, id, bytes));
    }
    Ok(out)
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
    fn migrate_to_encrypted_leaves_no_plaintext_even_for_a_few_rows() {
        // Regression: the previous implementation re-encoded rows in
        // place and relied on `Database::compact()` to reclaim the
        // freed plaintext pages. compact() only rewrites the file when
        // there is enough free space to be worth it, so for a small
        // data dir (a realistic incremental encrypt) the old plaintext
        // values survived in slack space. The 25-row sibling test
        // happened to trigger compaction and missed this. Three rows is
        // small enough that the old path left remnants.
        let tmp = TempDir::new().unwrap();
        let written = populate_plaintext(&tmp, 3);

        let dek = Dek::generate().unwrap();
        migrate_to_encrypted(tmp.path(), &dek).unwrap();

        let redb_path = tmp.path().join("episodic").join("data").join("mneme.redb");
        let on_disk = std::fs::read(&redb_path).unwrap();
        for (_, v) in &written {
            let found = on_disk.windows(v.len()).any(|w| w == v.as_slice());
            assert!(
                !found,
                "plaintext value {:?} still present in redb slack after migration",
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

    #[test]
    fn encrypt_rerun_preserves_already_encrypted_redb_rows() {
        // Regression: step 2.5 rebuilds redb from an empty file, so a
        // re-run that *skips* already-MNE1 values (instead of writing
        // them through the raw backend) silently drops every row.
        let tmp = TempDir::new().unwrap();
        let written = populate_plaintext(&tmp, 8);
        let dek = Dek::generate().unwrap();

        migrate_to_encrypted(tmp.path(), &dek).unwrap();
        let rerun = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert_eq!(rerun.redb_records_migrated, 0);
        assert_eq!(rerun.redb_records_skipped, 8);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let s = EncryptedStorage::open_redb(&tmp.path().join("episodic"), &dek).unwrap();
            for (k, v) in &written {
                assert_eq!(
                    s.get(k).await.unwrap().as_deref(),
                    Some(v.as_slice()),
                    "row lost or double-sealed by the encrypt re-run"
                );
            }
        });
    }

    // ---------- semantic surface (hnsw.idx + semantic/wal) ----------

    use crate::ids::{MemoryId, SessionId};
    use crate::index::snapshot;
    use crate::memory::working::Session;
    use crate::storage::wal::WalWriter;

    fn unit_vec(seed: f32) -> Vec<f32> {
        let raw = [
            (seed * 0.91).sin(),
            (seed * 0.91).cos(),
            (seed * 1.73).sin(),
            (seed * 1.73).cos(),
        ];
        let n: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        raw.iter().map(|x| x / n).collect()
    }

    /// Plaintext semantic fixture exactly as a v1.0/v1.1 daemon leaves
    /// it: a 2-vector snapshot at `applied_lsn = 2` plus one
    /// outstanding `VectorInsert` (lsn 3) in the WAL. Returns the id
    /// of the WAL-only vector so tests can prove the fold happened.
    fn populate_semantic_plaintext(root: &Path) -> MemoryId {
        let semantic = root.join("semantic");
        let mut idx = HnswIndex::new(4);
        idx.insert(MemoryId::new(), &unit_vec(1.0)).unwrap();
        idx.insert(MemoryId::new(), &unit_vec(2.0)).unwrap();
        snapshot::save(&idx, 2, &semantic.join("hnsw.idx")).unwrap();

        let wal_only = MemoryId::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let w = WalWriter::open(&semantic.join("wal"), 3).unwrap();
            w.append(WalOp::VectorInsert {
                id: wal_only,
                vec: unit_vec(3.0),
            })
            .await
            .unwrap();
        });
        wal_only
    }

    fn populate_session_plaintext(root: &Path) -> SessionId {
        let mut session = Session::new();
        session.push_turn("user", "hello mneme");
        session.checkpoint(&root.join("sessions")).unwrap();
        session.id
    }

    fn semantic_wal_segments(root: &Path) -> usize {
        let dir = root.join("semantic").join("wal");
        if !dir.exists() {
            return 0;
        }
        std::fs::read_dir(&dir)
            .unwrap()
            .flat_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("log"))
            .count()
    }

    /// The test class whose absence shipped the v1.2.0 `-32000` bug:
    /// the migration's seal side checked against the *runtime's* open
    /// side, not against its own helpers.
    #[test]
    fn encrypt_migration_output_opens_with_the_runtime_loaders() {
        let tmp = TempDir::new().unwrap();
        populate_plaintext(&tmp, 3);
        let wal_only = populate_semantic_plaintext(tmp.path());
        let sid = populate_session_plaintext(tmp.path());

        let dek = Dek::generate().unwrap();
        let report = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert!(report.hnsw_snapshot_migrated);
        assert!(report.semantic_wal_segments_dropped >= 1);
        assert_eq!(report.session_snapshots_migrated, 1);

        let aead = Aead::new(&dek);
        // Exactly what SemanticStore::open_with_crypto reads at boot.
        let snapshot_path = tmp.path().join("semantic").join("hnsw.idx");
        let (idx, lsn) = snapshot::load_with_crypto(&snapshot_path, Some(&aead))
            .expect("sealed snapshot must open under the runtime AAD position");
        assert_eq!(lsn, 3);
        assert_eq!(idx.len(), 3);
        let hits = idx.search(&unit_vec(3.0), 1).unwrap();
        assert_eq!(
            hits[0].0, wal_only,
            "outstanding WAL record must be folded into the sealed snapshot"
        );
        assert_eq!(
            semantic_wal_segments(tmp.path()),
            0,
            "encrypted boot must not find leftover plaintext segments"
        );
        // Exactly what the session restore path reads at boot.
        let loaded = Session::load_with_crypto(&tmp.path().join("sessions"), sid, Some(&aead))
            .expect("sealed session must open under the bare-id AAD position");
        assert_eq!(loaded.turns.len(), 1);
    }

    /// Hand-craft the on-disk state the broken v1.2.0 migration left
    /// behind — snapshot sealed under the bare-filename AAD, WAL still
    /// plaintext, session sealed under the full-filename AAD — and
    /// prove a re-run repairs all three.
    #[test]
    fn encrypt_rerun_repairs_broken_v120_layout() {
        let tmp = TempDir::new().unwrap();
        populate_plaintext(&tmp, 2);
        let wal_only = populate_semantic_plaintext(tmp.path());
        let sid = populate_session_plaintext(tmp.path());

        let dek = Dek::generate().unwrap();
        let aead = Aead::new(&dek);

        // Seal the snapshot the way v1.2.0 did: legacy filename AAD,
        // WAL left alone.
        let snapshot_path = tmp.path().join("semantic").join("hnsw.idx");
        let bytes = std::fs::read(&snapshot_path).unwrap();
        file_envelope::seal_to_path(
            &snapshot_path,
            AadDomain::Hnsw,
            HNSW_LEGACY_AAD_POSITION,
            &aead,
            &bytes,
        )
        .unwrap();
        // Session sealed under the full filename.
        let session_path = tmp.path().join("sessions").join(format!("{sid}.snapshot"));
        let sbytes = std::fs::read(&session_path).unwrap();
        file_envelope::seal_to_path(
            &session_path,
            AadDomain::Session,
            format!("{sid}.snapshot").as_bytes(),
            &aead,
            &sbytes,
        )
        .unwrap();

        // Sanity: this is the state that killed the daemon.
        assert!(snapshot::load_with_crypto(&snapshot_path, Some(&aead)).is_err());
        assert!(Session::load_with_crypto(&tmp.path().join("sessions"), sid, Some(&aead)).is_err());

        let report = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert!(report.hnsw_snapshot_migrated, "legacy AAD must be repaired");
        assert_eq!(report.session_snapshots_migrated, 1);

        let (idx, lsn) = snapshot::load_with_crypto(&snapshot_path, Some(&aead)).unwrap();
        assert_eq!(lsn, 3);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.search(&unit_vec(3.0), 1).unwrap()[0].0, wal_only);
        assert_eq!(semantic_wal_segments(tmp.path()), 0);
        Session::load_with_crypto(&tmp.path().join("sessions"), sid, Some(&aead)).unwrap();
    }

    #[test]
    fn decrypt_migration_returns_semantic_surface_to_plaintext() {
        let tmp = TempDir::new().unwrap();
        populate_plaintext(&tmp, 2);
        let wal_only = populate_semantic_plaintext(tmp.path());
        let sid = populate_session_plaintext(tmp.path());

        let dek = Dek::generate().unwrap();
        migrate_to_encrypted(tmp.path(), &dek).unwrap();
        let report = migrate_to_plaintext(tmp.path(), &dek).unwrap();
        assert!(report.hnsw_snapshot_migrated);
        assert_eq!(report.session_snapshots_migrated, 1);

        // Plaintext loaders — what a post-decrypt boot uses.
        let snapshot_path = tmp.path().join("semantic").join("hnsw.idx");
        let (idx, lsn) = snapshot::load(&snapshot_path).unwrap();
        assert_eq!(lsn, 3);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.search(&unit_vec(3.0), 1).unwrap()[0].0, wal_only);
        assert_eq!(semantic_wal_segments(tmp.path()), 0);
        let loaded = Session::load(&tmp.path().join("sessions"), sid).unwrap();
        assert_eq!(loaded.turns.len(), 1);
    }

    #[test]
    fn semantic_migration_is_idempotent_when_already_in_target_format() {
        let tmp = TempDir::new().unwrap();
        populate_plaintext(&tmp, 2);
        populate_semantic_plaintext(tmp.path());
        populate_session_plaintext(tmp.path());

        let dek = Dek::generate().unwrap();
        migrate_to_encrypted(tmp.path(), &dek).unwrap();
        let rerun = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert!(!rerun.hnsw_snapshot_migrated);
        assert_eq!(rerun.semantic_wal_segments_dropped, 0);
        assert_eq!(rerun.session_snapshots_migrated, 0);
    }

    #[test]
    fn encrypt_migration_with_wal_but_no_snapshot_still_folds_records() {
        // A daemon younger than its first snapshot tick has WAL
        // segments but no hnsw.idx. The migration must still preserve
        // those vectors (dim inferred from the first record) — and
        // must NOT leave plaintext segments behind for the encrypted
        // boot to choke on.
        let tmp = TempDir::new().unwrap();
        populate_plaintext(&tmp, 1);
        let id = MemoryId::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let w = WalWriter::open(&tmp.path().join("semantic").join("wal"), 1).unwrap();
            w.append(WalOp::VectorInsert {
                id,
                vec: unit_vec(7.0),
            })
            .await
            .unwrap();
        });

        let dek = Dek::generate().unwrap();
        let report = migrate_to_encrypted(tmp.path(), &dek).unwrap();
        assert!(report.hnsw_snapshot_migrated);
        assert_eq!(semantic_wal_segments(tmp.path()), 0);

        let aead = Aead::new(&dek);
        let (idx, lsn) =
            snapshot::load_with_crypto(&tmp.path().join("semantic").join("hnsw.idx"), Some(&aead))
                .unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.search(&unit_vec(7.0), 1).unwrap()[0].0, id);
    }
}
