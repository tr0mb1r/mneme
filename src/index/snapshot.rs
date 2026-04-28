//! Atomic snapshot save/load for the entire [`HnswIndex`] state.
//!
//! Per spec §8.2, the orchestrator periodically (every 1000 inserts or
//! 60 minutes) consolidates the in-memory HNSW into `hnsw.idx`. This
//! module is the bottom half: serialise + atomic-rename. The
//! orchestrator handles the *when* and the read-lock dance.
//!
//! # Format
//!
//! Postcard, with a 16-byte magic header so the file is identifiable
//! even when divorced from the surrounding `~/.mneme/` tree:
//!
//! ```text
//! +----------------+--------+--------------------+-------------------+
//! | "MNEME-HNSW-IDX" | u16(LE) | u64(LE) applied_lsn | postcard payload |
//! +----------------+--------+--------------------+-------------------+
//! ```
//!
//! `schema` versions:
//! * `1` — Phase 3 §6 baseline. **No longer accepted.** Pre-snapshot
//!   scheduler files lacked an `applied_lsn`, so we cannot tell how
//!   far through the WAL they cover. Loading a v1 file would
//!   double-insert every WAL record on restart. Callers must delete
//!   `hnsw.idx` and let the next snapshot scheduler tick rewrite it.
//! * `2` — Phase 3 §8 with snapshot scheduler. Records the highest
//!   LSN folded into the snapshot so [`crate::index::delta::replay_into`]
//!   knows where to resume.
//!
//! # Crash safety
//!
//! `save` writes to `<path>.tmp`, fsyncs the file, then renames.
//! Renames are atomic on POSIX; on Windows we fall back to
//! `replace_file` semantics via `std::fs::rename`. A `kill -9`
//! mid-save leaves either the previous good file or the new one — never
//! a half-written `<path>`.

use crate::index::hnsw::HnswIndex;
use crate::{MnemeError, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 14] = b"MNEME-HNSW-IDX";
const CURRENT_SCHEMA: u16 = 2;

/// Persist the full index state to `path` atomically.
///
/// `applied_lsn` is the highest semantic-WAL LSN whose effect is
/// already folded into `index`. Stored alongside the index so
/// recovery knows where to resume replay; setting it too low causes
/// duplicate work on restart, setting it too high causes data loss
/// (records get skipped). Caller is expected to read it from the
/// matching `Arc<AtomicU64>` exposed by
/// [`crate::index::delta::HnswApplier`].
///
/// Caller is expected to have a stable view of `index` for the
/// duration of this call — typically the orchestrator holds a read
/// lock on `Arc<RwLock<HnswIndex>>` while invoking `save`.
pub fn save(index: &HnswIndex, applied_lsn: u64, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MnemeError::Index(format!("create snapshot parent: {e}")))?;
    }
    let payload = postcard::to_allocvec(index)
        .map_err(|e| MnemeError::Index(format!("postcard encode snapshot: {e}")))?;

    let tmp_path = tmp_path_for(path);
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|e| MnemeError::Index(format!("open snapshot tmp: {e}")))?;
        f.write_all(MAGIC)
            .map_err(|e| MnemeError::Index(format!("write magic: {e}")))?;
        f.write_all(&CURRENT_SCHEMA.to_le_bytes())
            .map_err(|e| MnemeError::Index(format!("write schema: {e}")))?;
        f.write_all(&applied_lsn.to_le_bytes())
            .map_err(|e| MnemeError::Index(format!("write applied_lsn: {e}")))?;
        f.write_all(&payload)
            .map_err(|e| MnemeError::Index(format!("write payload: {e}")))?;
        // sync_all also covers the inode; needed before rename or the
        // rename can outlive the data on some filesystems (xfs, ext4
        // without data=ordered).
        f.sync_all()
            .map_err(|e| MnemeError::Index(format!("fsync snapshot: {e}")))?;
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|e| MnemeError::Index(format!("atomic rename snapshot: {e}")))?;
    Ok(())
}

/// Read a snapshot from `path`. Errors on missing file, magic
/// mismatch, schema mismatch, or malformed postcard. Returns the
/// loaded index alongside the `applied_lsn` it corresponds to.
pub fn load(path: &Path) -> Result<(HnswIndex, u64)> {
    let bytes = std::fs::read(path)
        .map_err(|e| MnemeError::Index(format!("read snapshot {path:?}: {e}")))?;
    if bytes.len() < MAGIC.len() + 2 + 8 {
        return Err(MnemeError::Index(format!(
            "snapshot {path:?} too short ({} bytes)",
            bytes.len()
        )));
    }
    let (magic, rest) = bytes.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(MnemeError::Index(format!(
            "snapshot {path:?} missing MNEME-HNSW-IDX magic"
        )));
    }
    let (schema_bytes, rest) = rest.split_at(2);
    let schema = u16::from_le_bytes([schema_bytes[0], schema_bytes[1]]);
    if schema != CURRENT_SCHEMA {
        return Err(MnemeError::Index(format!(
            "snapshot {path:?} schema {schema} != supported {CURRENT_SCHEMA}; \
             delete the file and let the snapshot scheduler rewrite it"
        )));
    }
    let (lsn_bytes, payload) = rest.split_at(8);
    let applied_lsn = u64::from_le_bytes(lsn_bytes.try_into().unwrap());
    let idx = postcard::from_bytes::<HnswIndex>(payload)
        .map_err(|e| MnemeError::Index(format!("decode snapshot {path:?}: {e}")))?;
    Ok((idx, applied_lsn))
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MemoryId;
    use tempfile::TempDir;

    fn vec_for(seed: f32) -> Vec<f32> {
        let raw = [
            (seed * 0.91).sin(),
            (seed * 0.91).cos(),
            (seed * 1.73).sin(),
            (seed * 1.73).cos(),
        ];
        let n: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        raw.iter().map(|x| x / n).collect()
    }

    #[test]
    fn round_trip_pending_only() {
        let mut idx = HnswIndex::new(4);
        let id = MemoryId::new();
        idx.insert(id, &vec_for(1.0)).unwrap();

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hnsw.idx");
        save(&idx, 0, &path).unwrap();
        let (loaded, lsn) = load(&path).unwrap();
        assert_eq!(lsn, 0);
        assert_eq!(loaded.len(), 1);
        let hits = loaded.search(&vec_for(1.0), 1).unwrap();
        assert_eq!(hits[0].0, id);
    }

    #[test]
    fn round_trip_after_rebuild() {
        let mut idx = HnswIndex::new(4);
        let target = MemoryId::new();
        idx.insert(target, &vec_for(100.0)).unwrap();
        for s in 0..30 {
            idx.insert(MemoryId::new(), &vec_for(s as f32)).unwrap();
        }
        idx.rebuild_snapshot().unwrap();

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hnsw.idx");
        save(&idx, 31, &path).unwrap();
        let (loaded, lsn) = load(&path).unwrap();
        assert_eq!(lsn, 31);
        assert_eq!(loaded.len(), 31);
        // The committed HNSW round-trips: target should still be the
        // top hit for its own query vector.
        let hits = loaded.search(&vec_for(100.0), 1).unwrap();
        assert_eq!(hits[0].0, target);
    }

    #[test]
    fn round_trip_with_tombstones() {
        let mut idx = HnswIndex::new(4);
        let dead = MemoryId::new();
        idx.insert(dead, &vec_for(1.0)).unwrap();
        for s in 0..10 {
            idx.insert(MemoryId::new(), &vec_for(s as f32 + 5.0))
                .unwrap();
        }
        idx.delete(dead).unwrap();

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hnsw.idx");
        save(&idx, 12, &path).unwrap();
        let (loaded, lsn) = load(&path).unwrap();
        assert_eq!(lsn, 12);
        assert_eq!(loaded.len(), 10);
        let hits = loaded.search(&vec_for(1.0), 5).unwrap();
        assert!(hits.iter().all(|(id, _)| *id != dead));
    }

    #[test]
    fn missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        // HnswIndex doesn't impl Debug (instant-distance's HnswMap
        // doesn't), so unwrap_err is unavailable; pattern-match
        // explicitly instead.
        match load(&tmp.path().join("nope.idx")) {
            Err(MnemeError::Index(_)) => {}
            Err(other) => panic!("expected MnemeError::Index, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn corrupted_magic_errors() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bad.idx");
        std::fs::write(&p, b"NOT-MNEME-HNSW-IDX-payload-with-extra-bytes-padding").unwrap();
        match load(&p) {
            Err(MnemeError::Index(_)) => {}
            Err(other) => panic!("expected MnemeError::Index, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn schema_v1_rejected_with_clear_message() {
        // Hand-craft a v1-style header (schema=1, no applied_lsn).
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("v1.idx");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(MAGIC).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        // Zero-byte "payload" — we never reach decode, the schema
        // check fires first.
        f.write_all(&[0u8; 32]).unwrap();
        // HnswIndex doesn't impl Debug, so `Result::unwrap_err` and
        // `{:?}`-print don't work — check by hand.
        match load(&p) {
            Err(MnemeError::Index(msg)) => {
                assert!(
                    msg.contains("schema 1") && msg.contains("delete"),
                    "expected v1-rejection message mentioning deletion, got: {msg}"
                );
            }
            Err(other) => panic!("expected schema-mismatch error, got: {other}"),
            Ok(_) => panic!("expected schema-mismatch error, got Ok"),
        }
    }

    #[test]
    fn save_creates_no_dot_tmp_after_success() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hnsw.idx");
        let mut idx = HnswIndex::new(4);
        idx.insert(MemoryId::new(), &vec_for(1.0)).unwrap();
        save(&idx, 0, &path).unwrap();
        assert!(path.exists());
        assert!(
            !tmp_path_for(&path).exists(),
            "tmp file must be renamed away"
        );
    }
}
