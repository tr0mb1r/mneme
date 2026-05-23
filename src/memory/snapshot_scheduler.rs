use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::index::hnsw::HnswIndex;
use crate::index::snapshot;
use crate::storage::wal;
use crate::{MnemeError, Result};

/// State shared between the SemanticStore and the scheduler task.
/// All fields are `Arc` or atomic so cloning into the spawned task
/// never produces a cycle — the task holds an `Arc<SnapshotState>`
/// only, never `Arc<SemanticStore>`.
pub(crate) struct SnapshotState {
    pub(crate) snapshot_path: PathBuf,
    pub(crate) wal_dir: PathBuf,
    pub(crate) inserts_since: AtomicU64,
    pub(crate) inserts_threshold: u64,
    pub(crate) interval: Duration,
    pub(crate) notify: Notify,
    /// Set by `shutdown()` so the scheduler runs one final snapshot
    /// and exits cleanly.
    pub(crate) shutdown: AtomicBool,
    /// Bumped each time a snapshot completes successfully — handy for
    /// tests that need to wait for the scheduler to react.
    pub(crate) snapshot_count: AtomicU64,
    /// Shared with [`SemanticStore`] and [`HnswApplier`]; `fetch_max`-
    /// style writes from the applier, plain `load(SeqCst)` from the
    /// scheduler.
    pub(crate) applied_lsn: Arc<AtomicU64>,
    /// Same `Arc<RwLock<HnswIndex>>` the SemanticStore + applier
    /// share. Scheduler takes write-lock for `rebuild_snapshot`,
    /// downgrades to read-lock for `save`.
    pub(crate) index: Arc<RwLock<HnswIndex>>,
    /// `tokio::Mutex<()>` shared with `SemanticStore::write_lock` so
    /// the scheduler can serialise itself against `remember`/`forget`.
    pub(crate) write_lock: Arc<tokio::sync::Mutex<()>>,
}

// ---------- Scheduler ----------

pub(crate) async fn scheduler_loop(state: Arc<SnapshotState>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(state.interval) => {}
            _ = state.notify.notified() => {}
        }

        let stopping = state.shutdown.load(Ordering::SeqCst);
        let due_by_count = state.inserts_since.load(Ordering::SeqCst) >= state.inserts_threshold;

        // On shutdown we always force a final snapshot if there are
        // pending inserts — that's the whole point of the explicit
        // `shutdown()` API.
        let pending = state.inserts_since.load(Ordering::SeqCst) > 0;
        if stopping {
            if pending && let Err(e) = run_snapshot(&state).await {
                tracing::warn!(error = %e, "final snapshot on shutdown failed");
            }
            return;
        }

        if due_by_count && let Err(e) = run_snapshot(&state).await {
            tracing::warn!(error = %e, "scheduled snapshot failed; will retry next tick");
        }
        // Time-based wakeups without count pressure are intentional
        // no-ops — the scheduler exists to bound the worst-case gap
        // between snapshots, not to write empty ones.
    }
}

pub(crate) async fn run_snapshot(state: &SnapshotState) -> Result<()> {
    run_snapshot_inline(
        &state.write_lock,
        &state.index,
        &state.applied_lsn,
        &state.snapshot_path,
        &state.wal_dir,
    )
    .await?;
    state.inserts_since.store(0, Ordering::SeqCst);
    state.snapshot_count.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

pub(crate) async fn run_snapshot_inline(
    write_lock: &tokio::sync::Mutex<()>,
    index: &Arc<RwLock<HnswIndex>>,
    applied_lsn: &Arc<AtomicU64>,
    snapshot_path: &Path,
    wal_dir: &Path,
) -> Result<()> {
    let _g = write_lock.lock().await;

    // 1. Rebuild the HNSW so the snapshot has a clean committed
    //    structure (no pending buffer, no tombstones).
    let lsn = {
        let mut idx = index
            .write()
            .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
        idx.rebuild_snapshot()?;
        // Capture applied_lsn under the write lock — the WAL applier
        // can't be running concurrently because we hold write_lock,
        // so no record can land between this read and the save below.
        applied_lsn.load(Ordering::SeqCst)
    };

    // 2. Save under a read lock — searches stay live, but new
    //    remember()/forget() callers stay queued on `write_lock`.
    {
        let idx = index
            .read()
            .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
        snapshot::save(&idx, lsn, snapshot_path)?;
    }

    // 3. Truncate fully-covered WAL segments. After save() returns
    //    successfully the snapshot is durable, so the records folded
    //    into it can be reclaimed.
    let _removed = wal::truncate_through(wal_dir, lsn)?;
    Ok(())
}
