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

use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::daemon::auth::{self, AUTH_HEADER_PREFIX};
use crate::daemon::socket_path;
use crate::daemon::wait_for_socket;
use crate::storage::layout;
use crate::{MnemeError, Result};

/// MCP protocol version advertised during synthetic reinitialize.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

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

/// True for connect errors that indicate no daemon is listening:
/// the socket file is missing (ENOENT) or present but no one is
/// accepting connections (ECONNREFUSED). In both cases D12's
/// auto-spawn protocol should fire.
fn is_connect_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Total time `mneme client` will wait for an auto-spawned daemon's
/// socket to appear before giving up. ADR-0012 D12's original 5 s
/// budget did not account for the bge-m3 model_loader's
/// HuggingFace HEAD/GET probe (~1–4 s even with weights cached
/// locally), the HNSW snapshot replay (~50–500 ms), or the
/// first-boot upgrade audit. A warm-cache `mneme daemon` boot
/// routinely exceeded 5 s in production, causing the client to exit
/// before the daemon finished booting. The agent's MCP host then
/// saw the subprocess die and forced the user into a manual `/mcp`
/// reconnect. 30 s is well above the warm-cache worst-case (~5 s
/// with the model already on disk) and tight enough to surface real
/// failures (lockfile contention, OOM at boot, etc.) instead of
/// hanging forever. First-ever boots that have to download the
/// embedder weights from HuggingFace still exceed this budget; that
/// scenario surfaces a clear `daemon did not start within 30s`
/// error rather than the silent 5 s exit, and the next agent
/// `mneme client` invocation reconnects to the daemon once the
/// download completes.
const SPAWN_WAIT_DEADLINE: Duration = Duration::from_secs(30);

/// Spawn a detached daemon and wait for its socket to appear.
/// Exponential backoff: 10 ms initial, 2× per attempt, 1 s cap,
/// total budget [`SPAWN_WAIT_DEADLINE`] per ADR-0012 D12.
async fn spawn_daemon_with_backoff(socket: &Path) -> Result<()> {
    crate::daemon::spawn_daemon_detached()?;
    wait_for_socket(socket, SPAWN_WAIT_DEADLINE)
        .await
        .map_err(|e| {
            MnemeError::Mcp(format!(
                "daemon did not start within {:?}: {e}",
                SPAWN_WAIT_DEADLINE
            ))
        })
}

/// Build an auth-line string `MNEME-AUTH: <token>\n` from raw token bytes.
fn format_auth_line(token: &[u8]) -> Result<String> {
    let token_str = std::str::from_utf8(token)
        .map_err(|e| MnemeError::Config(format!("auth token is not utf-8: {e}")))?;
    let mut line = String::with_capacity(AUTH_HEADER_PREFIX.len() + token_str.len() + 1);
    line.push_str(AUTH_HEADER_PREFIX);
    line.push_str(token_str);
    line.push('\n');
    Ok(line)
}

/// Establish the initial connection (with D12 auto-spawn).
async fn initial_connect(socket: &Path) -> Result<UnixStream> {
    match UnixStream::connect(socket).await {
        Ok(s) => Ok(s),
        Err(e) if is_connect_error(&e) => {
            spawn_daemon_with_backoff(socket).await?;
            UnixStream::connect(socket).await.map_err(|e| {
                MnemeError::Mcp(format!(
                    "could not connect to daemon socket at {} after spawning daemon: {e} \
                     (check ~/.mneme/logs/mneme.log for daemon boot errors)",
                    socket.display()
                ))
            })
        }
        Err(e) => Err(MnemeError::Mcp(format!(
            "could not connect to daemon socket at {}: {e} \
             (is `mneme daemon` running?)",
            socket.display()
        ))),
    }
}

/// Write the `MNEME-AUTH: <token>\n` handshake line to a connected stream.
async fn send_auth_line(stream: &mut UnixStream, auth_line: &str) -> Result<()> {
    stream
        .write_all(auth_line.as_bytes())
        .await
        .map_err(MnemeError::Io)?;
    stream.flush().await.map_err(MnemeError::Io)
}

/// Send synthetic MCP initialize + initialized to a fresh daemon connection
/// so the agent's subsequent MCP frames are not rejected with `-32002`
/// ("server not initialized"). This is required because the daemon enforces
/// the handshake order (server.rs:313-317).
///
/// Reads the initialize response, checks that it is one, and then
/// sends the `notifications/initialized` notification. A reply that is
/// not a result for our request (the daemon's auth rejection frame, for
/// one) is `InvalidData`: retrying cannot help, so the caller gives up.
async fn reinitialize(stream: &mut UnixStream) -> io::Result<()> {
    let init_req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{MCP_PROTOCOL_VERSION}","capabilities":{{}},"clientInfo":{{"name":"mneme-client","version":"1.0"}}}}}}"#,
    );
    stream.write_all(init_req.as_bytes()).await?;
    stream.write_all(b"\n").await?;

    // Read exactly one newline-terminated line (the initialize response).
    // We read byte-by-byte so we don't accidentally consume bytes from the
    // daemon's next response in the same kernel buffer.
    let mut buf = [0u8; 4096];
    let mut n = 0;
    loop {
        if n >= buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "initialize response exceeded 4 KiB",
            ));
        }
        let b = match stream.read(&mut buf[n..n + 1]).await {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "daemon closed during reinitialize",
                ));
            }
            Ok(_) => buf[n],
            Err(e) => return Err(e),
        };
        n += 1;
        if b == b'\n' {
            break;
        }
    }

    let reply: serde_json::Value = serde_json::from_slice(&buf[..n]).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("initialize reply is not JSON: {e}"),
        )
    })?;
    if reply.get("result").is_none() {
        let message = reply["error"]["message"]
            .as_str()
            .unwrap_or("no result in the initialize reply");
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon refused the session: {message}"),
        ));
    }

    // Send the initialized notification.
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}")
        .await?;
    stream.write_all(b"\n").await?;
    stream.flush().await
}

/// Background task that reads stdin in a loop and forwards bytes over an
/// unbounded channel. Sends an empty Vec on EOF to signal the main loop.
async fn read_stdin_task(tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = match stdin.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            let _ = tx.send(vec![]);
            return;
        }
        if tx.send(buf[..n].to_vec()).is_err() {
            return;
        }
    }
}

async fn run(root: &Path) -> Result<()> {
    let socket = socket_path(root);

    // D12: initial connect with auto-spawn.
    let mut stream = initial_connect(&socket).await?;

    let token = auth::read_token(root).map_err(|e| {
        MnemeError::Config(format!(
            "could not read daemon auth token at {}: {e} \
             (run `mneme daemon` once to generate it, or `mneme auth rotate`)",
            auth::token_path(root).display()
        ))
    })?;

    let auth_line = format_auth_line(&token)?;
    send_auth_line(&mut stream, &auth_line).await?;

    // Spawn a background task that relays stdin bytes over a channel so
    // the main loop can keep reading them even during the reconnect window.
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel();
    tokio::spawn(read_stdin_task(stdin_tx));

    let mut stdout = tokio::io::stdout();
    let mut sock_buf = vec![0u8; 8192];

    loop {
        tokio::select! {
            result = stream.read(&mut sock_buf) => {
                match result {
                    Ok(n) if n > 0 => {
                        stdout.write_all(&sock_buf[..n]).await.map_err(|e| {
                            MnemeError::Mcp(format!(
                                "failed to write daemon response to stdout: {e}"
                            ))
                        })?;
                    }
                    // EOF: the daemon closed the connection. Get a
                    // new session, transparently.
                    Ok(_) => {
                        if let Err(e) = reestablish(&socket, root, &mut stream).await {
                            // Give up — close stdout so the agent sees
                            // EOF and knows the MCP server is gone.
                            let _ = stdout.shutdown().await;
                            return Err(e);
                        }
                    }
                    // The reset a killed daemon leaves behind when it
                    // had not read everything we sent: same thing.
                    Err(e) if is_disconnect(&e) => {
                        if let Err(e) = reestablish(&socket, root, &mut stream).await {
                            let _ = stdout.shutdown().await;
                            return Err(e);
                        }
                    }
                    Err(e) => {
                        return Err(MnemeError::Mcp(format!(
                            "daemon → stdout read error: {e}"
                        )));
                    }
                }
            }
            msg = stdin_rx.recv() => {
                match msg {
                    Some(data) if data.is_empty() => {
                        // Agent closed stdin — normal session end.
                        let _ = stream.shutdown().await;
                        return Ok(());
                    }
                    Some(data) => {
                        match stream.write_all(&data).await {
                            Ok(()) => {}
                            // The agent wrote before we had read the
                            // EOF of a daemon that is already gone.
                            // Reconnect and send the bytes to the new
                            // daemon instead of dying on EPIPE.
                            Err(e) if is_disconnect(&e) => {
                                if let Err(e) = reestablish(&socket, root, &mut stream).await {
                                    let _ = stdout.shutdown().await;
                                    return Err(e);
                                }
                                stream.write_all(&data).await.map_err(|e| {
                                    MnemeError::Mcp(format!(
                                        "failed to write stdin to daemon after reconnect: {e}"
                                    ))
                                })?;
                            }
                            Err(e) => {
                                return Err(MnemeError::Mcp(format!(
                                    "failed to write stdin to daemon: {e}"
                                )));
                            }
                        }
                    }
                    None => {
                        // stdin reader task exited.
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// True for the errors a connection gives when its daemon is gone or
/// going: reset, broken pipe, aborted, or EOF in the middle of a reply.
fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotConnected
    )
}

/// Replace a dead connection with a working MCP session: connect,
/// authenticate, replay `initialize`. Retries with backoff until
/// [`SPAWN_WAIT_DEADLINE`].
///
/// A successful `connect` does not mean a daemon will serve us. A Unix
/// socket connects as soon as the listener has room in its backlog,
/// before anyone calls `accept`, and a daemon that is being killed
/// closes its client connections and its listener at slightly different
/// moments. A client that sees the EOF first can connect to the dying
/// daemon's listener; the connection is reset when that listener closes
/// a moment later. So the session only counts once the daemon has
/// answered `initialize`, and a disconnect before that is the same
/// "not up yet" as ECONNREFUSED. (This was the cause of the
/// `client_reconnects_after_daemon_restart` flake: the client exited
/// with "reinitialize failed: connection reset" before the replacement
/// daemon had even been started.)
async fn reestablish(socket: &Path, root: &Path, stream: &mut UnixStream) -> Result<()> {
    reestablish_within(socket, root, stream, SPAWN_WAIT_DEADLINE).await
}

async fn reestablish_within(
    socket: &Path,
    root: &Path,
    stream: &mut UnixStream,
    deadline: Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    let mut delay = Duration::from_millis(10);
    let max_delay = Duration::from_secs(1);
    let timed_out = || {
        MnemeError::Mcp(format!(
            "daemon did not restart within {deadline:?} after disconnect"
        ))
    };

    loop {
        let remaining = deadline
            .checked_sub(start.elapsed())
            .ok_or_else(timed_out)?;
        // Connect + auth; waits out ECONNREFUSED / ENOENT itself.
        let mut fresh = connect_and_auth(socket, root, remaining).await?;
        // Bounded: a listener that never accepts would otherwise hold
        // us here for ever.
        let left = deadline.saturating_sub(start.elapsed());
        match tokio::time::timeout(left, reinitialize(&mut fresh)).await {
            Ok(Ok(())) => {
                *stream = fresh;
                return Ok(());
            }
            // Not a daemon that will serve us (see above). Try again.
            Ok(Err(e)) if is_disconnect(&e) => {}
            Ok(Err(e)) => {
                return Err(MnemeError::Mcp(format!(
                    "MCP reinitialize failed after reconnect: {e}"
                )));
            }
            Err(_) => return Err(timed_out()),
        }
        if start.elapsed() >= deadline {
            return Err(timed_out());
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}

/// Connect to the daemon socket and send the auth line, waiting with
/// exponential backoff while nothing is listening.
///
/// The usual budget is [`SPAWN_WAIT_DEADLINE`] (30 s) on purpose: the
/// EOF we're recovering from is usually the daemon being restarted
/// (`mneme stop` + `mneme daemon`), and the replacement's boot is
/// the same warm-cache scenario the spawn path documents (~5 s
/// typical, more under model-load or first-boot upgrade audit). A
/// tighter budget here would have the client exit while the new
/// daemon is still binding, forcing the MCP host into a manual
/// `/mcp` reconnect — exactly what D12's reconnect protocol exists
/// to avoid.
async fn connect_and_auth(socket: &Path, root: &Path, deadline: Duration) -> Result<UnixStream> {
    let mut delay = Duration::from_millis(10);
    let max_delay = Duration::from_secs(1);
    let start = std::time::Instant::now();

    loop {
        match UnixStream::connect(socket).await {
            Ok(mut s) => {
                let token = auth::read_token(root).map_err(|e| {
                    MnemeError::Config(format!("could not read auth token during reconnect: {e}"))
                })?;
                let auth_line = format_auth_line(&token)?;
                match send_auth_line(&mut s, &auth_line).await {
                    Ok(()) => return Ok(s),
                    // The listener went away under the connection.
                    Err(MnemeError::Io(e)) if is_disconnect(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) if is_connect_error(&e) => {}
            Err(e) => {
                return Err(MnemeError::Mcp(format!("reconnect error: {e}")));
            }
        }
        if start.elapsed() >= deadline {
            return Err(MnemeError::Mcp(format!(
                "daemon did not restart within {deadline:?} after disconnect"
            )));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
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

    #[test]
    fn is_connect_error_matches_not_found() {
        assert!(is_connect_error(&io::Error::new(
            io::ErrorKind::NotFound,
            "no file"
        )));
    }

    #[test]
    fn is_connect_error_matches_connection_refused() {
        assert!(is_connect_error(&io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "refused"
        )));
    }

    #[test]
    fn is_connect_error_does_not_match_other_errors() {
        assert!(!is_connect_error(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "perms"
        )));
        assert!(!is_connect_error(&io::Error::new(
            io::ErrorKind::TimedOut,
            "timeout"
        )));
        assert!(!is_connect_error(&io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted"
        )));
    }

    #[tokio::test]
    async fn client_errors_when_token_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Plant a live listener so connect succeeds, then let the
        // token read fail — this exercises the post-connect error path.
        let socket = socket_path(&root);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let _listener = UnixListener::bind(&socket).unwrap();

        // Don't ensure_token — token file absent → read_token fails
        // after the connect succeeds.
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

    /// A fake daemon connection: read the auth line and `initialize`,
    /// answer it, read `notifications/initialized`, then echo one line.
    async fn serve_one_session(stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let auth = lines.next_line().await.unwrap().unwrap();
        assert!(auth.starts_with(AUTH_HEADER_PREFIX.trim_end()), "{auth}");
        let init = lines.next_line().await.unwrap().unwrap();
        assert!(init.contains("\"initialize\""), "{init}");
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
            .await
            .unwrap();
        let note = lines.next_line().await.unwrap().unwrap();
        assert!(note.contains("notifications/initialized"), "{note}");
        if let Ok(Some(line)) = lines.next_line().await {
            let _ = write.write_all(format!("echo:{line}\n").as_bytes()).await;
        }
    }

    fn root_with_token() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("run")).unwrap();
        auth::ensure_token(&root).unwrap();
        (tmp, root)
    }

    /// The race behind the `client_reconnects_after_daemon_restart`
    /// flake, made deterministic. The old daemon's listener is still
    /// open when the client reconnects, never accepts, and then closes:
    /// the client's connection is reset in the middle of `initialize`.
    /// That must be retried, not reported as a failed session.
    #[tokio::test]
    async fn reestablish_survives_a_listener_that_dies_under_the_connection() {
        let (_tmp, root) = root_with_token();
        let socket = socket_path(&root);

        let dying = UnixListener::bind(&socket).unwrap();
        // Any connected stream will do as the "dead" one to replace.
        let (mut stream, _peer) = UnixStream::pair().unwrap();

        let daemons = {
            let socket = socket.clone();
            tokio::spawn(async move {
                // Long enough for the client to connect, authenticate
                // and block reading the initialize reply.
                tokio::time::sleep(Duration::from_millis(150)).await;
                drop(dying); // the pending connection is reset
                std::fs::remove_file(&socket).unwrap();
                // Nothing listens for a while (ENOENT for the client).
                tokio::time::sleep(Duration::from_millis(150)).await;
                let replacement = UnixListener::bind(&socket).unwrap();
                let (conn, _) = replacement.accept().await.unwrap();
                serve_one_session(conn).await;
            })
        };

        reestablish_within(&socket, &root, &mut stream, Duration::from_secs(10))
            .await
            .expect("the client must end up on the replacement daemon");

        // The stream it handed back is the initialized session.
        stream.write_all(b"ping\n").await.unwrap();
        let mut reply = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "echo:ping\n");
        daemons.await.unwrap();
    }

    /// The kernel behaviour the fix rests on: `connect` succeeds against
    /// a listener nobody accepts on, and closing that listener ends the
    /// connection with a reset (or EOF), not with a daemon.
    #[tokio::test]
    async fn unaccepted_connection_dies_when_the_listener_closes() {
        let tmp = TempDir::new().unwrap();
        let socket = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut stream = UnixStream::connect(&socket)
            .await
            .expect("connects without accept");
        stream.write_all(b"hello\n").await.unwrap();
        drop(listener);
        let mut buf = [0u8; 1];
        match stream.read(&mut buf).await {
            Ok(0) => {}
            Ok(n) => panic!("read {n} bytes from nobody"),
            Err(e) => assert!(is_disconnect(&e), "{e}"),
        }
    }

    /// An auth rejection is answered with an error frame. It must not
    /// be taken for the initialize reply, and must not be retried.
    #[tokio::test]
    async fn reestablish_gives_up_when_the_daemon_refuses_the_session() {
        let (_tmp, root) = root_with_token();
        let socket = socket_path(&root);
        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            conn.write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32001,\"message\":\"token mismatch\"}}\n",
            )
            .await
            .unwrap();
            // Keep the connection open until the client has read it.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let err = reestablish_within(&socket, &root, &mut stream, Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("token mismatch"), "{err}");
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn reestablish_times_out_when_no_daemon_comes_back() {
        let (_tmp, root) = root_with_token();
        let socket = socket_path(&root);
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        let err = reestablish_within(&socket, &root, &mut stream, Duration::from_millis(300))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not restart"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
