//! redb-backed [`Storage`] implementation.
//!
//! # Durability model
//!
//! WAL-first, redb-as-materialized-view: every mutating operation is appended
//! to the WAL and fsynced before the redb transaction commits. The redb
//! transaction also bumps an `applied_lsn` row, so on startup we know exactly
//! where to resume replay. The WAL writer thread runs the redb commit on its
//! own thread (via the [`Applier`](super::wal::Applier) hook), giving us
//! LSN-ordered serialization for free — no in-process ordering tricks.
//!
//! # Recovery
//!
//! On open:
//!   1. Open redb, read `applied_lsn` (default 0 if missing).
//!   2. Iterate every WAL record. Records with `lsn > applied_lsn` are
//!      replayed into redb in a single batch transaction.
//!   3. Open the `WalWriter` at `max_observed_lsn + 1` so new appends
//!      continue the LSN sequence.

use crate::storage::{
    Storage,
    wal::{self, Applier, ReplayRecord, WalOp, WalWriter},
};
use crate::{MnemeError, Result};
use async_trait::async_trait;
use redb::{Database, TableDefinition, TableError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DATA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mneme");
const META_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("__mneme_meta__");
const APPLIED_LSN_KEY: &[u8] = b"applied_lsn";

pub struct RedbStorage {
    db: Arc<Database>,
    writer: Arc<WalWriter>,
}

impl RedbStorage {
    /// Open (or create) a [`RedbStorage`] rooted at `root`.
    ///
    /// Layout:
    /// ```text
    /// {root}/
    ///   data/mneme.redb
    ///   wal/wal-{lsn:016x}.log
    /// ```
    ///
    /// Per spec §7.3 the caller passes `~/.mneme/episodic`.
    pub fn open(root: &Path) -> Result<Arc<Self>> {
        let data_dir = root.join("data");
        let wal_dir = root.join("wal");
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&wal_dir)?;

        let db_path = data_dir.join("mneme.redb");
        let db = Arc::new(Database::create(&db_path)?);

        // 1. Read applied_lsn (durable from previous runs).
        let applied_lsn = read_applied_lsn(&db)?;

        // 2. Replay any WAL records past applied_lsn into redb.
        let mut max_observed = applied_lsn;
        let mut to_apply: Vec<ReplayRecord> = Vec::new();
        for r in wal::replay(&wal_dir)? {
            let rec = r?;
            if rec.lsn > max_observed {
                max_observed = rec.lsn;
            }
            if rec.lsn > applied_lsn {
                to_apply.push(rec);
            }
        }
        if !to_apply.is_empty() {
            apply_batch_to_redb(&db, &to_apply)?;
        }

        // 3. Open the WAL writer at next_lsn = max_observed + 1, with the
        //    redb applier wired in.
        let applier = RedbApplier {
            db: Arc::clone(&db),
        };
        let writer = WalWriter::open_with_applier(&wal_dir, max_observed + 1, Box::new(applier))?;

        Ok(Arc::new(Self {
            db,
            writer: Arc::new(writer),
        }))
    }

    /// Last LSN known to be applied to redb. Useful for diagnostics.
    pub fn applied_lsn(&self) -> Result<u64> {
        read_applied_lsn(&self.db)
    }

    /// Path to the redb data directory (under `root`).
    pub fn data_dir(root: &Path) -> PathBuf {
        root.join("data")
    }

    /// Path to the WAL directory (under `root`).
    pub fn wal_dir(root: &Path) -> PathBuf {
        root.join("wal")
    }
}

#[async_trait]
impl Storage for RedbStorage {
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.writer
            .append(WalOp::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            })
            .await?;
        Ok(())
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let db = Arc::clone(&self.db);
        let key = key.to_vec();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
            let txn = db.begin_read()?;
            let table = match txn.open_table(DATA_TABLE) {
                Ok(t) => t,
                Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            Ok(table.get(key.as_slice())?.map(|v| v.value().to_vec()))
        })
        .await
        .map_err(|e| MnemeError::Storage(format!("join: {e}")))?
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        self.writer
            .append(WalOp::Delete { key: key.to_vec() })
            .await?;
        Ok(())
    }

    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let db = Arc::clone(&self.db);
        let prefix = prefix.to_vec();
        tokio::task::spawn_blocking(move || -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
            let txn = db.begin_read()?;
            let table = match txn.open_table(DATA_TABLE) {
                Ok(t) => t,
                Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            let mut out = Vec::new();
            let range = table.range(prefix.as_slice()..)?;
            for entry in range {
                let (k_guard, v_guard) = entry?;
                let k_bytes: &[u8] = k_guard.value();
                if !k_bytes.starts_with(&prefix) {
                    break;
                }
                out.push((k_bytes.to_vec(), v_guard.value().to_vec()));
            }
            Ok(out)
        })
        .await
        .map_err(|e| MnemeError::Storage(format!("join: {e}")))?
    }

    async fn flush(&self) -> Result<()> {
        // Per-write fsync is already in the WAL contract; no extra work.
        Ok(())
    }
}

// ---------- Internals ----------

struct RedbApplier {
    db: Arc<Database>,
}

impl Applier for RedbApplier {
    fn apply_batch(&mut self, records: &[ReplayRecord]) -> Result<()> {
        apply_batch_to_redb(&self.db, records)
    }
}

fn apply_batch_to_redb(db: &Database, records: &[ReplayRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let txn = db.begin_write()?;
    let mut max_lsn: u64 = 0;
    {
        let mut t = txn.open_table(DATA_TABLE)?;
        for rec in records {
            if rec.lsn > max_lsn {
                max_lsn = rec.lsn;
            }
            match &rec.op {
                WalOp::Put { key, value } => {
                    t.insert(key.as_slice(), value.as_slice())?;
                }
                WalOp::Delete { key } => {
                    t.remove(key.as_slice())?;
                }
                // Vector ops belong to the semantic-index WAL, not redb's.
                // Encountering one here means a misconfigured caller is
                // routing through the wrong WalWriter — log and skip rather
                // than corrupt the redb table.
                WalOp::VectorInsert { .. }
                | WalOp::VectorDelete { .. }
                | WalOp::VectorReplace { .. } => {
                    tracing::error!(
                        lsn = rec.lsn,
                        "vector WalOp routed to redb applier; skipping"
                    );
                }
            }
        }
    }
    {
        let mut m = txn.open_table(META_TABLE)?;
        m.insert(APPLIED_LSN_KEY, &max_lsn.to_le_bytes()[..])?;
    }
    txn.commit()?;
    Ok(())
}

fn read_applied_lsn(db: &Database) -> Result<u64> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(META_TABLE) {
        Ok(t) => t,
        Err(TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    match table.get(APPLIED_LSN_KEY)? {
        Some(v) => {
            let bytes: &[u8] = v.value();
            if bytes.len() != 8 {
                return Err(MnemeError::Storage(format!(
                    "applied_lsn has wrong length: {}",
                    bytes.len()
                )));
            }
            Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
        }
        None => Ok(0),
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn put_get_delete_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            s.put(b"hello", b"world").await.unwrap();
            assert_eq!(s.get(b"hello").await.unwrap(), Some(b"world".to_vec()));
            assert_eq!(s.get(b"nope").await.unwrap(), None);
            s.delete(b"hello").await.unwrap();
            assert_eq!(s.get(b"hello").await.unwrap(), None);
        });
    }

    #[test]
    fn writes_survive_reopen() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            for i in 0u8..50 {
                s.put(&[i], &[i.wrapping_mul(2)]).await.unwrap();
            }
            // Drop without explicit shutdown — applied_lsn is durable in redb.
        });

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            for i in 0u8..50 {
                let v = s.get(&[i]).await.unwrap();
                assert_eq!(v.as_deref(), Some(&[i.wrapping_mul(2)][..]));
            }
        });
    }

    #[test]
    fn scan_prefix_returns_only_matching_keys() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            s.put(b"user:alice", b"1").await.unwrap();
            s.put(b"user:bob", b"2").await.unwrap();
            s.put(b"user:carol", b"3").await.unwrap();
            s.put(b"meta:version", b"1").await.unwrap();

            let users = s.scan_prefix(b"user:").await.unwrap();
            let users_keys: Vec<_> = users.iter().map(|(k, _)| k.clone()).collect();
            assert_eq!(
                users_keys,
                vec![
                    b"user:alice".to_vec(),
                    b"user:bob".to_vec(),
                    b"user:carol".to_vec(),
                ]
            );

            let meta = s.scan_prefix(b"meta:").await.unwrap();
            assert_eq!(meta.len(), 1);
            assert_eq!(meta[0].0, b"meta:version".to_vec());

            let none = s.scan_prefix(b"zzz").await.unwrap();
            assert!(none.is_empty());
        });
    }

    #[test]
    fn applied_lsn_advances_with_writes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            assert_eq!(s.applied_lsn().unwrap(), 0);
            s.put(b"a", b"1").await.unwrap();
            assert!(s.applied_lsn().unwrap() >= 1);
            s.put(b"b", b"2").await.unwrap();
            s.put(b"c", b"3").await.unwrap();
            assert!(s.applied_lsn().unwrap() >= 3);
        });
    }

    #[test]
    fn delete_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            s.put(b"k", b"v").await.unwrap();
            s.delete(b"k").await.unwrap();
        });

        rt.block_on(async {
            let s = RedbStorage::open(root).unwrap();
            assert_eq!(s.get(b"k").await.unwrap(), None);
        });
    }
}
