//! Unix-domain-socket listener for the v1.1 daemon (ADR-0012 D2/D5).
//!
//! Responsibilities:
//!
//! 1. Resolve the per-data-dir socket path (`<root>/run/mneme.sock`).
//! 2. Detect + clean up orphaned socket files left by a crashed prior
//!    daemon — try connecting first, unlink only if the listener is
//!    actually dead (D5).
//! 3. Refuse to start if another live daemon is already serving.
//! 4. Bind with mode `0600` so only the owning user can connect (the
//!    auth-token check at handshake is defense in depth on top).
//! 5. Wrap the [`tokio::net::UnixListener`] in a [`Listener`] RAII guard
//!    that unlinks the socket on drop — even on panic — so `mneme stop`
//!    plus subsequent `mneme daemon` see a clean filesystem.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::net::UnixListener;

/// Filename of the per-data-dir Unix domain socket.
pub const SOCKET_FILENAME: &str = "mneme.sock";

/// Owner-only mode bits applied to the socket file after bind.
pub const SOCKET_MODE: u32 = 0o600;

/// Owner-only mode bits applied to `<root>/run/` on first creation.
/// Mirrors the `upgrade_audit` module's choice so the directory is
/// uniformly bounded to the data-dir owner before any daemon state
/// (auth token, sockets, lifecycle markers) lands inside.
pub const RUN_DIR_MODE: u32 = 0o700;

/// Time we'll spend trying to connect to a possibly-stale socket
/// before deciding it's dead. The peer is local — anything beyond a
/// few hundred ms is not "slow", it's "no listener". Kept tight so
/// stale-cleanup doesn't add latency to legitimate daemon starts.
const STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum ListenerError {
    /// Another daemon is alive on the same data directory — refuse to
    /// start. Carries the path so the caller can print it verbatim
    /// without reaching back into this module.
    #[error("another mneme daemon is already serving {0}")]
    AlreadyAlive(PathBuf),
    /// IO failure while binding, unlinking, or chmod-ing the socket.
    #[error("daemon listener IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Path to the daemon's Unix socket given the data-dir root.
pub fn socket_path(root: &Path) -> PathBuf {
    root.join("run").join(SOCKET_FILENAME)
}

/// Wait for a daemon to be ready to serve, polling with exponential
/// backoff. Used by D12's spawn protocol (`mneme client` auto-spawns
/// the daemon and waits for it to finish booting).
///
/// Backoff starts at 10 ms and doubles each attempt, capped at 1 s.
/// Returns `Ok(())` once a `UnixStream::connect` to `path` succeeds;
/// returns an error after the total elapsed time exceeds `deadline`.
///
/// Why we probe with `connect` rather than `path.exists()`: the file
/// can appear well before the daemon's accept loop is ready (e.g.
/// during a daemon restart where the old socket file is still on
/// disk, or in the window between bind and the accept task being
/// spawned). A bare existence check returns true in those cases and
/// callers downstream get `ECONNREFUSED` or an empty read on their
/// next operation — exactly the race that broke
/// `client_reconnects_after_daemon_restart` (tests/daemon_e2e.rs:484
/// "post-reconnect stats JSON: EOF while parsing a value").
pub async fn wait_for_socket(path: &Path, deadline: Duration) -> Result<(), ListenerError> {
    let started = std::time::Instant::now();
    let mut delay = Duration::from_millis(10);
    let max_delay = Duration::from_secs(1);
    // Per-attempt probe timeout — the peer is local, anything slower
    // than this is functionally "not ready yet" rather than slow.
    // Matches `STALE_PROBE_TIMEOUT` upstairs so a hung listener
    // doesn't wedge the wait loop.
    let probe_timeout = STALE_PROBE_TIMEOUT;
    loop {
        match tokio::time::timeout(probe_timeout, tokio::net::UnixStream::connect(path)).await {
            Ok(Ok(_stream)) => return Ok(()),
            Ok(Err(_)) | Err(_) => {
                // Not ready: file missing, ECONNREFUSED (post-stale-
                // cleanup window or pre-accept), other transient I/O,
                // or probe timed out. Fall through to the backoff arm.
            }
        }
        if started.elapsed() + delay > deadline {
            return Err(ListenerError::Io {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "daemon socket did not accept connections within {deadline:?} (waited {:?})",
                        started.elapsed()
                    ),
                ),
            });
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}

/// Bind a listener at `<root>/run/mneme.sock` after performing the
/// stale-cleanup probe. Returns a [`Listener`] RAII guard that
/// unlinks the socket file on drop (so `mneme stop` followed by a
/// fresh `mneme daemon` always sees a clean filesystem).
pub async fn bind_listener(root: &Path) -> Result<Listener, ListenerError> {
    let path = socket_path(root);
    ensure_run_dir(&path)?;
    cleanup_stale_socket(&path).await?;
    let inner = UnixListener::bind(&path).map_err(|e| ListenerError::Io {
        path: path.clone(),
        source: e,
    })?;
    set_socket_perms(&path)?;
    Ok(Listener {
        inner,
        path: Some(path),
    })
}

fn ensure_run_dir(socket: &Path) -> Result<(), ListenerError> {
    let dir = socket.parent().ok_or_else(|| ListenerError::Io {
        path: socket.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent dir"),
    })?;
    std::fs::create_dir_all(dir).map_err(|e| ListenerError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let perms = std::fs::Permissions::from_mode(RUN_DIR_MODE);
    std::fs::set_permissions(dir, perms).map_err(|e| ListenerError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

async fn cleanup_stale_socket(path: &Path) -> Result<(), ListenerError> {
    if !path.exists() {
        return Ok(());
    }

    // Fast path: if the existing entry isn't even a socket file
    // type (e.g. someone left a regular file at the path, or a
    // restored backup placed garbage there), unlink without probing.
    // `UnixStream::connect` against a regular file returns
    // `ENOTSOCK` rather than a friendly `io::ErrorKind`, so doing
    // this check up front keeps the connect-probe match arms small.
    let metadata = std::fs::symlink_metadata(path).map_err(|e| ListenerError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let file_type = metadata.file_type();
    use std::os::unix::fs::FileTypeExt;
    if !file_type.is_socket() {
        std::fs::remove_file(path).map_err(|src| ListenerError::Io {
            path: path.to_path_buf(),
            source: src,
        })?;
        return Ok(());
    }

    // Slow path: it IS a socket file. Probe by attempting to connect.
    // Outcomes:
    // - Ok(_)         → another daemon is alive; refuse.
    // - Err(refused)  → the socket's there but no listener — stale.
    // - Err(notfound) → race: the file vanished between stat and
    //                   connect (unlikely but cheap to handle).
    // - Err(other)    → unknown failure (perms? broken FS?). Refuse
    //                   loudly rather than silently unlink — could
    //                   mask a live process behind a permissions
    //                   issue.
    // - Timeout       → peer accepted but never closed. Hung daemon
    //                   is still a daemon; treat as alive.
    match tokio::time::timeout(STALE_PROBE_TIMEOUT, tokio::net::UnixStream::connect(path)).await {
        Ok(Ok(_stream)) => Err(ListenerError::AlreadyAlive(path.to_path_buf())),
        Ok(Err(e))
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(path).map_err(|src| ListenerError::Io {
                path: path.to_path_buf(),
                source: src,
            })?;
            Ok(())
        }
        Ok(Err(e)) => Err(ListenerError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
        Err(_) => Err(ListenerError::AlreadyAlive(path.to_path_buf())),
    }
}

fn set_socket_perms(path: &Path) -> Result<(), ListenerError> {
    let perms = std::fs::Permissions::from_mode(SOCKET_MODE);
    std::fs::set_permissions(path, perms).map_err(|e| ListenerError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

/// RAII wrapper around the bound listener. On drop, unlinks the
/// socket file so the next `mneme daemon` invocation doesn't trip
/// the stale-socket probe needlessly. `Drop` is best-effort by
/// design — a failed unlink logs at warn level rather than panicking.
#[derive(Debug)]
pub struct Listener {
    inner: UnixListener,
    /// `Some(_)` until [`take_path`] consumes it; lets callers
    /// suppress the auto-unlink (e.g. when handing off to a child
    /// process that will manage the socket lifetime).
    path: Option<PathBuf>,
}

impl Listener {
    /// Borrow the underlying [`tokio::net::UnixListener`] for accept
    /// loops or `select!` bodies.
    pub fn as_inner(&self) -> &UnixListener {
        &self.inner
    }

    /// Path to the bound socket file.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Disable the auto-unlink-on-drop. Returns the path so the
    /// caller can take over lifecycle ownership.
    pub fn take_path(&mut self) -> Option<PathBuf> {
        self.path.take()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(e) = std::fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "daemon socket cleanup failed on drop"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn bind_creates_run_dir_and_socket_with_owner_perms() {
        let tmp = TempDir::new().unwrap();
        let listener = bind_listener(tmp.path()).await.expect("bind ok");

        let path = socket_path(tmp.path());
        assert!(path.exists(), "socket file must exist after bind");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE, "socket must be 0o600, got {mode:o}");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, RUN_DIR_MODE,
            "run/ must be 0o700, got {dir_mode:o}"
        );

        drop(listener);
        assert!(!path.exists(), "socket must be unlinked on drop");
    }

    #[tokio::test]
    async fn stale_socket_is_cleaned_up_and_rebind_succeeds() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("run");
        std::fs::create_dir_all(&dir).unwrap();

        // Plant a stale socket file — a regular file at the socket
        // path mimics the post-crash state. UnixStream::connect on
        // it returns ECONNREFUSED, our cleanup unlinks it, and the
        // bind below wins.
        let path = socket_path(tmp.path());
        std::fs::write(&path, b"stale").unwrap();
        assert!(path.exists());

        let listener = bind_listener(tmp.path()).await.expect("rebind ok");
        // Socket file was replaced by a real socket entry, not the
        // stale regular file; bind succeeded.
        use std::os::unix::fs::FileTypeExt;
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(
            meta.file_type().is_socket(),
            "rebound entry must be a socket, got {:?}",
            meta.file_type()
        );
        drop(listener);
    }

    #[tokio::test]
    async fn refuses_to_bind_when_another_daemon_is_alive() {
        let tmp = TempDir::new().unwrap();
        let first = bind_listener(tmp.path()).await.expect("first bind ok");

        let err = bind_listener(tmp.path())
            .await
            .expect_err("second bind must refuse");
        match err {
            ListenerError::AlreadyAlive(p) => {
                assert_eq!(p, socket_path(tmp.path()));
            }
            other => panic!("expected AlreadyAlive, got {other:?}"),
        }

        drop(first);
    }

    /// Drop's auto-unlink can be suppressed by `take_path` — useful
    /// when the listener fd is being handed to a long-lived child
    /// process that will own the socket lifetime.
    #[tokio::test]
    async fn take_path_suppresses_drop_unlink() {
        let tmp = TempDir::new().unwrap();
        let mut listener = bind_listener(tmp.path()).await.unwrap();
        let path = listener.take_path().expect("path was Some");
        drop(listener);
        assert!(
            path.exists(),
            "take_path must suppress the drop-time unlink"
        );
        // Manual cleanup so subsequent test runs see a clean state
        // (TempDir's drop will reap the directory anyway).
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn socket_path_is_under_run_subdir() {
        let tmp = TempDir::new().unwrap();
        let path = socket_path(tmp.path());
        assert!(path.starts_with(tmp.path().join("run")));
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some(SOCKET_FILENAME)
        );
    }
}
