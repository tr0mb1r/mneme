//! L1 working memory — per-session conversation state with on-disk
//! checkpointing.
//!
//! Two layers in this module:
//!
//! * [`Session`] — the on-disk serialisation primitive. Plain struct
//!   with `checkpoint(...)` / `load(...)` / `shutdown(...)` methods.
//!   Phase 2 shipped this much.
//! * [`ActiveSession`] — Phase 6 §8.2 wrapper. Owns an `Arc<RwLock<
//!   Session>>` and a turn counter so the MCP server (write side) and
//!   the [`crate::memory::checkpoint_scheduler::CheckpointScheduler`]
//!   (read-and-flush side) can share state without copying.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::{MnemeError, Result};

/// One conversational turn within a session. Phase 2 keeps the schema
/// minimal — later phases will extend with embeddings, references to
/// L4 memories, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    pub role: String,
    pub content: String,
    pub at: DateTime<Utc>,
}

/// In-memory session state. Produced by the MCP server on each new
/// conversation; checkpointed periodically to survive restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub turns: Vec<Turn>,
    pub started_at: DateTime<Utc>,
    pub last_checkpoint_at: Option<DateTime<Utc>>,
    /// `true` once a clean shutdown has flushed; `false` after a crash.
    pub clean_shutdown: bool,
}

impl Session {
    /// Start a new session with no turns.
    pub fn new() -> Self {
        Self {
            id: SessionId::new(),
            turns: Vec::new(),
            started_at: Utc::now(),
            last_checkpoint_at: None,
            clean_shutdown: false,
        }
    }

    /// Record a turn.
    pub fn push_turn(&mut self, role: impl Into<String>, content: impl Into<String>) {
        self.turns.push(Turn {
            role: role.into(),
            content: content.into(),
            at: Utc::now(),
        });
    }

    /// Persist this session to `{sessions_dir}/{id}.snapshot`.
    pub fn checkpoint(&mut self, sessions_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(sessions_dir)?;
        let path = self.snapshot_path(sessions_dir);
        let bytes =
            serde_json::to_vec(self).map_err(|e| MnemeError::Storage(format!("serde: {e}")))?;
        // Atomic-ish replace: write to .tmp then rename.
        let tmp = path.with_extension("snapshot.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        self.last_checkpoint_at = Some(Utc::now());
        Ok(())
    }

    /// Mark a clean shutdown and checkpoint a final time.
    pub fn shutdown(&mut self, sessions_dir: &Path) -> Result<()> {
        self.clean_shutdown = true;
        self.checkpoint(sessions_dir)
    }

    /// Restore a session previously written via [`checkpoint`].
    pub fn load(sessions_dir: &Path, id: SessionId) -> Result<Self> {
        let path = sessions_dir.join(format!("{id}.snapshot"));
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| MnemeError::Storage(format!("serde {path:?}: {e}")))
    }

    fn snapshot_path(&self, sessions_dir: &Path) -> PathBuf {
        sessions_dir.join(format!("{}.snapshot", self.id))
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// Server-lifetime wrapper around a [`Session`] plus the bookkeeping
/// the checkpoint scheduler needs.
///
/// One `ActiveSession` lives for the duration of a `mneme run` — boot
/// allocates a fresh [`SessionId`], every successful `tools/call`
/// pushes a turn, and shutdown flushes a final checkpoint. The
/// scheduler holds an `Arc` of this and reads the turn counter to
/// decide when to fire.
///
/// Concurrency model:
/// * Writes (`push_turn`, `checkpoint`) take the inner `RwLock`'s
///   write half. The MCP server is single-tasked today, so there's
///   no cross-handler contention; future per-handler tasks will
///   serialise on the write lock without affecting reads.
/// * Reads (`turns_snapshot`, `id`, `started_at`) take the read
///   half. The orchestrator's L1 fold-in (separate work item) will
///   pull through that path.
pub struct ActiveSession {
    inner: Arc<RwLock<Session>>,
    sessions_dir: PathBuf,
    /// Cumulative turn count since process start. Bumped on every
    /// `push_turn`. The scheduler reads this on tick boundaries; a
    /// successful checkpoint records the value at the moment of the
    /// flush in `last_flush_turns` so the next-fire threshold is
    /// `last_flush_turns + interval_turns`.
    turn_counter: Arc<AtomicU64>,
    /// Snapshot of `turn_counter` at the last successful checkpoint.
    /// `0` until the first checkpoint completes. Used by the
    /// scheduler to test "interval_turns since last flush".
    last_flush_turns: Arc<AtomicU64>,
    /// Total successful checkpoints since boot. Surfaced for
    /// diagnostics + the `mneme://stats` resource.
    checkpoints_total: Arc<AtomicU64>,
}

impl ActiveSession {
    /// Allocate a fresh session and prepare the on-disk directory.
    /// Failures here surface immediately rather than on first
    /// checkpoint — better to refuse to boot if the sessions dir
    /// is unwritable than to discover it 30 seconds in.
    pub fn open(sessions_dir: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&sessions_dir).map_err(MnemeError::Io)?;
        Ok(Arc::new(Self {
            inner: Arc::new(RwLock::new(Session::new())),
            sessions_dir,
            turn_counter: Arc::new(AtomicU64::new(0)),
            last_flush_turns: Arc::new(AtomicU64::new(0)),
            checkpoints_total: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// The active session's id. Stable for the lifetime of `mneme
    /// run`.
    pub fn id(&self) -> SessionId {
        self.inner.read().expect("session lock poisoned").id
    }

    /// When the active session started. Stable for the lifetime of
    /// `mneme run`.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.inner.read().expect("session lock poisoned").started_at
    }

    /// Last successful checkpoint timestamp, or `None` if no flush
    /// has happened yet.
    pub fn last_checkpoint_at(&self) -> Option<DateTime<Utc>> {
        self.inner
            .read()
            .expect("session lock poisoned")
            .last_checkpoint_at
    }

    /// Total turns recorded since boot. Cheap atomic read.
    pub fn turns_total(&self) -> u64 {
        self.turn_counter.load(Ordering::SeqCst)
    }

    /// Turns recorded since the most recent successful checkpoint.
    /// Used by the scheduler's interval-turns trigger.
    pub fn turns_since_last_checkpoint(&self) -> u64 {
        self.turn_counter
            .load(Ordering::SeqCst)
            .saturating_sub(self.last_flush_turns.load(Ordering::SeqCst))
    }

    /// Total successful checkpoints since boot.
    pub fn checkpoints_total(&self) -> u64 {
        self.checkpoints_total.load(Ordering::SeqCst)
    }

    /// Path to where this session's snapshot lives on disk. Useful
    /// for tests + the future `mneme://session/{id}` resource.
    pub fn snapshot_path(&self) -> PathBuf {
        self.sessions_dir.join(format!("{}.snapshot", self.id()))
    }

    /// Append a turn. Bumps the per-process turn counter (visible to
    /// the scheduler immediately) and the in-memory turn list (will
    /// be flushed on the next checkpoint).
    pub fn push_turn(&self, role: impl Into<String>, content: impl Into<String>) {
        {
            let mut s = self.inner.write().expect("session lock poisoned");
            s.push_turn(role, content);
        }
        self.turn_counter.fetch_add(1, Ordering::SeqCst);
    }

    /// Flush the current state to disk. Idempotent — calling twice
    /// in a row writes the same bytes. Cheap (a single `write +
    /// rename`) so the scheduler can call it freely.
    pub fn checkpoint(&self) -> Result<()> {
        // The Session::checkpoint API takes &mut self; lock-write
        // covers the whole flush so a concurrent push_turn can't
        // mutate the buffer mid-serialisation.
        let mut s = self.inner.write().expect("session lock poisoned");
        s.checkpoint(&self.sessions_dir)?;
        // Capture the counter at the moment the flush succeeded so
        // the next-fire threshold is computed from this point. There
        // is a tiny window where push_turn could increment between
        // the buffer serialisation and this load; the worst case is
        // we count one more turn than was actually flushed, and the
        // next checkpoint flushes one fewer — strictly conservative.
        let now_turns = self.turn_counter.load(Ordering::SeqCst);
        self.last_flush_turns.store(now_turns, Ordering::SeqCst);
        self.checkpoints_total.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Mark `clean_shutdown = true` and write a final checkpoint.
    /// Idempotent.
    pub fn shutdown(&self) -> Result<()> {
        let mut s = self.inner.write().expect("session lock poisoned");
        s.shutdown(&self.sessions_dir)?;
        let now_turns = self.turn_counter.load(Ordering::SeqCst);
        self.last_flush_turns.store(now_turns, Ordering::SeqCst);
        self.checkpoints_total.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Cheap clone of the turn list for read-side fold-in (orchestrator
    /// L1 path). Holds the read lock only for the duration of the
    /// clone.
    pub fn turns_snapshot(&self) -> Vec<Turn> {
        self.inner
            .read()
            .expect("session lock poisoned")
            .turns
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_checkpoint_then_load() {
        let tmp = TempDir::new().unwrap();
        let mut s = Session::new();
        s.push_turn("user", "hello");
        s.push_turn("assistant", "hi there");
        s.checkpoint(tmp.path()).unwrap();
        let id = s.id;

        let loaded = Session::load(tmp.path(), id).unwrap();
        assert_eq!(loaded.turns, s.turns);
        assert_eq!(loaded.id, s.id);
        assert!(!loaded.clean_shutdown);
    }

    #[test]
    fn shutdown_marks_clean() {
        let tmp = TempDir::new().unwrap();
        let mut s = Session::new();
        s.shutdown(tmp.path()).unwrap();
        let loaded = Session::load(tmp.path(), s.id).unwrap();
        assert!(loaded.clean_shutdown);
    }

    #[test]
    fn active_session_push_turn_bumps_counter() {
        let tmp = TempDir::new().unwrap();
        let s = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(s.turns_total(), 0);
        s.push_turn("user", "hi");
        s.push_turn("assistant", "hello");
        assert_eq!(s.turns_total(), 2);
        assert_eq!(s.turns_since_last_checkpoint(), 2);
    }

    #[test]
    fn active_session_checkpoint_resets_turns_since_last_flush() {
        let tmp = TempDir::new().unwrap();
        let s = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        s.push_turn("user", "a");
        s.push_turn("user", "b");
        s.push_turn("user", "c");
        assert_eq!(s.turns_since_last_checkpoint(), 3);
        s.checkpoint().unwrap();
        assert_eq!(s.turns_since_last_checkpoint(), 0);
        assert_eq!(s.checkpoints_total(), 1);
        assert_eq!(s.turns_total(), 3, "cumulative counter never resets");

        s.push_turn("user", "d");
        assert_eq!(s.turns_since_last_checkpoint(), 1);
    }

    #[test]
    fn active_session_checkpoint_persists_to_disk_round_trips() {
        let tmp = TempDir::new().unwrap();
        let s = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        s.push_turn("user", "remember this");
        s.push_turn("assistant", "ok");
        s.checkpoint().unwrap();

        let id = s.id();
        let loaded = Session::load(tmp.path(), id).unwrap();
        assert_eq!(loaded.turns.len(), 2);
        assert_eq!(loaded.turns[0].content, "remember this");
        assert_eq!(loaded.turns[1].role, "assistant");
        assert!(!loaded.clean_shutdown);
    }

    #[test]
    fn active_session_shutdown_marks_clean_and_increments_count() {
        let tmp = TempDir::new().unwrap();
        let s = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        s.push_turn("user", "x");
        s.shutdown().unwrap();
        let id = s.id();
        let loaded = Session::load(tmp.path(), id).unwrap();
        assert!(loaded.clean_shutdown);
        assert_eq!(s.checkpoints_total(), 1);
    }

    #[test]
    fn active_session_open_creates_dir_idempotently() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("not").join("yet");
        let s = ActiveSession::open(nested.clone()).unwrap();
        assert!(nested.exists());
        // Second open with the same dir succeeds — create_dir_all is
        // idempotent, and the SessionId differs because we allocate
        // a fresh one.
        let s2 = ActiveSession::open(nested).unwrap();
        assert_ne!(s.id(), s2.id());
    }

    #[test]
    fn checkpoint_atomic_replace() {
        // Two checkpoints in a row produce only the final snapshot;
        // .tmp must not be left behind.
        let tmp = TempDir::new().unwrap();
        let mut s = Session::new();
        s.push_turn("user", "first");
        s.checkpoint(tmp.path()).unwrap();
        s.push_turn("user", "second");
        s.checkpoint(tmp.path()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "expected only one snapshot file: {names:?}");
        assert!(names[0].ends_with(".snapshot"));
    }
}
