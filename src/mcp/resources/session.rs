//! `mneme://session/{id}` — Phase 6 §8.2 read-side companion to the
//! L1 working-session checkpoint scheduler.
//!
//! Returns the JSON-serialised `Session` for any `id` the client
//! supplies. The active session is served from in-memory state (so
//! mid-session reads see turns the next checkpoint hasn't yet
//! flushed). Past sessions fall back to disk via `Session::load`.
//!
//! URI scheme: `mneme://session/<ulid>`. The `descriptor().uri` is
//! the RFC 6570 template form (`mneme://session/{id}`); the registry
//! routes any URI starting with `mneme://session/` to this handler.
//!
//! Behaviour:
//! * Active session id → in-memory snapshot, including the
//!   `clean_shutdown` flag (which is `false` while the process is
//!   still up) and the latest turn list.
//! * Past session id present on disk → loaded snapshot.
//! * Unknown id → `ResourceError::NotFound`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::ids::SessionId;
use crate::memory::working::{ActiveSession, Session};

/// URI prefix the registry matches against. Any `mneme://session/...`
/// URI routes to the [`SessionResource`].
pub const URI_PREFIX: &str = "mneme://session/";

pub struct SessionResource {
    /// In-memory state of the currently-active session. None in
    /// fixtures that test only the disk-load path.
    active: Option<Arc<ActiveSession>>,
    /// Where past-session snapshots live on disk. Same path
    /// `cli::run` hands to `ActiveSession::open`.
    sessions_dir: PathBuf,
}

impl SessionResource {
    pub fn new(active: Option<Arc<ActiveSession>>, sessions_dir: PathBuf) -> Self {
        Self {
            active,
            sessions_dir,
        }
    }

    /// Parse the trailing ULID off `mneme://session/<ulid>`. Returns
    /// `None` for malformed URIs so the caller can produce a clean
    /// `NotFound` error.
    fn parse_session_id(uri: &str) -> Option<SessionId> {
        let suffix = uri.strip_prefix(URI_PREFIX)?;
        // Take everything up to a fragment / query string boundary.
        let raw = suffix.split(['?', '#', '/']).next()?;
        // SessionId wraps a ulid::Ulid; reuse its FromStr.
        raw.parse::<ulid::Ulid>().ok().map(SessionId)
    }
}

#[async_trait]
impl Resource for SessionResource {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            // RFC 6570 template form. Hosts that surface URI
            // templates show this directly; the registry's
            // prefix-match handles substitution at dispatch time.
            uri: "mneme://session/{id}",
            name: "session",
            description: "L1 working-session snapshot for a given session id (active in-memory \
                 state, or a past session loaded from disk).",
            mime_type: "application/json",
        }
    }

    async fn read(&self, uri: &str) -> Result<ResourceContent, ResourceError> {
        let id = Self::parse_session_id(uri)
            .ok_or_else(|| ResourceError::NotFound(format!("invalid session uri: {uri}")))?;

        // Active session: serve from memory. The id check is cheap;
        // if the active session matches, we render its current
        // state without round-tripping through disk.
        if let Some(active) = self.active.as_ref()
            && active.id() == id
        {
            let body = json!({
                "id": id.to_string(),
                "started_at": active.started_at().to_rfc3339(),
                "last_checkpoint_at": active
                    .last_checkpoint_at()
                    .map(|d| d.to_rfc3339()),
                "turns_total": active.turns_total(),
                "turns_since_last_checkpoint": active.turns_since_last_checkpoint(),
                "checkpoints_total": active.checkpoints_total(),
                "clean_shutdown": false,
                "turns": active.turns_snapshot()
                    .iter()
                    .map(|t| json!({
                        "role": t.role,
                        "content": t.content,
                        "at": t.at.to_rfc3339(),
                    }))
                    .collect::<Vec<_>>(),
                "source": "active",
            });
            let text = serde_json::to_string(&body)
                .map_err(|e| ResourceError::Internal(format!("serialise active session: {e}")))?;
            return Ok(ResourceContent {
                uri: uri.to_owned(),
                mime_type: "application/json",
                text,
            });
        }

        // Past session: load from disk.
        let session = Session::load(&self.sessions_dir, id).map_err(|e| {
            // Distinguish "not found" (file missing) from other I/O
            // errors so the protocol layer can return the right code.
            // `Session::load` doesn't categorise errors, so we treat
            // any failure as NotFound here. A real not-found is the
            // common case; a corrupt-snapshot scenario would surface
            // as the same error with similar UX (the user goes to
            // inspect the file directly).
            ResourceError::NotFound(format!("session {id} not found: {e}"))
        })?;

        let body = json!({
            "id": session.id.to_string(),
            "started_at": session.started_at.to_rfc3339(),
            "last_checkpoint_at": session
                .last_checkpoint_at
                .map(|d| d.to_rfc3339()),
            "turns_total": session.turns.len() as u64,
            "checkpoints_total": serde_json::Value::Null,
            "clean_shutdown": session.clean_shutdown,
            "turns": session.turns.iter().map(|t| json!({
                "role": t.role,
                "content": t.content,
                "at": t.at.to_rfc3339(),
            })).collect::<Vec<_>>(),
            "source": "disk",
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ResourceError::Internal(format!("serialise past session: {e}")))?;
        Ok(ResourceContent {
            uri: uri.to_owned(),
            mime_type: "application/json",
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn parses_session_id_from_uri() {
        let id = SessionId::new();
        let uri = format!("{URI_PREFIX}{id}");
        let parsed = SessionResource::parse_session_id(&uri).unwrap();
        assert_eq!(parsed, id);
    }

    #[tokio::test]
    async fn parse_rejects_unknown_scheme() {
        assert!(SessionResource::parse_session_id("mneme://stats").is_none());
        assert!(SessionResource::parse_session_id("mneme://session/").is_none());
        assert!(SessionResource::parse_session_id("mneme://session/not-a-ulid").is_none());
    }

    #[tokio::test]
    async fn read_active_session_serves_in_memory_state() {
        let tmp = TempDir::new().unwrap();
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        active.push_turn("user", "first");
        active.push_turn("assistant", "reply");

        let r = SessionResource::new(Some(Arc::clone(&active)), tmp.path().to_path_buf());
        let uri = format!("{URI_PREFIX}{}", active.id());
        let c = r.read(&uri).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v["id"], active.id().to_string());
        assert_eq!(v["turns_total"], 2);
        assert_eq!(v["source"], "active");
        assert_eq!(v["turns"][0]["role"], "user");
        assert_eq!(v["turns"][1]["content"], "reply");
        // Active session never reports clean_shutdown=true.
        assert_eq!(v["clean_shutdown"], false);
    }

    #[tokio::test]
    async fn read_past_session_loads_from_disk() {
        let tmp = TempDir::new().unwrap();
        // Boot a session, write a checkpoint, drop it.
        let prior = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        prior.push_turn("user", "from yesterday");
        prior.shutdown().unwrap();
        let prior_id = prior.id();
        drop(prior);

        // Now stand up a NEW active session and resource.
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        let r = SessionResource::new(Some(Arc::clone(&active)), tmp.path().to_path_buf());
        let uri = format!("{URI_PREFIX}{prior_id}");
        let c = r.read(&uri).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v["id"], prior_id.to_string());
        assert_eq!(v["source"], "disk");
        assert_eq!(v["clean_shutdown"], true);
        assert_eq!(v["turns_total"], 1);
        assert_eq!(v["turns"][0]["content"], "from yesterday");
    }

    #[tokio::test]
    async fn read_unknown_id_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let r = SessionResource::new(None, tmp.path().to_path_buf());
        let unknown = SessionId::new();
        let uri = format!("{URI_PREFIX}{unknown}");
        let err = r.read(&uri).await.unwrap_err();
        assert!(matches!(err, ResourceError::NotFound(_)));
    }

    #[tokio::test]
    async fn descriptor_uri_is_template_form() {
        let tmp = TempDir::new().unwrap();
        let r = SessionResource::new(None, tmp.path().to_path_buf());
        assert_eq!(r.descriptor().uri, "mneme://session/{id}");
    }
}
