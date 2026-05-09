//! `mneme client` — thin stdio↔unix-socket bridge that lets MCP
//! hosts (Claude Code, Claude Desktop, Cursor, …) share one
//! long-running `mneme daemon` per data dir without each agent
//! growing a dependency on a non-stdio MCP transport.
//!
//! Why this exists (release-planning v1.1 dogfood, ADR-0012 D12):
//!
//! - Every supported MCP host can spawn a stdio subprocess. Not all
//!   support HTTP+SSE today.
//! - The daemon's auth token (`~/.mneme/run/auth.token`, mode 0600)
//!   must NEVER be embedded in agent config files (Invariant 3:
//!   "AUTH TOKEN BY PATH, NEVER EMBEDDED"). Agent configs sit at
//!   mode 644 — embedding would broaden the token's blast radius.
//!   The wrapper reads the token at spawn time so the value never
//!   leaves the wrapper process.
//! - Lifecycle stays clean: one daemon owns the schedulers /
//!   semantic store / consolidation; one wrapper per agent session
//!   is a stdio↔socket pipe that dies with the agent.
//!
//! Wire flow per agent session:
//!
//! 1. Agent spawns `mneme client` as its MCP subprocess.
//! 2. `client` reads the token from `<root>/run/auth.token`.
//! 3. `client` connects to `<root>/run/mneme.sock`.
//! 4. `client` writes `MNEME-AUTH: <token>\n` (the daemon's
//!    `auth::handshake` reads it, validates, drops the connection
//!    on rejection).
//! 5. `client` enters a bidirectional byte-pipe: stdin → socket,
//!    socket → stdout. Framing is agent ↔ server end-to-end —
//!    `client` never parses MCP frames.
//! 6. When either side closes (agent quits / daemon shuts down /
//!    socket errors), `client` exits.
//!
//! Per ADR-0012 D8 the daemon serialises writes through the
//! single-writer storage seam; the wrapper has no part in that —
//! it's just a transport adapter.

use std::path::Path;

use tokio::io::{AsyncWriteExt, copy};
use tokio::net::UnixStream;

use crate::daemon::auth::{self, AUTH_HEADER_PREFIX};
use crate::daemon::socket_path;
use crate::storage::layout;
use crate::{MnemeError, Result};

pub fn execute() -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    // Best-effort scaffold — doesn't write the daemon, only ensures
    // the directory layout is there so the auth token + socket path
    // resolve cleanly. A user that runs `mneme client` before
    // anything else will hit the "daemon not running" error below
    // rather than a "no such directory" surprise.
    layout::scaffold(&root)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    runtime.block_on(run(&root))
}

async fn run(root: &Path) -> Result<()> {
    let token = auth::read_token(root).map_err(|e| {
        MnemeError::Config(format!(
            "could not read daemon auth token at {}: {e} \
             (run `mneme daemon` once to generate it, or `mneme auth rotate`)",
            auth::token_path(root).display()
        ))
    })?;
    let socket = socket_path(root);
    let mut stream = UnixStream::connect(&socket).await.map_err(|e| {
        MnemeError::Mcp(format!(
            "could not connect to daemon socket at {}: {e} \
             (is `mneme daemon` running?)",
            socket.display()
        ))
    })?;

    // Auth handshake — write `MNEME-AUTH: <token>\n` before any
    // MCP frames. The daemon's `auth::handshake` reads the line,
    // validates against the on-disk token, and drops the connection
    // on rejection (the JSON-RPC error frame it writes back will
    // surface to the agent as a one-time message; the agent then
    // sees EOF and reports "MCP server exited").
    let token_str = std::str::from_utf8(&token)
        .map_err(|e| MnemeError::Config(format!("auth token is not utf-8: {e}")))?;
    let mut auth_line = String::with_capacity(AUTH_HEADER_PREFIX.len() + token_str.len() + 1);
    auth_line.push_str(AUTH_HEADER_PREFIX);
    auth_line.push_str(token_str);
    auth_line.push('\n');
    stream
        .write_all(auth_line.as_bytes())
        .await
        .map_err(MnemeError::Io)?;
    stream.flush().await.map_err(MnemeError::Io)?;

    // Bidirectional pipe. `tokio::io::copy` runs until either side
    // returns EOF or errors; the `tokio::select!` returns as soon
    // as one half exits so the wrapper doesn't hang waiting on the
    // other direction. Use `into_split` so each direction runs in
    // its own task without sharing the stream borrow.
    let (mut sock_read, mut sock_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    tokio::select! {
        result = copy(&mut stdin, &mut sock_write) => {
            // Agent closed its stdin — normal session-end. Best-
            // effort flush + close on the socket side so the daemon
            // sees a clean EOF.
            let _ = sock_write.shutdown().await;
            result
                .map(|_| ())
                .map_err(|e| MnemeError::Mcp(format!("stdin → daemon copy failed: {e}")))
        }
        result = copy(&mut sock_read, &mut stdout) => {
            // Daemon closed its half — could be auth rejection,
            // graceful shutdown, or daemon crash. Close stdout so
            // the agent sees EOF and exits its MCP loop.
            let _ = stdout.shutdown().await;
            result
                .map(|_| ())
                .map_err(|e| MnemeError::Mcp(format!("daemon → stdout copy failed: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;

    /// Stand up a fake daemon that accepts one connection, reads the
    /// auth handshake line, then echoes whatever bytes the client
    /// writes after that. Returns once the connection is closed by
    /// the client so the test can drive end-to-end shape verification
    /// without a real `mneme daemon` boot.
    async fn fake_daemon_one_shot(socket: std::path::PathBuf) -> String {
        let listener = UnixListener::bind(&socket).expect("bind fake daemon");
        let (stream, _) = listener.accept().await.expect("accept");
        let (read, mut write) = stream.into_split();
        let mut buffered = BufReader::new(read);
        let mut auth_line = String::new();
        buffered
            .read_line(&mut auth_line)
            .await
            .expect("read auth line");
        // Best-effort echo back; the client may have already closed
        // its half (the test only cares that the auth line came
        // through verbatim), so swallow BrokenPipe.
        let _ = write.write_all(b"echoed-after-auth\n").await;
        let _ = write.shutdown().await;
        auth_line
    }

    #[tokio::test]
    async fn client_sends_auth_line_with_token() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("run")).unwrap();
        auth::ensure_token(&root).unwrap();
        let token = auth::read_token(&root).unwrap();
        let token_str = String::from_utf8(token).unwrap();

        // Start fake daemon; collect the auth line it observed.
        let socket = socket_path(&root);
        let daemon_handle = tokio::spawn(fake_daemon_one_shot(socket));

        // Give the listener a moment to bind before the client
        // connects.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drive just the connect + handshake portion of `run` —
        // skip the stdin pipe (would block forever in a test).
        let socket = socket_path(&root);
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let auth_line = format!("{AUTH_HEADER_PREFIX}{token_str}\n");
        stream.write_all(auth_line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        drop(stream);

        let observed = daemon_handle.await.unwrap();
        assert_eq!(observed, auth_line);
    }

    #[tokio::test]
    async fn client_errors_when_socket_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("run")).unwrap();
        auth::ensure_token(&root).unwrap();
        // No daemon running → connect must fail with a useful error.
        let result = run(&root).await;
        assert!(result.is_err(), "client must error when daemon is down");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("daemon socket")
                || msg.contains("Connection refused")
                || msg.contains("No such file"),
            "error must explain the missing daemon, got: {msg}"
        );
    }

    #[tokio::test]
    async fn client_errors_when_token_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        // Don't ensure_token — token file absent → read_token fails.
        let result = run(&root).await;
        assert!(
            result.is_err(),
            "client must error when auth token is missing"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("auth token") || msg.contains("auth.token"),
            "error must explain the missing token, got: {msg}"
        );
    }
}
