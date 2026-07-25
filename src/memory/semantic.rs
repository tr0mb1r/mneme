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
//! 2. `index.read().search(query_vec, k * RECALL_OVERFETCH)` — the
//!    over-fetch leaves headroom for filter rejections without an
//!    extra round trip.
//! 3. For each `(id, score)`, load the [`MemoryItem`] from
//!    [`Storage`]. Missing metadata is logged and skipped — see the
//!    write-path ordering note for when this can happen.
//! 4. Apply the [`RecallFilters`] predicates (scope, kind, tags,
//!    similarity floor).
//! 5. If fewer than `k` candidates survived and the index still has
//!    unseen vectors, widen the probe and repeat from step 2 over the
//!    newly-revealed suffix. Without this, a selective filter
//!    silently returns a short list; see [`SemanticStore::recall`].
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

use super::snapshot_scheduler;

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
use crate::storage::MEM_KEY_PREFIX;
use crate::storage::Storage;
use crate::storage::wal::{self, WalOp, WalWriter};
use crate::{MnemeError, Result};

/// How many extra results to over-fetch from HNSW on the *first*
/// filtered-recall probe. 4× matches `index::hnsw::OVERSHOOT_FACTOR`'s
/// philosophy for tombstones — it lets filters reject up to ~75% of
/// hits without a second probe.
///
/// It is only the starting width: [`SemanticStore::recall`] widens
/// geometrically (see [`RECALL_WIDEN_FACTOR`]) until it has `k`
/// survivors or the index is exhausted. A fixed 4× would silently
/// underfill whenever the requested scope / kind / tag set is a
/// minority of the corpus — asking for 10 `work` memories out of a
/// 10 000-memory corpus that is 1 % `work` would return ~0.
const RECALL_OVERFETCH: usize = 4;

/// Growth factor applied to the HNSW fetch width on each successive
/// probe when filters have rejected too much to fill `k`. Geometric
/// so the worst case is O(log(corpus/k)) probes rather than O(corpus/k).
const RECALL_WIDEN_FACTOR: usize = 4;

/// Snapshot file name under `<root>/semantic/`. Documented here so the
/// scheduler and the loader can't drift.
pub const SNAPSHOT_FILE: &str = "hnsw.idx";

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
///
/// Every field is a *narrowing* predicate: a candidate must satisfy
/// all of the populated ones to survive. [`SemanticStore::recall`]
/// compensates for the narrowing by widening its HNSW fetch until it
/// has enough survivors, so a selective filter costs extra probes
/// rather than silently returning a short list.
#[derive(Debug, Clone, Default)]
pub struct RecallFilters {
    pub scope: Option<String>,
    pub kind: Option<MemoryKind>,
    /// Every tag listed here must be present on the memory (AND, not
    /// OR). Empty means "no tag constraint". Matching is exact and
    /// case-sensitive, consistent with how `remember` stores them.
    pub tags: Vec<String>,
    /// Cosine-similarity floor in `[-1.0, 1.0]`, where `1.0` is
    /// identical. Hits scoring below it are dropped. `None` keeps
    /// every hit the index returns — which is the pre-v1.3 behaviour
    /// and means a query with no good match still yields the `k`
    /// nearest vectors, however far away they are.
    ///
    /// Relates to [`RecallHit::score`] (a cosine *distance*) as
    /// `similarity = 1.0 - score`.
    pub min_similarity: Option<f32>,
}

impl RecallFilters {
    /// Convenience for the common "just filter by scope" case.
    pub fn with_scope(scope: impl Into<String>) -> Self {
        Self {
            scope: Some(scope.into()),
            ..Default::default()
        }
    }

    /// Apply every populated predicate to one candidate.
    fn matches(&self, item: &MemoryItem, score: f32) -> bool {
        if let Some(want_scope) = &self.scope
            && &item.scope != want_scope
        {
            return false;
        }
        if let Some(want_kind) = self.kind
            && item.kind != want_kind
        {
            return false;
        }
        if !self
            .tags
            .iter()
            .all(|want| item.tags.iter().any(|have| have == want))
        {
            return false;
        }
        if let Some(floor) = self.min_similarity
            && similarity_from_distance(score) < floor
        {
            return false;
        }
        true
    }
}

/// Convert the cosine *distance* the index reports into the cosine
/// *similarity* agents reason about. Distance is `1 - cos θ` over
/// L2-normalized vectors, so this is just the inverse.
pub fn similarity_from_distance(distance: f32) -> f32 {
    1.0 - distance
}

/// Cosine-similarity floor at which a new memory is reported as a
/// near-duplicate of an existing one.
///
/// Deliberately strict. The report is advisory — it never blocks a
/// write — but a false positive teaches the agent to distrust it, and
/// under BGE-M3 genuinely distinct facts about the same subject
/// routinely reach 0.85-0.90. 0.95 keeps the signal to restatements
/// and near-verbatim repeats.
pub const NEAR_DUPLICATE_SIMILARITY: f32 = 0.95;

/// An existing memory that a pending write closely restates. Produced
/// by [`SemanticStore::remember_checked`].
#[derive(Debug, Clone, PartialEq)]
pub struct NearDuplicate {
    pub id: MemoryId,
    /// Cosine similarity to the incoming content, `1.0` = identical.
    pub similarity: f32,
    /// The existing memory's content, so the caller can show the agent
    /// what it is about to duplicate without a second round trip.
    pub content: String,
    /// The existing memory's scope, which may differ from the incoming
    /// write's.
    pub scope: String,
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
    snapshot: Option<Arc<snapshot_scheduler::SnapshotState>>,
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
        Self::open_with_crypto(root, storage, embedder, config, None)
    }

    /// Variant of [`open`] that takes an optional AEAD codec so the
    /// HNSW snapshot file and the semantic WAL are encrypted at rest
    /// (ADR-0013 P5d / P4 extended to semantic/wal). `None` is the
    /// legacy plaintext path.
    pub fn open_with_crypto(
        root: &Path,
        storage: Arc<dyn Storage>,
        embedder: Arc<dyn Embedder>,
        config: SnapshotConfig,
        aead: Option<Arc<crate::crypto::Aead>>,
    ) -> Result<Arc<Self>> {
        let semantic_root = root.join("semantic");
        let wal_dir = semantic_root.join("wal");
        let snapshot_path = semantic_root.join(SNAPSHOT_FILE);
        std::fs::create_dir_all(&wal_dir)?;

        // 1. Try to seed from the on-disk snapshot. A failure here
        // (file missing, bad magic, schema mismatch) is non-fatal —
        // we fall back to a cold start so a single corrupted snapshot
        // doesn't lock users out of their data.
        let (mut idx, mut applied_lsn) =
            match snapshot::load_with_crypto(&snapshot_path, aead.as_deref()) {
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

        // 2. Replay any WAL records past applied_lsn. Use encrypted
        // replay when an AEAD codec is provided.
        let replay_iter = match aead.as_ref() {
            Some(a) => wal::replay_encrypted(&wal_dir, Arc::clone(a))?,
            None => wal::replay(&wal_dir)?,
        };
        let max_lsn = replay_into(&mut idx, replay_iter, applied_lsn)?;
        applied_lsn = applied_lsn.max(max_lsn);

        // 3. Open the WAL writer with an applier that shares applied_lsn.
        let index = Arc::new(RwLock::new(idx));
        let applied_lsn_atomic = Arc::new(AtomicU64::new(applied_lsn));
        let applier = HnswApplier::new(Arc::clone(&index), Arc::clone(&applied_lsn_atomic));
        let wal_writer = match aead.as_ref() {
            Some(a) => WalWriter::open_with_applier_encrypted(
                &wal_dir,
                max_lsn + 1,
                Box::new(applier),
                Arc::clone(a),
            )?,
            None => WalWriter::open_with_applier(&wal_dir, max_lsn + 1, Box::new(applier))?,
        };

        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // 4. Maybe spawn the scheduler.
        let (snapshot_state, scheduler_join) = if config.enabled {
            let state = Arc::new(snapshot_scheduler::SnapshotState {
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
                aead: aead.clone(),
            });
            let task_state = Arc::clone(&state);
            let join = tokio::spawn(async move {
                snapshot_scheduler::scheduler_loop(task_state).await;
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
        self.remember_checked(content, kind, tags, scope, false)
            .await
            .map(|(id, _)| id)
    }

    /// [`SemanticStore::remember`], optionally reporting whether the
    /// new content is a near-duplicate of something already stored.
    ///
    /// L4 has no lifecycle pass — nothing dedupes, decays, or compacts
    /// it — so an agent that re-states the same fact every session
    /// grows the corpus without bound and dilutes recall precision.
    /// This is the cheap half of the fix: tell the caller, let it
    /// decide. The memory is stored either way; the verbatim principle
    /// means mneme never silently drops or rewrites what it was given.
    ///
    /// The check costs one index search and **no extra embedding** —
    /// it reuses the vector computed for the write, which matters
    /// because a second BGE-M3 forward pass would roughly double the
    /// 150 ms p95 `remember` budget.
    pub async fn remember_checked(
        &self,
        content: &str,
        kind: MemoryKind,
        tags: Vec<String>,
        scope: String,
        check_duplicates: bool,
    ) -> Result<(MemoryId, Option<NearDuplicate>)> {
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

        // Probe before inserting, so the new row cannot match itself.
        let duplicate = if check_duplicates {
            self.nearest_duplicate(&vector).await?
        } else {
            None
        };

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
        Ok((item.id, duplicate))
    }

    /// Top-1 index probe for [`remember_checked`]. Returns `Some` only
    /// when the nearest existing memory clears
    /// [`NEAR_DUPLICATE_SIMILARITY`].
    ///
    /// Deliberately does **not** filter by scope: the same fact stored
    /// under two scopes is still worth flagging, and the caller has the
    /// scope of both rows to decide.
    async fn nearest_duplicate(&self, vector: &[f32]) -> Result<Option<NearDuplicate>> {
        let hits = {
            let guard = self
                .index
                .read()
                .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
            guard.search(vector, 1)?
        };
        let Some((id, distance)) = hits.first().copied() else {
            return Ok(None);
        };
        let similarity = similarity_from_distance(distance);
        if similarity < NEAR_DUPLICATE_SIMILARITY {
            return Ok(None);
        }
        // An orphan vector (metadata already deleted) is not a
        // duplicate of anything the agent can act on.
        let Some(bytes) = self.storage.get(&mem_key(&id)).await? else {
            return Ok(None);
        };
        let existing: MemoryItem = postcard::from_bytes(&bytes)
            .map_err(|e| MnemeError::Storage(format!("decode MemoryItem {id}: {e}")))?;
        Ok(Some(NearDuplicate {
            id,
            similarity,
            content: existing.content,
            scope: existing.scope,
        }))
    }

    /// Top-`k` nearest memories to `query`, optionally filtered.
    ///
    /// Returns an empty `Vec` (not an error) when nothing matches —
    /// `recall` is allowed to be empty by spec.
    ///
    /// # Filter compensation
    ///
    /// Filters in [`RecallFilters`] are applied *after* the vector
    /// search, because the HNSW indexes vectors only — it has no
    /// notion of scope, kind, or tags. A single fixed-width probe
    /// therefore underfills whenever the filter is selective: the
    /// nearest `k · 4` vectors may contain zero `work`-scoped rows
    /// even when the corpus holds hundreds.
    ///
    /// So we widen instead. Each probe asks the index for more
    /// candidates than the last (geometric, [`RECALL_WIDEN_FACTOR`])
    /// and only the newly-revealed suffix is decoded — [`HnswIndex::search`]
    /// returns a distance-ordered prefix, so a wider probe is a
    /// superset whose leading entries are unchanged. The loop stops on
    /// the first of: `k` survivors found, the index returned fewer
    /// candidates than we asked for (exhausted), or we have already
    /// asked for every live vector.
    ///
    /// Cost: unfiltered recall is exactly one probe, as before. A
    /// filtered recall that matches nothing degrades to a full index
    /// walk — correct, and bounded by `O(log(corpus / k))` probes.
    ///
    /// [`HnswIndex::search`]: crate::index::hnsw::HnswIndex::search
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

        let mut out: Vec<RecallHit> = Vec::with_capacity(k);
        // Candidates already decoded, so a widened probe re-walks only
        // the suffix it revealed.
        let mut consumed = 0usize;
        let mut want = k.saturating_mul(RECALL_OVERFETCH).max(k);
        let mut probes = 0usize;

        loop {
            // The HNSW search is sync + cheap; do it inside a `read()`
            // guard scope so we don't hold the lock across the storage
            // awaits below.
            let (raw_hits, live_len): (Vec<(MemoryId, f32)>, usize) = {
                let guard = self
                    .index
                    .read()
                    .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
                (guard.search(&qvec, want)?, guard.len())
            };
            probes += 1;

            for (id, score) in raw_hits.iter().skip(consumed) {
                let key = mem_key(id);
                let bytes = match self.storage.get(&key).await? {
                    Some(b) => b,
                    None => {
                        // Vector exists in HNSW but no metadata in KV —
                        // see the write-path ordering note. Skip; the
                        // consolidation scheduler's orphan sweep
                        // (`gc_orphan_vectors`) tombstones it.
                        tracing::warn!(memory_id = %id, "recall: orphan vector, no metadata");
                        continue;
                    }
                };
                let item: MemoryItem = postcard::from_bytes(&bytes)
                    .map_err(|e| MnemeError::Storage(format!("decode MemoryItem {id}: {e}")))?;

                if !filters.matches(&item, *score) {
                    continue;
                }
                out.push(RecallHit {
                    item,
                    score: *score,
                });
                if out.len() >= k {
                    break;
                }
            }
            consumed = raw_hits.len();

            if out.len() >= k {
                break;
            }
            // `search` gave us less than we asked for ⇒ we have seen
            // every live vector it can offer. Same conclusion if the
            // width already covers the whole live corpus.
            if raw_hits.len() < want || want >= live_len {
                break;
            }
            let wider = want.saturating_mul(RECALL_WIDEN_FACTOR);
            // Belt-and-braces: a non-growing width would spin forever.
            // Unreachable while RECALL_WIDEN_FACTOR > 1 and `want` is
            // below usize::MAX, both of which hold by construction.
            if wider <= want {
                break;
            }
            want = wider;
        }

        if probes > 1 {
            tracing::debug!(
                probes,
                returned = out.len(),
                requested = k,
                "recall widened its index probe to satisfy filters"
            );
        }
        Ok(out)
    }

    /// Tombstone every indexed vector whose metadata row is gone.
    /// Returns how many were reclaimed.
    ///
    /// # Why orphans exist
    ///
    /// [`SemanticStore::forget`] deletes the `mem:` row and *then*
    /// appends the `VectorDelete`. A `kill -9` between the two leaves a
    /// vector with no metadata. `recall` already skips such rows (it
    /// logs `orphan vector, no metadata` and moves on), so an orphan is
    /// never *returned* — but it still occupies RAM, consumes a slot in
    /// every search's candidate budget, and makes `index.len()`
    /// disagree with the real corpus size.
    ///
    /// # Safety of the sweep
    ///
    /// It can only ever remove vectors that are already unreachable: a
    /// missing metadata row means no `recall` could surface that id.
    /// The direction is one-way, so a spuriously-slow storage read
    /// cannot cause data loss — worst case is a vector that stays one
    /// pass longer. Reuses `forget`, which tombstones unconditionally
    /// and no-ops on the already-absent metadata delete, so the WAL
    /// records are identical to an interrupted `forget` completing.
    pub async fn gc_orphan_vectors(&self) -> Result<usize> {
        let ids = {
            let guard = self
                .index
                .read()
                .map_err(|e| MnemeError::Index(format!("hnsw rwlock poisoned: {e}")))?;
            guard.live_ids()
        };

        let mut reclaimed = 0usize;
        for id in ids {
            if self.storage.get(&mem_key(&id)).await?.is_some() {
                continue;
            }
            // `forget` tombstones the vector even with no metadata to
            // delete — exactly the completion an interrupted forget
            // never got to.
            self.forget(id).await?;
            reclaimed += 1;
            tracing::debug!(memory_id = %id, "gc: reclaimed orphan vector");
        }
        if reclaimed > 0 {
            tracing::info!(reclaimed, "gc: tombstoned orphan vectors");
        }
        Ok(reclaimed)
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
            return snapshot_scheduler::run_snapshot_inline(
                &self.write_lock,
                &self.index,
                &self.applied_lsn,
                &snapshot_path,
                &wal_dir,
                None,
            )
            .await;
        };
        snapshot_scheduler::run_snapshot(state).await
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
            .recall("topic", 10, &RecallFilters::with_scope("work"))
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.item.scope == "work"));
        assert!(hits.iter().any(|h| h.item.id == work_id));
    }

    /// Regression: a selective scope filter must not silently
    /// underfill. Pre-v1.3 `recall` probed a fixed `k * 4` candidates
    /// and post-filtered, so asking for 10 `work` memories out of a
    /// corpus that is ~4 % `work` returned far fewer than 10 even
    /// though the index held plenty. The widening loop fixes it.
    #[tokio::test]
    async fn recall_fills_k_when_scope_is_a_small_minority() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());

        // 480 noise memories in `personal`, 20 in `work`. With the old
        // fixed 4× probe, a k=10 `work` recall saw only the 40 nearest
        // vectors overall — statistically ~1-2 `work` rows.
        for i in 0..480 {
            s.remember(
                &format!("noise number {i}"),
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        }
        for i in 0..20 {
            s.remember(
                &format!("work item {i}"),
                MemoryKind::Fact,
                vec![],
                "work".into(),
            )
            .await
            .unwrap();
        }

        let hits = s
            .recall("anything at all", 10, &RecallFilters::with_scope("work"))
            .await
            .unwrap();

        assert_eq!(
            hits.len(),
            10,
            "scope filter underfilled: got {} of 10 requested from a 20-row scope",
            hits.len()
        );
        assert!(hits.iter().all(|h| h.item.scope == "work"));
        // No duplicates — the widening loop must not re-emit the
        // candidates an earlier probe already consumed.
        let mut ids: Vec<_> = hits.iter().map(|h| h.item.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "widening loop emitted duplicate hits");
    }

    /// A filter that matches nothing returns empty rather than
    /// looping forever or erroring.
    #[tokio::test]
    async fn recall_with_unmatchable_filter_terminates_empty() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        for i in 0..50 {
            s.remember(
                &format!("row {i}"),
                MemoryKind::Fact,
                vec![],
                "personal".into(),
            )
            .await
            .unwrap();
        }
        let hits = s
            .recall("row", 10, &RecallFilters::with_scope("nonexistent-scope"))
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn recall_filters_by_tags_requiring_all() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let both = s
            .remember(
                "deploy notes",
                MemoryKind::Fact,
                vec!["ops".into(), "prod".into()],
                "personal".into(),
            )
            .await
            .unwrap();
        let only_one = s
            .remember(
                "deploy notes staging",
                MemoryKind::Fact,
                vec!["ops".into()],
                "personal".into(),
            )
            .await
            .unwrap();

        // Both tags required ⇒ only the two-tag memory survives.
        let hits = s
            .recall(
                "deploy",
                10,
                &RecallFilters {
                    tags: vec!["ops".into(), "prod".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item.id, both);

        // A single tag both share ⇒ both survive.
        let hits = s
            .recall(
                "deploy",
                10,
                &RecallFilters {
                    tags: vec!["ops".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.item.id == only_one));
    }

    #[tokio::test]
    async fn recall_min_similarity_drops_weak_hits() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        s.remember("exact phrase here", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();

        // Unfiltered: the nearest vector comes back however far it is.
        let loose = s
            .recall("exact phrase here", 10, &RecallFilters::default())
            .await
            .unwrap();
        assert_eq!(loose.len(), 1);

        // A floor above the achievable similarity drops it. 1.01 is
        // unreachable by construction (max similarity is 1.0), so this
        // holds for any embedder including the stub.
        let strict = s
            .recall(
                "exact phrase here",
                10,
                &RecallFilters {
                    min_similarity: Some(1.01),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(strict.is_empty(), "min_similarity floor was not applied");
    }

    #[tokio::test]
    async fn remember_checked_flags_an_identical_restatement() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let first = s
            .remember(
                "the staging database is wiped every Sunday",
                MemoryKind::Fact,
                vec![],
                "work".into(),
            )
            .await
            .unwrap();

        let (second, dup) = s
            .remember_checked(
                "the staging database is wiped every Sunday",
                MemoryKind::Fact,
                vec![],
                "work".into(),
                true,
            )
            .await
            .unwrap();

        let dup = dup.expect("identical content must be flagged");
        assert_eq!(dup.id, first);
        assert_eq!(dup.scope, "work");
        assert!(
            dup.similarity >= NEAR_DUPLICATE_SIMILARITY,
            "similarity {} below threshold",
            dup.similarity
        );
        // Advisory only — the write still landed.
        assert_ne!(second, first);
        assert!(s.get(second).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn remember_checked_stays_quiet_for_unrelated_content() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        s.remember(
            "the staging database is wiped every Sunday",
            MemoryKind::Fact,
            vec![],
            "work".into(),
        )
        .await
        .unwrap();

        let (_, dup) = s
            .remember_checked(
                "alice prefers tabs over spaces",
                MemoryKind::Preference,
                vec![],
                "work".into(),
                true,
            )
            .await
            .unwrap();
        assert!(dup.is_none(), "unrelated content flagged as duplicate");
    }

    /// Opting out must skip the probe entirely — `remember` is on the
    /// 150 ms p95 write path.
    #[tokio::test]
    async fn remember_without_check_never_reports_duplicates() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        s.remember("same text", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let (_, dup) = s
            .remember_checked("same text", MemoryKind::Fact, vec![], "p".into(), false)
            .await
            .unwrap();
        assert!(dup.is_none());
    }

    /// An orphan is an indexed vector whose metadata row is gone —
    /// what an interrupted `forget` leaves behind. The sweep must
    /// reclaim it and leave every reachable memory alone.
    #[tokio::test]
    async fn gc_reclaims_orphan_vectors_only() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        let keep = s
            .remember("keep me", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let orphan = s
            .remember("orphan me", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();

        // Simulate a `forget` that died after the metadata delete but
        // before the WAL tombstone: drop the row behind the store's back.
        s.storage.delete(&mem_key(&orphan)).await.unwrap();

        let reclaimed = s.gc_orphan_vectors().await.unwrap();
        assert_eq!(reclaimed, 1, "expected exactly the orphan to be reclaimed");
        assert!(s.get(keep).await.unwrap().is_some(), "live memory removed");

        // Idempotent: a second sweep finds nothing.
        assert_eq!(s.gc_orphan_vectors().await.unwrap(), 0);

        // And the reclaimed vector no longer occupies a recall slot.
        let hits = s
            .recall("orphan me", 10, &RecallFilters::default())
            .await
            .unwrap();
        assert!(hits.iter().all(|h| h.item.id != orphan));
    }

    #[tokio::test]
    async fn gc_on_a_clean_store_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let s = store_with_stub(tmp.path());
        for i in 0..5 {
            s.remember(&format!("row {i}"), MemoryKind::Fact, vec![], "p".into())
                .await
                .unwrap();
        }
        assert_eq!(s.gc_orphan_vectors().await.unwrap(), 0);
    }

    #[test]
    fn similarity_is_the_inverse_of_distance() {
        assert_eq!(similarity_from_distance(0.0), 1.0);
        assert_eq!(similarity_from_distance(1.0), 0.0);
        assert_eq!(similarity_from_distance(2.0), -1.0);
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
                    kind: Some(MemoryKind::Decision),
                    ..Default::default()
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
