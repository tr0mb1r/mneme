//! Re-embed migration triggered by a change to
//! `config.embeddings.model`.
//!
//! When the active embedder differs from the one that produced the
//! on-disk vectors (different model name OR different output dim), the
//! existing semantic WAL + snapshot are useless: every record carries
//! a vector at the wrong dim. The dim-skip in
//! `crate::index::delta::apply_one` keeps the boot from crashing, but
//! the memories are still searchable only as orphan KV rows.
//!
//! This module restores them. On every boot we compare the configured
//! embedder identity against a tiny `<root>/semantic/embedder.json`
//! sidecar. On mismatch we:
//!
//! 1. Wipe the stale snapshot + WAL.
//! 2. Iterate every `MemoryItem` in `Storage` and re-embed `content`
//!    with the new model.
//! 3. Build a fresh in-memory HNSW from the new vectors.
//! 4. Save a snapshot at `applied_lsn = 0` (no WAL records yet).
//! 5. Write the new sidecar.
//!
//! The whole operation is idempotent: a crash mid-migration leaves the
//! KV rows untouched, the sidecar absent, so the next boot re-runs.
//! Storage rows aren't modified — we don't risk losing user data.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::embed::Embedder;
use crate::index::hnsw::HnswIndex;
use crate::index::snapshot;
use crate::memory::semantic::MemoryItem;
use crate::storage::Storage;
use crate::{MnemeError, Result};

/// Sidecar filename. Lives under `<root>/semantic/`. Schema is forward-
/// compatible — unknown fields are ignored, missing optional fields
/// default to `None`.
const SIDECAR_FILE: &str = "embedder.json";

/// Same prefix the semantic store uses for memory metadata. Re-declared
/// here so this module doesn't depend on `memory::semantic` internals.
const MEM_KEY_PREFIX: &[u8] = b"mem:";

/// Lightweight on-disk record of which embedder produced the vectors
/// the semantic layer is currently sitting on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbedderIdentity {
    /// Short name from `config.embeddings.model` (e.g. `"minilm-l6"`,
    /// `"bge-m3"`, `"stub"`).
    pub model: String,
    /// Output dim. We compare both `model` and `dim` to catch the case
    /// where a user pins a custom model with the same short name as a
    /// previous one.
    pub dim: usize,
}

impl EmbedderIdentity {
    pub fn new(model: impl Into<String>, dim: usize) -> Self {
        Self {
            model: model.into(),
            dim,
        }
    }

    fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MnemeError::Embedding(format!("create sidecar dir: {e}")))?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| MnemeError::Embedding(format!("encode embedder.json: {e}")))?;
        // Atomic temp+rename so a crash mid-write leaves the previous
        // sidecar intact (or no sidecar at all on first run).
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)
            .map_err(|e| MnemeError::Embedding(format!("write {tmp:?}: {e}")))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| MnemeError::Embedding(format!("rename {tmp:?}: {e}")))?;
        Ok(())
    }
}

/// Result of [`migrate_if_needed`]. Surfaced to logs so a sysadmin can
/// see what happened on boot.
#[derive(Debug)]
pub enum Outcome {
    /// Sidecar matched the current embedder; nothing to do.
    NoChange,
    /// Sidecar absent or mismatched; re-embedded `count` memories.
    Migrated { count: usize },
}

/// If the on-disk embedder identity disagrees with the active one,
/// re-vectorize every stored memory. Otherwise a fast no-op.
///
/// `model_name` is what the embedder calls itself externally — typically
/// `config.embeddings.model`, or `"stub"` when `MNEME_EMBEDDER=stub` is
/// set. Used as part of the identity tuple so flipping in/out of stub
/// mode also triggers a migration.
pub async fn migrate_if_needed(
    root: &Path,
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn Embedder>,
    model_name: &str,
) -> Result<Outcome> {
    let semantic_root = root.join("semantic");
    let sidecar_path = semantic_root.join(SIDECAR_FILE);
    let current = EmbedderIdentity::new(model_name, embedder.dim());

    if let Some(prev) = EmbedderIdentity::read(&sidecar_path)
        && prev == current
    {
        return Ok(Outcome::NoChange);
    }

    // Mismatch (or first boot after this fix lands). Re-embed.
    let count = re_embed_all(&semantic_root, &*storage, &*embedder).await?;
    current.write(&sidecar_path)?;
    Ok(Outcome::Migrated { count })
}

async fn re_embed_all(
    semantic_root: &Path,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
) -> Result<usize> {
    // 1. Wipe stale snapshot + WAL. Best-effort: missing files are
    //    fine; permission errors propagate.
    let snapshot_path = semantic_root.join("hnsw.idx");
    if snapshot_path.exists() {
        std::fs::remove_file(&snapshot_path)
            .map_err(|e| MnemeError::Embedding(format!("remove stale snapshot: {e}")))?;
    }
    wipe_wal_dir(&semantic_root.join("wal"))?;

    // 2. Pull every memory row, decode, re-embed, push into a fresh
    //    HNSW. We don't bother with a WAL — the snapshot we save at
    //    the end stands on its own (`applied_lsn = 0` plus an empty
    //    WAL means SemanticStore::open replays nothing).
    let raw = storage
        .scan_prefix(MEM_KEY_PREFIX)
        .await
        .map_err(|e| MnemeError::Embedding(format!("scan memories: {e}")))?;
    let total = raw.len();

    let mut index = HnswIndex::new(embedder.dim());
    let mut migrated = 0usize;
    for (key, value) in raw {
        let item: MemoryItem = match postcard::from_bytes(&value) {
            Ok(it) => it,
            Err(e) => {
                tracing::warn!(
                    key = ?key,
                    error = %e,
                    "skipping malformed MemoryItem during re-embed"
                );
                continue;
            }
        };
        let vector = embedder.embed(&item.content).await?;
        if vector.len() != embedder.dim() {
            return Err(MnemeError::Embedding(format!(
                "embedder returned dim {} but advertised {}",
                vector.len(),
                embedder.dim()
            )));
        }
        index.insert(item.id, &vector)?;
        migrated += 1;
        if migrated.is_multiple_of(100) {
            tracing::info!(progress = migrated, total, "re-embed migration progress");
        }
    }

    // 3. Build the snapshot directly. Skip if there's nothing to
    //    write: a fresh install has no MemoryItems, and we'd rather
    //    leave hnsw.idx absent than persist an empty placeholder.
    if migrated > 0 {
        index.rebuild_snapshot()?;
        snapshot::save(&index, 0, &snapshot_path)?;
    }
    Ok(migrated)
}

/// Remove every `wal-*.log` segment. We don't touch other files (e.g.
/// a manual backup the user dropped in there).
fn wipe_wal_dir(wal_dir: &Path) -> Result<()> {
    if !wal_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(wal_dir)
        .map_err(|e| MnemeError::Embedding(format!("read {wal_dir:?}: {e}")))?
    {
        let entry = entry.map_err(|e| MnemeError::Embedding(format!("walk {wal_dir:?}: {e}")))?;
        let p: PathBuf = entry.path();
        if p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("wal-") && n.ends_with(".log"))
        {
            std::fs::remove_file(&p)
                .map_err(|e| MnemeError::Embedding(format!("remove {p:?}: {e}")))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::{MemoryKind, SemanticStore};
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    /// Concrete `Embedder` with a configurable dim — exercises the
    /// "model swap" path without needing two real models.
    struct FixedDimEmbedder(usize);

    #[async_trait::async_trait]
    impl Embedder for FixedDimEmbedder {
        fn dim(&self) -> usize {
            self.0
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            // Deterministic: take the byte length, hash into a vector
            // of `self.0` floats, L2-normalize. Doesn't matter what
            // the values are — we only need shape correctness here.
            let mut v = vec![0.0f32; self.0];
            let seed = (text.len() as f32) + 1.0;
            for (i, slot) in v.iter_mut().enumerate() {
                *slot = (seed * (i as f32 * 0.13 + 0.5)).sin();
            }
            let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            for x in &mut v {
                *x /= n;
            }
            Ok(v)
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
    }

    #[tokio::test]
    async fn first_boot_migrates_and_writes_sidecar() {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        // Seed two memories using a 4-dim embedder (simulates a prior
        // session), going through SemanticStore so the data shape is
        // realistic.
        {
            let prev: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
            let s = SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), prev).unwrap();
            s.remember("alpha", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            s.remember("bravo", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
        }

        // Now boot with an 8-dim embedder. No sidecar yet → migrate.
        let new: Arc<dyn Embedder> = Arc::new(FixedDimEmbedder(8));
        let outcome = migrate_if_needed(tmp.path(), Arc::clone(&storage), new, "fake-8d")
            .await
            .unwrap();
        match outcome {
            Outcome::Migrated { count } => assert_eq!(count, 2),
            other => panic!("expected Migrated, got {other:?}"),
        }

        // Sidecar present + correct.
        let sidecar = tmp.path().join("semantic").join(SIDECAR_FILE);
        let identity = EmbedderIdentity::read(&sidecar).expect("sidecar written");
        assert_eq!(identity.dim, 8);
        assert_eq!(identity.model, "fake-8d");
    }

    #[tokio::test]
    async fn matching_sidecar_is_no_change() {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(FixedDimEmbedder(8));

        // First run: migrates (empty store) and writes sidecar.
        migrate_if_needed(
            tmp.path(),
            Arc::clone(&storage),
            Arc::clone(&embedder),
            "fake-8d",
        )
        .await
        .unwrap();
        // Second run: same identity → NoChange.
        let outcome = migrate_if_needed(tmp.path(), storage, embedder, "fake-8d")
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::NoChange));
    }

    #[tokio::test]
    async fn dim_change_triggers_migration_and_wipes_old_state() {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();

        // Boot 1 — 4-dim, two memories, sidecar gets written.
        {
            let e: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
            let s = SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), Arc::clone(&e))
                .unwrap();
            s.remember("alpha", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            s.remember("bravo", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            migrate_if_needed(tmp.path(), Arc::clone(&storage), e, "stub-4d")
                .await
                .unwrap();
        }
        let semantic_root = tmp.path().join("semantic");
        // The migration on first boot wrote a snapshot.
        assert!(semantic_root.join("hnsw.idx").exists());

        // Stash the WAL contents so we can prove they got wiped.
        let wal_dir = semantic_root.join("wal");
        let pre_wal: Vec<_> = std::fs::read_dir(&wal_dir)
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();

        // Boot 2 — 8-dim. Sidecar mismatches → migrate.
        let e2: Arc<dyn Embedder> = Arc::new(FixedDimEmbedder(8));
        let outcome = migrate_if_needed(tmp.path(), Arc::clone(&storage), e2, "fake-8d")
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Migrated { count: 2 }));

        // Old WAL segments are gone.
        for old in &pre_wal {
            assert!(!old.exists(), "old WAL segment {old:?} survived migration");
        }
        // Fresh snapshot reflects the new dim.
        let (loaded, lsn) = snapshot::load(&semantic_root.join("hnsw.idx")).unwrap();
        assert_eq!(loaded.dim(), 8);
        assert_eq!(lsn, 0);
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn empty_store_writes_sidecar_without_snapshot() {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(FixedDimEmbedder(8));

        let outcome = migrate_if_needed(tmp.path(), storage, embedder, "fake-8d")
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Migrated { count: 0 }));
        assert!(
            !tmp.path().join("semantic").join("hnsw.idx").exists(),
            "no snapshot on empty migration"
        );
        assert!(tmp.path().join("semantic").join(SIDECAR_FILE).exists());
    }
}
