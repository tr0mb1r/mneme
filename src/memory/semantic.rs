//! Semantic memory layer (Phase 3 §7+§8): the layer-4 store the
//! `recall` tool actually queries, with snapshot+delta persistence.
//!
//! Ties three pieces together behind a single owner:
//!
//! * [`crate::storage::Storage`] — durable metadata KV. Each
//!   memory item lives at `b"mem:" + ulid_bytes`; the value is a
//!   postcard-encoded [`MemoryItem`].
//! * [`crate::embed::Embedder`] — turns text into a fixed-dim vector.
//! * [`crate::index::hnsw::HnswIndex`] — in-memory HNSW for nearest-
//!   neighbour search, persisted via a dedicated WAL at
//!   `<root>/semantic/wal/` plus periodic full snapshots at
//!   `<root>/semantic/hnsw.idx`.
//!
//! # Write path
//!
//! `remember(content, ..)` is serialized through a write mutex so two
//! concurrent calls produce a deterministic ordering for the redb +
//! WAL pair (matters for replay determinism and for the snapshot/delta
//! scheme below). Inside the lock:
//!
//! 1. Embed the content (CPU-bound, async via the embedder's worker).
//! 2. Persist the [`MemoryItem`] metadata to [`Storage`] — durable in
//!    the redb-WAL once `put().await` resolves.
//! 3. Append a [`WalOp::VectorInsert`] to the semantic WAL — the
//!    [`HnswApplier`] runs in-line on the WAL writer thread and
//!    mutates the in-memory HNSW under its `RwLock` before the ack
//!    fires.
//!
//! By the time `remember().await` resolves, both the metadata and the
//! vector are durable, and the next `recall()` will see the new
//! memory.
//!
//! # Read path
//!
//! `recall(query, k, filters)`:
//!
//! 1. Embed the query.
//! 2. `index.read().search(query_vec, k * OVERFETCH)` — `OVERFETCH`
//!    leaves headroom for filter rejections without an extra round
//!    trip.
//! 3. For each `(id, score)`, load the [`MemoryItem`] from
//!    [`Storage`]. Missing metadata is logged and skipped — see the
//!    write-path ordering note for when this can happen.
//! 4. Apply scope/kind filters; truncate to `k`.
//!
//! # Forget path
//!
//! `forget(id)` issues a metadata delete and a [`WalOp::VectorDelete`]
//! tombstone. Both are durable before the call returns; the HNSW
//! continues to filter the id out of search results until the next
//! [`HnswIndex::rebuild_snapshot`].
//!
//! # Snapshot scheduler
//!
//! When [`SnapshotConfig::enabled`], a background tokio task wakes
//! periodically (every `interval`) **or** on demand when the running
//! count of unsnapshot-ed inserts crosses `inserts_threshold`. Wake
//! → take `write_lock` → `rebuild_snapshot()` → drop write, take read
//! → [`crate::index::snapshot::save`] → drop read → truncate any
//! WAL segments fully covered by the snapshot's `applied_lsn`.
//!
//! Holding `write_lock` keeps `remember`/`forget` callers from
//! racing with the snapshot, but searches stay live: they take a
//! read lock on `index`, which is compatible with the read lock the
//! scheduler holds during `save`.
//!
//! ## Recovery
//!
//! On startup, [`SemanticStore::open`] tries to load
//! `<root>/semantic/hnsw.idx`. If present and well-formed, the index
//! is seeded from it and the embedded `applied_lsn` becomes the
//! lower bound for WAL replay — so we replay only records past the
//! snapshot. Schema-mismatched or corrupt snapshots fall back to a
//! cold start (full WAL replay) with a clear log line; nothing on
//! disk is auto-deleted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::embed::Embedder;
use crate::ids::MemoryId;
use crate::index::delta::{HnswApplier, replay_into};
use crate::index::hnsw::HnswIndex;
use crate::index::snapshot;
use crate::memory::activity::ActivityCounter;
use crate::storage::Storage;
use crate::storage::wal::{self, WalOp, WalWriter};
use crate::{MnemeError, Result};

/// Key prefix for memory metadata in the KV store. Length kept short
/// so the per-row overhead in redb stays small; the suffix is the
/// raw 16-byte ULID so prefix scans iterate in creation order.
const MEM_KEY_PREFIX: &[u8] = b"mem:";

/// How many extra results to over-fetch from HNSW before applying
/// scope/kind filters. 4× matches `index::hnsw::OVERSHOOT_FACTOR`'s
/// philosophy for tombstones — gives filters headroom to reject up to
/// ~75% of hits without underfilling.
const RECALL_OVERFETCH: usize = 4;

/// Snapshot file name under `<root>/semantic/`. Documented here so the
/// scheduler and the loader can't drift.
const SNAPSHOT_FILE: &str = "hnsw.idx";

/// Memory item types. Matches the `recall`/`remember` tool input
/// schemas verbatim so the agent's JSON value can be parsed straight
/// into this enum without a second mapping step.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Fact,
    Decision,
    Preference,
    Conversation,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Decision => "decision",
            MemoryKind::Preference => "preference",
            MemoryKind::Conversation => "conversation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fact" => Some(MemoryKind::Fact),
            "decision" => Some(MemoryKind::Decision),
            "preference" => Some(MemoryKind::Preference),
            "conversation" => Some(MemoryKind::Conversation),
            _ => None,
        }
    }
}

/// Persisted form of a memory. Stored postcard-encoded under
/// `MEM_KEY_PREFIX || ulid_bytes` in the KV layer. The vector is NOT
/// stored here — it lives in the HNSW (durable via the semantic WAL),
/// so we don't pay for it twice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryItem {
    pub id: MemoryId,
    pub content: String,
    pub kind: MemoryKind,
    pub tags: Vec<String>,
    pub scope: String,
    pub created_at: DateTime<Utc>,
}

/// One result row from [`SemanticStore::recall`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    pub item: MemoryItem,
    /// Cosine distance under the embedder's L2-normalized output:
    /// `0.0` is identical, `2.0` is opposite. Lower is more similar.
    pub score: f32,
}

/// Optional filters applied after the HNSW returns candidates.
#[derive(Debug, Clone, Default)]
pub struct RecallFilters {
    pub scope: Option<String>,
    pub kind: Option<MemoryKind>,
}

/// Patch passed to [`SemanticStore::update`]. Each `Some` field is
/// applied on top of the existing memory; `None` fields are left
/// untouched. `created_at` is never patchable — a memory keeps the
/// timestamp it was first stored under.
#[derive(Debug, Clone, Default)]
pub struct UpdatePatch {
    pub content: Option<String>,
    pub kind: Option<MemoryKind>,
    pub tags: Option<Vec<String>>,
    pub scope: Option<String>,
}

impl UpdatePatch {
    pub fn is_empty(&self) -> bool {
        self.content.is_none() && self.kind.is_none() && self.tags.is_none() && self.scope.is_none()
    }
}

/// Tunables for the snapshot scheduler (Phase 3 §8).
///
/// * `inserts_threshold` — number of `remember`/`forget` ops since the
///   last snapshot before we force a new one, irrespective of clock.
/// * `interval` — time-based ceiling. The scheduler also wakes up at
///   most this often to check whether a snapshot is due.
/// * `enabled` — when `false`, no scheduler task runs and the only
///   snapshots are explicit `SemanticStore::snapshot_now` calls. Tests
///   that don't care about snapshot behaviour use [`Self::disabled`].
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    pub inserts_threshold: u64,
    pub interval: Duration,
    pub enabled: bool,
}

impl SnapshotConfig {
    /// Production defaults (`config::CheckpointsConfig::default`):
    /// 1000 inserts or 60 minutes between snapshots.
    pub fn production() -> Self {
        Self {
            inserts_threshold: 1000,
            interval: Duration::from_secs(60 * 60),
            enabled: true,
        }
    }

    /// Disable the background scheduler entirely. The snapshot file
    /// is still loaded on startup if present, but never rewritten
    /// unless [`SemanticStore::snapshot_now`] is called explicitly.
    pub fn disabled() -> Self {
        Self {
            inserts_threshold: u64::MAX,
            interval: Duration::from_secs(60 * 60),
            enabled: false,
        }
    }

    /// Aggressive thresholds for tests that want to observe a
    /// snapshot fire after a small number of inserts.
    pub fn for_tests(inserts_threshold: u64) -> Self {
        Self {
            inserts_threshold,
            interval: Duration::from_secs(60),
            enabled: true,
        }
    }
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self::production()
    }
}

/// State shared between the SemanticStore and the scheduler task.
/// All fields are `Arc` or atomic so cloning into the spawned task
/// never produces a cycle — the task holds an `Arc<SnapshotState>`
/// only, never `Arc<SemanticStore>`.
struct SnapshotState {
    snapshot_path: PathBuf,
    wal_dir: PathBuf,
    inserts_since: AtomicU64,
    inserts_threshold: u64,
    interval: Duration,
    notify: Notify,
    /// Set by `shutdown()` so the scheduler runs one final snapshot
    /// and exits cleanly.
    shutdown: AtomicBool,
    /// Bumped each time a snapshot completes successfully — handy for
    /// tests that need to wait for the scheduler to react.
    snapshot_count: AtomicU64,
    /// Shared with [`SemanticStore`] and [`HnswApplier`]; `fetch_max`-
    /// style writes from the applier, plain `load(SeqCst)` from the
    /// scheduler.
    applied_lsn: Arc<AtomicU64>,
    /// Same `Arc<RwLock<HnswIndex>>` the SemanticStore + applier
    /// share. Scheduler takes write-lock for `rebuild_snapshot`,
    /// downgrades to read-lock for `save`.
    index: Arc<RwLock<HnswIndex>>,
    /// `tokio::Mutex<()>` shared with `SemanticStore::write_lock` so
    /// the scheduler can serialise itself against `remember`/`forget`.
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

/// The Phase 3 §7 owner. Constructed once at startup by
/// [`crate::cli::run`]; tools hold an `Arc` of it and call its async
/// methods directly.
pub struct SemanticStore {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn Embedder>,
    index: Arc<RwLock<HnswIndex>>,
    applied_lsn: Arc<AtomicU64>,

    // `Drop` on `WalWriter` joins the writer thread and fsyncs the
    // active segment, so `SemanticStore::drop` already gives a clean
    // stop for the WAL. The snapshot scheduler is stopped via
    // `shutdown()` (or, best-effort, our own `Drop`).
    wal: WalWriter,

    // Tokio mutex (not `std::sync::Mutex`) so it can be held across
    // the embedder + storage `await`s without blocking the runtime.
    // Wrapped in `Arc` so the scheduler can take the same lock.
    write_lock: Arc<tokio::sync::Mutex<()>>,

    // `None` when `SnapshotConfig::disabled` was passed in.
    snapshot: Option<Arc<SnapshotState>>,
    scheduler_join: std::sync::Mutex<Option<JoinHandle<()>>>,

    /// Bumped on every user-facing mutation (`remember`, `forget`,
    /// `update`). The L3 consolidation scheduler reads this to gate
    /// its passes — a steady stream of writes suppresses
    /// consolidation until the system goes idle. Internal-only;
    /// callers consume it via [`Self::activity_counter`].
    activity: Arc<ActivityCounter>,
}

impl SemanticStore {
    /// Boot the semantic layer rooted at `<root>/semantic/`.
    ///
    /// `storage` is the same `Arc<dyn Storage>` the rest of the
    /// process uses (typically the redb at `<root>/episodic/`).
    /// `embedder` produces fixed-dim L2-normalized vectors; the HNSW
    /// is sized off `embedder.dim()` exactly once.
    ///
    /// Steps:
    /// 1. Try to load `<root>/semantic/hnsw.idx`. On success the
    ///    in-memory index starts pre-warmed and `applied_lsn` is
    ///    seeded from the snapshot. On failure we log + start cold.
    /// 2. Replay any WAL records past `applied_lsn` into the index.
    /// 3. Open a `WalWriter` at `max_observed_lsn + 1` with an
    ///    [`HnswApplier`] sharing the `applied_lsn` atomic.
    /// 4. If `config.enabled`, spawn a background scheduler task
    ///    that wakes on insert-count or interval and fires a snapshot.
    ///
    /// Construction must happen INSIDE a tokio runtime context
    /// (`#[tokio::main]` / `block_on(...)`) because the scheduler is
    /// spawned via `tokio::spawn`. Tests using `#[tokio::test]` are
    /// fine; sync tests should pass `SnapshotConfig::disabled()`.
    pub fn open(
        root: &Path,
        storage: Arc<dyn Storage>,
        embedder: Arc<dyn Embedder>,
        config: SnapshotConfig,
    ) -> Result<Arc<Self>> {
        let semantic_root = root.join("semantic");
        let wal_dir = semantic_root.join("wal");
        let snapshot_path = semantic_root.join(SNAPSHOT_FILE);
        std::fs::create_dir_all(&wal_dir)?;

        // 1. Try to seed from the on-disk snapshot. A failure here
        // (file missing, bad magic, schema mismatch) is non-fatal —
        // we fall back to a cold start so a single corrupted snapshot
        // doesn't lock users out of their data.
        let (mut idx, mut applied_lsn) = match snapshot::load(&snapshot_path) {
            Ok((loaded, lsn)) => {
                if loaded.dim() != embedder.dim() {
                    tracing::warn!(
                        snapshot_dim = loaded.dim(),
                        embedder_dim = embedder.dim(),
                        path = %snapshot_path.display(),
                        "snapshot dim mismatches embedder; ignoring snapshot and starting cold"
                    );
                    (HnswIndex::new(embedder.dim()), 0u64)
                } else {
                    tracing::info!(
                        applied_lsn = lsn,
                        len = loaded.len(),
                        path = %snapshot_path.display(),
                        "loaded HNSW snapshot"
                    );
                    (loaded, lsn)
                }
            }
            Err(e) if !snapshot_path.exists() => {
                // Missing file is the common case for fresh installs;
                // log at trace, not warn.
                tracing::trace!("no snapshot at {}: {e}", snapshot_path.display());
                (HnswIndex::new(embedder.dim()), 0u64)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %snapshot_path.display(),
                    "failed to load HNSW snapshot; starting cold"
                );
                (HnswIndex::new(embedder.dim()), 0u64)
            }
        };

        // 2. Replay any WAL records past applied_lsn.
        let max_lsn = replay_into(&mut idx, wal::replay(&wal_dir)?, applied_lsn)?;
        applied_lsn = applied_lsn.max(max_lsn);

        // 3. Open the WAL writer with an applier that shares applied_lsn.
        let index = Arc::new(RwLock::new(idx));
        let applied_lsn_atomic = Arc::new(AtomicU64::new(applied_lsn));
        let applier = HnswApplier::new(Arc::clone(&index), Arc::clone(&applied_lsn_atomic));
        let wal_writer = WalWriter::open_with_applier(&wal_dir, max_lsn + 1, Box::new(applier))?;

        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // 4. Maybe spawn the scheduler.
        let (snapshot_state, scheduler_join) = if config.enabled {
            let state = Arc::new(SnapshotState {
                snapshot_path,
                wal_dir,
                inserts_since: AtomicU64::new(0),
                inserts_threshold: config.inserts_threshold,
                interval: config.interval,
                notify: Notify::new(),
                shutdown: AtomicBool::new(false),
                snapshot_count: AtomicU64::new(0),
                applied_lsn: Arc::clone(&applied_lsn_atomic),
                index: Arc::clone(&index),
                write_lock: Arc::clone(&write_lock),
            });
            let task_state = Arc::clone(&state);
            let join = tokio::spawn(async move {
                scheduler_loop(task_state).await;
            });
            (Some(state), std::sync::Mutex::new(Some(join)))
        } else {
            (None, std::sync::Mutex::new(None))
        };

        Ok(Arc::new(Self {
            storage,
            embedder,
            index,
            applied_lsn: applied_lsn_atomic,
            wal: wal_writer,
            write_lock,
            snapshot: snapshot_state,
            scheduler_join,
            activity: ActivityCounter::new(),
        }))
    }

    /// Hand out the shared activity counter. The L3 consolidation
    /// scheduler clones this `Arc` so it can see every `remember` /
    /// `forget` / `update` without re-reading store state.
    pub fn activity_counter(&self) -> Arc<ActivityCounter> {
        Arc::clone(&self.activity)
    }

    /// Convenience for tests: open with the scheduler disabled.
    #[cfg(test)]
    pub(crate) fn open_disabled(
        root: &Path,
        storage: Arc<dyn Storage>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Arc<Self>> {
        Self::open(root, storage, embedder, SnapshotConfig::disabled())
    }

    /// Vector dimension of the underlying embedder. Surfaced for
    /// diagnostics + the `mneme://stats` resource.
    pub fn dim(&self) -> usize {
        self.embedder.dim()
    }

    /// Live (non-tombstoned) memory count in the in-memory HNSW.
    pub fn len(&self) -> usize {
        self.index.read().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current `applied_lsn` — the highest semantic-WAL LSN whose
    /// effect is folded into the in-memory HNSW. Surfaced for the
    /// `mneme://stats` resource and for tests asserting that a
    /// snapshot covered as much WAL ground as we expected.
    pub fn applied_lsn(&self) -> u64 {
        self.applied_lsn.load(Ordering::SeqCst)
    }

    /// Total number of snapshots produced by the scheduler since
    /// boot. Tests `await` until this advances to confirm async
    /// scheduler behaviour.
    #[cfg(test)]
    pub(crate) fn snapshot_count(&self) -> u64 {
        self.snapshot
            .as_ref()
            .map(|s| s.snapshot_count.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Persist a new memory and index its embedding.
    ///
    /// Order matters and is intentional:
    ///
    /// 1. Embed the content (no locks held — we don't want to block
    ///    other readers while a forward pass runs).
    /// 2. Acquire the write lock, append metadata to the KV store,
    ///    append the vector to the semantic WAL.
    ///
    /// If the WAL append fails after the KV write succeeds we
    /// surface the error; the metadata becomes a (temporarily)
    /// orphan record. A future garbage-collection sweep can detect
    /// these by intersecting `scan_prefix(b"mem:")` with the HNSW
    /// member set.
    pub async fn remember(
        &self,
        content: &str,
        kind: MemoryKind,
        tags: Vec<String>,
        scope: String,
    ) -> Result<MemoryId> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(MnemeError::Storage("memory content is empty".into()));
        }

        let vector = self.embedder.embed(trimmed).await?;
        if vector.len() != self.embedder.dim() {
            return Err(MnemeError::Embedding(format!(
                "embedder returned dim {} but advertised {}",
                vector.len(),
                self.embedder.dim()
            )));
        }

        let item = MemoryItem {
            id: MemoryId::new(),
            content: trimmed.to_owned(),
            kind,
            tags,
            scope,
            created_at: Utc::now(),
        };

        let key = mem_key(&item.id);
        let value = postcard::to_allocvec(&item)
            .map_err(|e| MnemeError::Storage(format!("encode MemoryItem: {e}")))?;

        let _g = self.write_lock.lock().await;
        self.storage.put(&key, &value).await?;
        self.wal
            .append(WalOp::VectorInsert {
                id: item.id,
                vec: vector,
            })
            .await?;
        self.note_mutation();
        Ok(item.id)
    }

    /// Top-`k` nearest memories to `query`, optionally filtered.
    ///
    /// Returns an empty `Vec` (not an error) when nothing matches —
    /// `recall` is allowed to be empty by spec.
    pub async fn recall(
        &self,
        query: &str,
        k: usize,
        filters: &RecallFilters,
    ) -> Result<Vec<RecallHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(MnemeError::Embedding("recall query is empty".into()));
        }
        let qvec = self.embedder.embed(trimmed).await?;

        // The HNSW search is sync + cheap; do it inside a `read()`
        // guard scope so we don't hold the lock across the storage
        // awaits below.
        let raw_hits: Vec<(MemoryId, f32)> = {
            let guard = self
                .index
                .read()
                .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
            guard.search(&qvec, k.saturating_mul(RECALL_OVERFETCH))?
        };

        let mut out = Vec::with_capacity(k);
        for (id, score) in raw_hits {
            let key = mem_key(&id);
            let bytes = match self.storage.get(&key).await? {
                Some(b) => b,
                None => {
                    // Vector exists in HNSW but no metadata in KV —
                    // see the write-path ordering note. Skip; future
                    // GC sweep will tombstone the orphan vector.
                    tracing::warn!(memory_id = %id, "recall: orphan vector, no metadata");
                    continue;
                }
            };
            let item: MemoryItem = postcard::from_bytes(&bytes)
                .map_err(|e| MnemeError::Storage(format!("decode MemoryItem {id}: {e}")))?;

            if let Some(want_scope) = &filters.scope
                && &item.scope != want_scope
            {
                continue;
            }
            if let Some(want_kind) = filters.kind
                && item.kind != want_kind
            {
                continue;
            }

            out.push(RecallHit { item, score });
            if out.len() >= k {
                break;
            }
        }
        Ok(out)
    }

    /// Tombstone a memory. Returns `true` if metadata existed
    /// (regardless of whether the HNSW knew about it) and `false`
    /// if neither knew about the id.
    pub async fn forget(&self, id: MemoryId) -> Result<bool> {
        let key = mem_key(&id);
        let existed = self.storage.get(&key).await?.is_some();

        let _g = self.write_lock.lock().await;
        if existed {
            self.storage.delete(&key).await?;
        }
        // Tombstone the vector unconditionally — even if the metadata
        // is gone, leaving an orphan vector wastes RAM and can return
        // `recall` results that mysteriously vanish at decode time.
        self.wal.append(WalOp::VectorDelete { id }).await?;
        self.note_mutation();
        Ok(existed)
    }

    /// Update an existing memory.
    ///
    /// Returns `Ok(true)` if the id existed (and the patch, if non-
    /// empty, was applied); `Ok(false)` if no memory with this id is
    /// stored. An empty patch on an existing id is a successful no-op.
    ///
    /// When `patch.content` is `Some`, the new text is re-embedded and
    /// a single [`WalOp::VectorReplace`] is appended to the semantic
    /// WAL. Metadata-only patches (`kind`/`tags`/`scope`) skip the
    /// embedder entirely — they only rewrite the postcard
    /// `MemoryItem` blob in [`Storage`].
    ///
    /// `created_at` is preserved across updates — it identifies when
    /// the memory was first stored, not when it was last touched.
    pub async fn update(&self, id: MemoryId, patch: UpdatePatch) -> Result<bool> {
        let key = mem_key(&id);
        let bytes = match self.storage.get(&key).await? {
            None => return Ok(false),
            Some(b) => b,
        };
        let mut item: MemoryItem = postcard::from_bytes(&bytes)
            .map_err(|e| MnemeError::Storage(format!("decode MemoryItem {id}: {e}")))?;

        if patch.is_empty() {
            return Ok(true);
        }

        // Re-embed BEFORE acquiring write_lock so we don't block
        // concurrent remember/forget callers on a forward pass — same
        // pattern as `remember`.
        let new_vector = match &patch.content {
            Some(new_content) => {
                let trimmed = new_content.trim();
                if trimmed.is_empty() {
                    return Err(MnemeError::Storage("update content is empty".into()));
                }
                let v = self.embedder.embed(trimmed).await?;
                if v.len() != self.embedder.dim() {
                    return Err(MnemeError::Embedding(format!(
                        "embedder returned dim {} but advertised {}",
                        v.len(),
                        self.embedder.dim()
                    )));
                }
                item.content = trimmed.to_owned();
                Some(v)
            }
            None => None,
        };

        if let Some(k) = patch.kind {
            item.kind = k;
        }
        if let Some(t) = patch.tags {
            item.tags = t;
        }
        if let Some(s) = patch.scope {
            item.scope = s;
        }

        let value = postcard::to_allocvec(&item)
            .map_err(|e| MnemeError::Storage(format!("encode MemoryItem: {e}")))?;

        let _g = self.write_lock.lock().await;
        self.storage.put(&key, &value).await?;
        if let Some(vec) = new_vector {
            self.wal.append(WalOp::VectorReplace { id, vec }).await?;
            self.note_mutation();
        }
        Ok(true)
    }

    /// Lookup a single memory by id. Used by tools/inspect paths;
    /// short-circuits the HNSW entirely.
    pub async fn get(&self, id: MemoryId) -> Result<Option<MemoryItem>> {
        let key = mem_key(&id);
        match self.storage.get(&key).await? {
            None => Ok(None),
            Some(bytes) => Ok(Some(postcard::from_bytes(&bytes).map_err(|e| {
                MnemeError::Storage(format!("decode MemoryItem {id}: {e}"))
            })?)),
        }
    }

    /// Force a snapshot regardless of insert count or interval.
    /// Useful for `mneme stop` and shutdown paths that want a clean
    /// disk state. Idempotent and safe to call concurrently with
    /// `remember`/`forget` — they'll serialise on `write_lock`.
    pub async fn snapshot_now(&self) -> Result<()> {
        let Some(state) = &self.snapshot else {
            // No scheduler configured — assemble the same handles
            // ad-hoc so callers get a deterministic snapshot anyway.
            let semantic_root = match self.snapshot_root_fallback() {
                Some(p) => p,
                None => return Ok(()),
            };
            let snapshot_path = semantic_root.join(SNAPSHOT_FILE);
            let wal_dir = semantic_root.join("wal");
            return run_snapshot_inline(
                &self.write_lock,
                &self.index,
                &self.applied_lsn,
                &snapshot_path,
                &wal_dir,
            )
            .await;
        };
        run_snapshot(state).await
    }

    /// When `SnapshotConfig::disabled` is in play we don't keep the
    /// derived paths around, so `snapshot_now` has nothing to write
    /// to. Tests that need an explicit snapshot enable the scheduler.
    fn snapshot_root_fallback(&self) -> Option<PathBuf> {
        // Disabled-mode `snapshot_now` is currently a no-op by design.
        // Returning `None` makes that explicit; callers see Ok(()) and
        // move on. If we ever need disabled-mode explicit snapshots,
        // we'll plumb the path through `SemanticStore` directly.
        None
    }

    /// Stop the scheduler gracefully and wait for any in-flight
    /// snapshot to fsync. Best practice in production shutdown
    /// paths so we don't leave the next boot doing a full WAL
    /// replay just because the scheduler hadn't ticked yet.
    pub async fn shutdown(&self) -> Result<()> {
        let Some(state) = &self.snapshot else {
            return Ok(());
        };
        state.shutdown.store(true, Ordering::SeqCst);
        state.notify.notify_one();

        let join = self
            .scheduler_join
            .lock()
            .map_err(|e| MnemeError::Index(format!("scheduler join mutex poisoned: {e}")))?
            .take();
        if let Some(j) = join
            && let Err(e) = j.await
        {
            tracing::warn!(error = %e, "scheduler task panicked during shutdown");
        }
        Ok(())
    }

    fn note_mutation(&self) {
        // Tell the snapshot scheduler we wrote.
        if let Some(state) = &self.snapshot {
            let new = state.inserts_since.fetch_add(1, Ordering::SeqCst) + 1;
            if new >= state.inserts_threshold {
                state.notify.notify_one();
            }
        }
        // Tell the consolidation scheduler the system is busy.
        self.activity.bump();
    }
}

impl Drop for SemanticStore {
    fn drop(&mut self) {
        // Best-effort: signal the scheduler to exit. We can't `await`
        // here, so the task may still be running briefly after Drop
        // returns — production code should call `shutdown().await`
        // explicitly for a deterministic stop.
        if let Some(state) = &self.snapshot {
            state.shutdown.store(true, Ordering::SeqCst);
            state.notify.notify_one();
        }
    }
}

// ---------- Scheduler ----------

async fn scheduler_loop(state: Arc<SnapshotState>) {
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

async fn run_snapshot(state: &SnapshotState) -> Result<()> {
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

async fn run_snapshot_inline(
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

fn mem_key(id: &MemoryId) -> Vec<u8> {
    let mut k = Vec::with_capacity(MEM_KEY_PREFIX.len() + 16);
    k.extend_from_slice(MEM_KEY_PREFIX);
    // ULID is 128 bits; serialise as the 16-byte big-endian form so
    // lexical key order matches creation-time order.
    k.extend_from_slice(&id.0.to_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::stub::StubEmbedder;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn store_with_stub(root: &Path) -> Arc<SemanticStore> {
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        SemanticStore::open_disabled(root, storage, embedder).unwrap()
    }

    #[tokio::test]
    async fn remember_then_recall_finds_self() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());

        let id = s
            .remember(
                "alpha bravo charlie",
                MemoryKind::Fact,
                vec!["t1".into()],
                "personal".into(),
            )
            .await
            .unwrap();

        let hits = s
            .recall("alpha bravo charlie", 5, &RecallFilters::default())
            .await
            .unwrap();
        assert!(!hits.is_empty(), "self-recall returned nothing");
        assert_eq!(hits[0].item.id, id);
        assert!(
            hits[0].score < 0.001,
            "self-distance ~0, got {}",
            hits[0].score
        );
        assert_eq!(hits[0].item.content, "alpha bravo charlie");
        assert_eq!(hits[0].item.kind, MemoryKind::Fact);
        assert_eq!(hits[0].item.tags, vec!["t1".to_string()]);
    }

    #[tokio::test]
    async fn empty_content_rejected() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let err = s
            .remember("   ", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap_err();
        assert!(matches!(err, MnemeError::Storage(_)));
    }

    #[tokio::test]
    async fn empty_query_rejected() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let err = s
            .recall("", 5, &RecallFilters::default())
            .await
            .unwrap_err();
        assert!(matches!(err, MnemeError::Embedding(_)));
    }

    #[tokio::test]
    async fn recall_filters_by_scope() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let work_id = s
            .remember("topic", MemoryKind::Fact, vec![], "work".into())
            .await
            .unwrap();
        let _personal_id = s
            .remember("topic", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();

        let hits = s
            .recall(
                "topic",
                10,
                &RecallFilters {
                    scope: Some("work".into()),
                    kind: None,
                },
            )
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.item.scope == "work"));
        assert!(hits.iter().any(|h| h.item.id == work_id));
    }

    #[tokio::test]
    async fn recall_filters_by_kind() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let _f = s
            .remember("topic A", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let d = s
            .remember("topic B", MemoryKind::Decision, vec![], "personal".into())
            .await
            .unwrap();

        let hits = s
            .recall(
                "topic",
                10,
                &RecallFilters {
                    scope: None,
                    kind: Some(MemoryKind::Decision),
                },
            )
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.item.kind == MemoryKind::Decision));
        assert!(hits.iter().any(|h| h.item.id == d));
    }

    #[tokio::test]
    async fn forget_removes_from_recall() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let id = s
            .remember("ephemeral", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();

        let before = s
            .recall("ephemeral", 5, &RecallFilters::default())
            .await
            .unwrap();
        assert!(before.iter().any(|h| h.item.id == id));

        let existed = s.forget(id).await.unwrap();
        assert!(existed);

        let after = s
            .recall("ephemeral", 5, &RecallFilters::default())
            .await
            .unwrap();
        assert!(after.iter().all(|h| h.item.id != id));
        assert!(s.get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_unknown_id_returns_false() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let stranger = MemoryId::new();
        assert!(!s.forget(stranger).await.unwrap());
    }

    #[tokio::test]
    async fn recall_with_zero_k_is_empty() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let _ = s
            .remember("anything", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let hits = s
            .recall("anything", 0, &RecallFilters::default())
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn wal_replay_restores_index_across_reopen() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));

        // First boot: write three memories.
        let mut written: Vec<MemoryId> = Vec::new();
        {
            let s = SemanticStore::open_disabled(
                tmp.path(),
                Arc::clone(&storage) as _,
                Arc::clone(&embedder),
            )
            .unwrap();
            for content in ["one", "two", "three"] {
                let id = s
                    .remember(content, MemoryKind::Fact, vec![], "personal".into())
                    .await
                    .unwrap();
                written.push(id);
            }
            // Drop closes the WAL writer thread + fsyncs the segment.
        }

        // Second boot: replay should rebuild the in-memory index and
        // recall must return the previously-written memories. Reuse
        // the same MemoryStorage so metadata survives the second
        // `SemanticStore::open` (real boots reuse the same redb).
        let s2 = SemanticStore::open_disabled(
            tmp.path(),
            Arc::clone(&storage) as _,
            Arc::clone(&embedder),
        )
        .unwrap();
        assert_eq!(s2.len(), 3, "replay should restore three vectors");
        let hits = s2
            .recall("one", 5, &RecallFilters::default())
            .await
            .unwrap();
        let returned_ids: std::collections::HashSet<_> = hits.iter().map(|h| h.item.id).collect();
        assert!(returned_ids.contains(&written[0]));
    }

    // ---------- Update tests ----------

    #[tokio::test]
    async fn update_unknown_id_returns_false() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let stranger = MemoryId::new();
        let ok = s
            .update(
                stranger,
                UpdatePatch {
                    content: Some("hi".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn update_empty_patch_is_noop_but_returns_true() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let id = s
            .remember("x", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let pre = s.get(id).await.unwrap().unwrap();
        let ok = s.update(id, UpdatePatch::default()).await.unwrap();
        assert!(ok);
        let post = s.get(id).await.unwrap().unwrap();
        assert_eq!(pre, post);
    }

    #[tokio::test]
    async fn update_metadata_only_skips_embedder() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let id = s
            .remember(
                "policy on PRs",
                MemoryKind::Fact,
                vec!["old-tag".into()],
                "personal".into(),
            )
            .await
            .unwrap();
        let lsn_before = s.applied_lsn();

        let ok = s
            .update(
                id,
                UpdatePatch {
                    kind: Some(MemoryKind::Decision),
                    tags: Some(vec!["new-tag".into()]),
                    scope: Some("work".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(ok);

        let item = s.get(id).await.unwrap().unwrap();
        assert_eq!(item.kind, MemoryKind::Decision);
        assert_eq!(item.tags, vec!["new-tag".to_string()]);
        assert_eq!(item.scope, "work");
        assert_eq!(item.content, "policy on PRs", "content untouched");
        assert_eq!(
            s.applied_lsn(),
            lsn_before,
            "metadata-only update must NOT append to the semantic WAL"
        );
    }

    #[tokio::test]
    async fn update_content_re_embeds_and_recall_finds_new_text() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let id = s
            .remember(
                "stale content alpha",
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        let pre = s.get(id).await.unwrap().unwrap();

        let ok = s
            .update(
                id,
                UpdatePatch {
                    content: Some("fresh content omega".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(ok);

        let post = s.get(id).await.unwrap().unwrap();
        assert_eq!(post.content, "fresh content omega");
        assert_eq!(post.created_at, pre.created_at, "created_at preserved");
        // Live count must NOT have grown — replace is in-place.
        assert_eq!(s.len(), 1);

        // Querying near the new text must surface this id at distance ~0.
        let hits = s
            .recall("fresh content omega", 5, &RecallFilters::default())
            .await
            .unwrap();
        assert!(hits.iter().any(|h| h.item.id == id));
        let top = hits.iter().find(|h| h.item.id == id).unwrap();
        assert!(top.score < 0.001, "self-distance ~0, got {}", top.score);
        assert_eq!(top.item.content, "fresh content omega");
    }

    #[tokio::test]
    async fn update_empty_content_rejected() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let id = s
            .remember("hi", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let err = s
            .update(
                id,
                UpdatePatch {
                    content: Some("   ".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MnemeError::Storage(_)));
    }

    #[tokio::test]
    async fn update_survives_restart() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));

        let id = {
            let s = SemanticStore::open_disabled(
                tmp.path(),
                Arc::clone(&storage) as _,
                Arc::clone(&embedder),
            )
            .unwrap();
            let id = s
                .remember(
                    "original alpha",
                    MemoryKind::Fact,
                    vec![],
                    "personal".into(),
                )
                .await
                .unwrap();
            s.update(
                id,
                UpdatePatch {
                    content: Some("rewritten omega".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            id
        };

        // Reopen — replay should rebuild HNSW so recall finds the new
        // text and not the original.
        let s2 = SemanticStore::open_disabled(
            tmp.path(),
            Arc::clone(&storage) as _,
            Arc::clone(&embedder),
        )
        .unwrap();
        assert_eq!(s2.len(), 1);
        let hits = s2
            .recall("rewritten omega", 5, &RecallFilters::default())
            .await
            .unwrap();
        let hit = hits.iter().find(|h| h.item.id == id).expect("post-update");
        assert!(hit.score < 0.001, "post-replay self-distance ~0");
        assert_eq!(hit.item.content, "rewritten omega");
    }

    // ---------- Snapshot scheduler tests ----------

    fn embedder_4d() -> Arc<dyn Embedder> {
        Arc::new(StubEmbedder::with_dim(4))
    }

    /// Wait until `predicate(store)` is true or `timeout` elapses.
    /// Better than a hard `sleep` because it actually proves the
    /// scheduler made progress instead of guessing at timing.
    async fn await_until<F: Fn(&SemanticStore) -> bool>(
        s: &SemanticStore,
        predicate: F,
        timeout: Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if predicate(s) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        predicate(s)
    }

    #[tokio::test]
    async fn snapshot_fires_after_insert_threshold() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let s = SemanticStore::open(
            tmp.path(),
            storage,
            embedder_4d(),
            SnapshotConfig::for_tests(3),
        )
        .unwrap();

        for i in 0..3 {
            s.remember(
                &format!("memory-{i}"),
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        }

        // Scheduler should fire a snapshot off the insert-count
        // pressure within a couple ticks.
        let fired = await_until(&s, |s| s.snapshot_count() >= 1, Duration::from_secs(5)).await;
        assert!(fired, "expected scheduler to produce ≥1 snapshot");
        assert!(
            tmp.path().join("semantic").join(SNAPSHOT_FILE).exists(),
            "hnsw.idx should exist after scheduled snapshot"
        );
    }

    #[tokio::test]
    async fn shutdown_produces_final_snapshot_and_truncates_wal() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let s = SemanticStore::open(
            tmp.path(),
            storage,
            embedder_4d(),
            // Threshold high enough that only `shutdown()` triggers a snapshot.
            SnapshotConfig::for_tests(1_000),
        )
        .unwrap();

        for i in 0..5 {
            s.remember(
                &format!("m-{i}"),
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        }
        let lsn_before = s.applied_lsn();
        assert!(lsn_before >= 5);

        s.shutdown().await.unwrap();

        let snapshot_path = tmp.path().join("semantic").join(SNAPSHOT_FILE);
        assert!(
            snapshot_path.exists(),
            "shutdown() must produce a final snapshot"
        );
        // Confirm the snapshot stored the LSN we expected.
        let (_idx, lsn) = snapshot::load(&snapshot_path).unwrap();
        assert_eq!(lsn, lsn_before, "snapshot LSN should match applied_lsn");
    }

    #[tokio::test]
    async fn restart_skips_wal_records_covered_by_snapshot() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let embedder = embedder_4d();

        let id1;
        {
            let s = SemanticStore::open(
                tmp.path(),
                Arc::clone(&storage) as Arc<dyn Storage>,
                Arc::clone(&embedder),
                SnapshotConfig::for_tests(2),
            )
            .unwrap();
            id1 = s
                .remember("one", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            let _id2 = s
                .remember("two", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            // Wait for the count-triggered snapshot.
            assert!(await_until(&s, |s| s.snapshot_count() >= 1, Duration::from_secs(5)).await);
            // Add post-snapshot writes — those must replay on restart.
            let _id3 = s
                .remember("three", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            s.shutdown().await.unwrap();
        }

        // Reopen — the second boot should load the snapshot and only
        // replay the post-snapshot records, but the in-memory state
        // must be identical to a full-replay boot.
        let s2 = SemanticStore::open(
            tmp.path(),
            Arc::clone(&storage) as Arc<dyn Storage>,
            Arc::clone(&embedder),
            SnapshotConfig::disabled(),
        )
        .unwrap();
        assert_eq!(s2.len(), 3, "all three memories must survive restart");
        let hits = s2
            .recall("one", 5, &RecallFilters::default())
            .await
            .unwrap();
        assert!(hits.iter().any(|h| h.item.id == id1));
    }

    #[tokio::test]
    async fn corrupt_snapshot_falls_back_to_cold_start() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();
        let embedder = embedder_4d();

        // Pre-create a corrupt snapshot file.
        let semantic_dir = tmp.path().join("semantic");
        std::fs::create_dir_all(&semantic_dir).unwrap();
        std::fs::write(
            semantic_dir.join(SNAPSHOT_FILE),
            b"this is not a valid snapshot",
        )
        .unwrap();

        let s = SemanticStore::open(
            tmp.path(),
            Arc::clone(&storage) as Arc<dyn Storage>,
            embedder,
            SnapshotConfig::disabled(),
        )
        .unwrap();
        // Cold start → empty index. No panic, no error.
        assert_eq!(s.len(), 0);
        assert_eq!(s.applied_lsn(), 0);
    }

    #[tokio::test]
    async fn snapshot_dim_mismatch_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::new();

        // Step 1: write a snapshot at dim=4.
        {
            let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
            let s = SemanticStore::open(
                tmp.path(),
                Arc::clone(&storage) as Arc<dyn Storage>,
                embedder,
                SnapshotConfig::for_tests(1),
            )
            .unwrap();
            s.remember("x", MemoryKind::Fact, vec![], "personal".into())
                .await
                .unwrap();
            assert!(await_until(&s, |s| s.snapshot_count() >= 1, Duration::from_secs(5)).await);
            s.shutdown().await.unwrap();
        }

        // Step 2: reopen with a different embedder dim. Snapshot must
        // be rejected (dim mismatch) and we must NOT crash. The WAL
        // is still dim=4, so the dim-8 boot can't replay it either —
        // nuke the WAL to simulate a fresh start at the new dim. In
        // production, mneme would do a full re-embed migration here.
        let wal_dir = tmp.path().join("semantic").join("wal");
        for entry in std::fs::read_dir(&wal_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let mismatched: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(8));
        let s2 = SemanticStore::open(
            tmp.path(),
            Arc::clone(&storage) as Arc<dyn Storage>,
            mismatched,
            SnapshotConfig::disabled(),
        )
        .unwrap();
        assert_eq!(s2.len(), 0);
        assert_eq!(s2.dim(), 8);
    }
}
