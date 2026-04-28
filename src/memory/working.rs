//! L2 working memory — per-session conversation state with on-disk
//! checkpointing.
//!
//! Phase 2 ships only the persistence primitive: a `Session` struct that
//! can serialize itself to `~/.mneme/sessions/{id}.snapshot` and reload.
//! Auto-checkpoint scheduling (every N seconds / M turns per spec §8.3)
//! is wired in when the MCP server starts driving sessions in a later phase.

use crate::ids::SessionId;
use crate::{MnemeError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
