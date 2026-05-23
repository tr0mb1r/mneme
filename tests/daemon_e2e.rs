//! v1.1 daemon end-to-end tests covering A.M2 (single client) and
//! A.M3 (multi-client + SIGTERM-driven shutdown). Per
//! release-planning v2.1 §3.9 + ADR-0012, these are release gates
//! for the daemon-mode track.
//!
//! Linux + macOS only (cfg(unix)) — Windows named-pipe support is M4
//! per ADR-0012 D2/D9.

#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");

/// Send SIGTERM to the daemon process. M3's `DaemonServeMany` mode
/// runs forever until interrupted, so tests need an explicit kill.
fn sigterm(child: &Child) {
    let pid = child.id().expect("daemon has a PID") as i32;
    // SAFETY: PID is the child we just spawned; SIGTERM is the
    // signal the daemon's `shutdown_signal()` handler installs.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM, {pid}) must succeed");
}

/// Send SIGKILL to the process. Used when we need the process to
/// exit immediately without its graceful-drain wait loop.
fn sigkill(child: &Child) {
    let pid = child.id().expect("daemon has a PID") as i32;
    let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "kill(SIGKILL, {pid}) must succeed");
}

/// Read the auth token the daemon generated at boot. Per ADR-0012
/// D3 every connection must present this as the first line —
/// `MNEME-AUTH: <token>\n`. Tests use this helper to mimic what
/// `mneme run`'s spawn-and-connect (deferred A.M2 piece D12) will
/// do automatically once it lands.
fn read_auth_token(data_dir: &Path) -> String {
    let path = data_dir.join("run").join("auth.token");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "auth token at {} not found ({e}); daemon should have created it",
            path.display()
        )
    });
    String::from_utf8(bytes).expect("token is utf-8 (43 chars URL-safe base64)")
}

/// Build the canonical auth prefix line: `MNEME-AUTH: <token>\n`.
fn auth_line(data_dir: &Path) -> String {
    format!("MNEME-AUTH: {}\n", read_auth_token(data_dir))
}

/// Poll the daemon socket until a `UnixStream::connect` succeeds.
/// The daemon performs boot work (storage open, embed migrate,
/// audit, scheduler spawn) before binding, so a fresh data dir on
/// slow disk can take ~1s; CI ubuntu under load occasionally pushes
/// past that. Cap at 15s so a hung daemon doesn't wedge the suite.
///
/// We probe with `connect` rather than `socket.exists()` because the
/// file can be present-but-not-yet-accepting in two cases that
/// matter to the daemon-restart tests:
///
/// 1. A killed daemon leaves a stale socket file on disk (Drop
///    didn't run) — `exists()` is true the whole time the new
///    daemon is booting.
/// 2. Between `bind_listener` removing the stale socket and the
///    new `UnixListener::bind` returning, there's a brief window
///    where the path doesn't resolve to a working listener.
///
/// Both windows would let the caller race ahead and read 0 bytes
/// from the client (panic: "post-reconnect stats JSON: EOF while
/// parsing a value" at line 484).
async fn wait_for_socket(socket: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let probe_timeout = Duration::from_millis(250);
    while std::time::Instant::now() < deadline {
        if let Ok(Ok(_stream)) =
            tokio::time::timeout(probe_timeout, UnixStream::connect(socket)).await
        {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "socket {} did not accept connections within 15 s",
        socket.display()
    );
}

#[tokio::test]
async fn daemon_serves_one_client_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    // Spawn the daemon. stdin closed (Stdio::null) so the
    // legacy stdio fallback path doesn't accidentally serve the
    // initialize from there — we want to exercise the socket
    // transport.
    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mneme daemon");

    wait_for_socket(&socket).await;

    // Connect a client and run the standard MCP handshake. Per
    // ADR-0012 D3 every DaemonServeMany connection must present
    // the auth prefix as the first line — without it, the daemon
    // drops the connection (see `daemon_rejects_missing_token`).
    let mut stream = UnixStream::connect(&socket)
        .await
        .expect("client connect to daemon socket");
    stream
        .write_all(auth_line(data_dir).as_bytes())
        .await
        .unwrap();
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "daemon-e2e-test", "version": "1" },
        },
    })
    .to_string();
    stream.write_all(initialize.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    })
    .to_string();
    stream.write_all(initialized.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();

    // Read one frame back. Cap at 5 s so a stuck server doesn't
    // wedge CI. Both halves of the stream are scoped inside this
    // block so they drop on exit — connection fully closes, daemon
    // sees EOF on its read half, shuts down.
    let payload: serde_json::Value = {
        let (read, write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut response = String::new();
        timeout(Duration::from_secs(5), reader.read_line(&mut response))
            .await
            .expect("response within 5 s")
            .expect("read response line");
        // Drop the write half explicitly so the daemon's read half
        // EOFs while we're still in this block; otherwise `write`
        // would hold the connection open until end of test scope.
        drop(write);
        drop(reader);
        serde_json::from_str(response.trim()).expect("response is valid JSON")
    };
    assert_eq!(payload["jsonrpc"], "2.0");
    assert_eq!(payload["id"], 1);
    assert_eq!(payload["result"]["protocolVersion"], "2025-06-18");
    assert!(payload["result"]["capabilities"]["tools"].is_object());

    // M3 multi-client mode: the daemon does NOT exit on a single
    // client EOF — it loops on accept waiting for the next client.
    // Send SIGTERM and verify the shutdown_signal() handler exits
    // cleanly within 10 s.
    sigterm(&daemon);
    let exit_status = timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("daemon exited within 10 s of SIGTERM")
        .expect("daemon wait succeeded");
    assert!(
        exit_status.success(),
        "daemon exited non-zero on SIGTERM: {exit_status:?}"
    );

    // Post-exit: the listener's RAII unlink runs as the future is
    // dropped, so the socket file must be gone.
    assert!(
        !socket.exists(),
        "socket file {} should not exist after daemon exit",
        socket.display()
    );
}

/// Two clients connect concurrently, both run the initialize
/// handshake and get distinct correct responses, then close. The
/// daemon serves them in parallel via tokio::spawn (M3
/// DaemonServeMany). Then we SIGTERM and confirm clean exit.
#[tokio::test]
async fn daemon_serves_two_clients_concurrently() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme daemon");

    wait_for_socket(&socket).await;

    async fn one_handshake(
        socket: &Path,
        client_label: &str,
        auth_prefix: String,
    ) -> serde_json::Value {
        let mut stream = UnixStream::connect(socket)
            .await
            .unwrap_or_else(|_| panic!("client {client_label} connect"));
        stream.write_all(auth_prefix.as_bytes()).await.unwrap();
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": client_label, "version": "1" },
            },
        })
        .to_string();
        stream.write_all(init.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        })
        .to_string();
        stream.write_all(initialized.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        let (read, write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut response = String::new();
        timeout(Duration::from_secs(5), reader.read_line(&mut response))
            .await
            .unwrap_or_else(|_| panic!("client {client_label} response timeout"))
            .unwrap_or_else(|_| panic!("client {client_label} response read err"));
        drop(write);
        drop(reader);
        serde_json::from_str(response.trim())
            .unwrap_or_else(|_| panic!("client {client_label} response parse"))
    }

    // Run two handshakes concurrently against the same daemon.
    let auth = auth_line(data_dir);
    let (a, b) = tokio::join!(
        one_handshake(&socket, "client-A", auth.clone()),
        one_handshake(&socket, "client-B", auth.clone())
    );
    assert_eq!(a["jsonrpc"], "2.0");
    assert_eq!(a["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(b["jsonrpc"], "2.0");
    assert_eq!(b["result"]["protocolVersion"], "2025-06-18");

    // Daemon stays alive after both clients close — verify by
    // connecting a third client and authenticating. (Bounded so
    // we don't hang if the daemon shut down unexpectedly.)
    let mut third = timeout(Duration::from_secs(2), UnixStream::connect(&socket))
        .await
        .expect("third connect within 2 s")
        .expect("third connect ok");
    third.write_all(auth.as_bytes()).await.unwrap();
    drop(third);

    // Now SIGTERM and confirm clean shutdown.
    sigterm(&daemon);
    let exit_status = timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("daemon exited within 10 s of SIGTERM")
        .expect("daemon wait succeeded");
    assert!(exit_status.success(), "{exit_status:?}");
    assert!(!socket.exists(), "socket should be gone post-exit");
}

#[tokio::test]
async fn second_daemon_against_same_data_dir_refuses_to_start() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    // First daemon — let it boot + bind + idle on accept.
    let mut first = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first daemon");
    wait_for_socket(&socket).await;

    // Second daemon — must fail fast. The lockfile-held check
    // (`mneme run` semantics, inherited via execute_with_mode)
    // catches it before the listener bind even runs.
    let second = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    let output = timeout(Duration::from_secs(5), second)
        .await
        .expect("second daemon exits within 5 s")
        .expect("output ok");
    assert!(
        !output.status.success(),
        "second daemon should exit non-zero, got {:?}",
        output.status
    );

    // SIGTERM the first daemon so the test doesn't leak a
    // background process across runs.
    sigterm(&first);
    let _ = timeout(Duration::from_secs(10), first.wait()).await;
}

/// A client that transparently reconnects when the daemon restarts.
///
/// Sequence:
///   1. Start daemon A, connect client, verify MCP works.
///   2. Kill daemon A, start daemon B (same data dir).
///   3. Client auto-reconnects (with backoff) + replays the MCP
///      initialize handshake.
///   4. Send another MCP request — must get a valid response,
///      proving the reinitialize + re-pipe succeeded.
#[tokio::test]
async fn client_reconnects_after_daemon_restart() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon_a = Command::new(BINARY)
        .arg("daemon")
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon A");
    wait_for_socket(&socket).await;

    // Spawn `mneme client` as a subprocess with piped stdio.
    // Capture client stderr (with MNEME_LOG=warn so reconnect/reinit
    // failures surface) so when this test flakes on CI, the failure
    // dump in `cargo test --log-failed` includes the diagnostic info
    // needed to identify whether the client exited via reconnect
    // timeout, reinit failure, auth-token read error, or something
    // else. Without this, a panic on `read_line → EOF` gives no
    // signal about what the client subprocess was doing.
    let mut client = Command::new(BINARY)
        .arg("client")
        .env("MNEME_LOG", "warn")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mneme client");

    let mut client_stdin = client.stdin.take().expect("client stdin open");
    let mut client_stdout = BufReader::new(client.stdout.take().expect("client stdout open"));
    let client_stderr = client.stderr.take().expect("client stderr open");
    // Drain stderr in the background and print on test failure via a
    // task handle the panic-or-success path joins below.
    let stderr_drain = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut reader = BufReader::new(client_stderr);
        let _ = reader.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Step 1: send a full MCP initialize handshake through the client
    // and verify we get a valid response back.
    client_stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"reconnect-test","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
        )
        .await
        .unwrap();

    let mut response = String::new();
    timeout(
        Duration::from_secs(5),
        client_stdout.read_line(&mut response),
    )
    .await
    .expect("initialize response within 5s")
    .expect("read initialize response");
    let parsed: serde_json::Value =
        serde_json::from_str(response.trim()).expect("initialize response JSON");
    assert_eq!(parsed["id"], 1);
    assert!(parsed["result"]["protocolVersion"].is_string());
    assert!(parsed["result"]["capabilities"]["tools"].is_object());

    // Prove the pipe works end-to-end with a tools/call stats.
    client_stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"stats","arguments":{}}}
"#,
        )
        .await
        .unwrap();
    let mut response = String::new();
    timeout(
        Duration::from_secs(5),
        client_stdout.read_line(&mut response),
    )
    .await
    .expect("stats response within 5s")
    .expect("read stats response");
    let parsed: serde_json::Value =
        serde_json::from_str(response.trim()).expect("stats response JSON");
    assert_eq!(parsed["id"], 2);
    assert!(parsed["result"]["content"][0]["text"].is_string());

    // Step 2: kill daemon A with SIGKILL so it exits immediately
    // (SIGTERM would trigger the 30-second graceful-drain deadline
    // since the client is still connected).
    sigkill(&daemon_a);
    let _ = timeout(Duration::from_secs(5), daemon_a.wait())
        .await
        .expect("daemon A must exit within 5s of SIGKILL")
        .expect("daemon A wait ok");
    // Socket stays on disk (SIGKILL skips the Listener Drop), but
    // daemon B's `bind_listener` stale-socket cleanup will unlink it.

    // Start daemon B on the same data dir.
    let mut daemon_b = Command::new(BINARY)
        .arg("daemon")
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon B");
    wait_for_socket(&socket).await;

    // Give the client time to detect the disconnect, reconnect with
    // backoff, re-auth, and replay the MCP initialize handshake.
    // Bumped 1500ms → 3000ms after the same panic kept surfacing on
    // ubuntu CI even after wait_for_socket grew a connect probe — the
    // tighter budget was leaving no slack for boot variance on the
    // CI runner. Stays well under the read_line 5s timeout so a hung
    // client still fails loudly.
    sleep(Duration::from_millis(3000)).await;

    // Step 3: send another request. If the client reconnected
    // transparently, this should succeed.
    client_stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"stats","arguments":{}}}
"#,
        )
        .await
        .unwrap();
    let mut response = String::new();
    timeout(
        Duration::from_secs(5),
        client_stdout.read_line(&mut response),
    )
    .await
    .expect("post-reconnect stats response within 5s")
    .expect("read post-reconnect stats response");

    // Replace `.expect("post-reconnect stats JSON")` with a manual
    // failure path that closes the client and drains its stderr
    // before panicking. The previous flake gave a bare "EOF while
    // parsing a value" with no signal about why the client exited.
    let parsed = match serde_json::from_str::<serde_json::Value>(response.trim()) {
        Ok(p) => p,
        Err(e) => {
            drop(client_stdin);
            let _ = timeout(Duration::from_secs(2), client.wait()).await;
            let stderr = match timeout(Duration::from_secs(2), stderr_drain).await {
                Ok(Ok(s)) => s,
                Ok(Err(je)) => format!("<stderr task panicked: {je}>"),
                Err(_) => "<stderr drain timed out>".to_string(),
            };
            sigterm(&daemon_b);
            let _ = timeout(Duration::from_secs(10), daemon_b.wait()).await;
            panic!(
                "post-reconnect stats JSON failed to parse ({e}); response={:?}; client stderr was:\n{}",
                response, stderr
            );
        }
    };
    assert_eq!(parsed["id"], 3);
    assert!(
        parsed["result"]["content"][0]["text"].is_string(),
        "stats response after daemon restart must be valid, got: {parsed:?}"
    );

    // Cleanup.
    drop(client_stdin);
    let _ = client.wait().await;
    let _ = stderr_drain.await;
    sigterm(&daemon_b);
    let _ = timeout(Duration::from_secs(10), daemon_b.wait()).await;
}

/// A client that connects without sending the auth prefix gets
/// dropped — the daemon writes a JSON-RPC error frame and closes.
/// Validates the ADR-0012 D3 enforcement at the connection
/// boundary.
#[tokio::test]
async fn daemon_rejects_missing_auth_token() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme daemon");
    wait_for_socket(&socket).await;

    // Connect without sending the MNEME-AUTH prefix; instead jump
    // straight to MCP initialize — daemon should reject and drop.
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    let bad_first_line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
    stream.write_all(bad_first_line).await.unwrap();

    // Read the rejection frame (or EOF).
    let (read, _write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut response = String::new();
    let _ = timeout(Duration::from_secs(5), reader.read_line(&mut response)).await;
    let parsed: serde_json::Value = serde_json::from_str(response.trim()).expect("rejection JSON");
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert!(
        parsed["error"]["code"].is_number(),
        "rejection must carry a JSON-RPC error code, got {parsed:?}"
    );
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("MNEME-AUTH"),
        "rejection message must mention the missing prefix, got {parsed:?}"
    );
    drop(reader);

    sigterm(&daemon);
    let _ = timeout(Duration::from_secs(10), daemon.wait()).await;
}

/// A client that presents the wrong token gets dropped with a
/// `TokenMismatch` error. The daemon must NOT proceed to MCP
/// dispatch.
#[tokio::test]
async fn daemon_rejects_invalid_auth_token() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme daemon");
    wait_for_socket(&socket).await;

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    stream
        .write_all(b"MNEME-AUTH: deliberately-wrong-token\n")
        .await
        .unwrap();

    let (read, _write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut response = String::new();
    let _ = timeout(Duration::from_secs(5), reader.read_line(&mut response)).await;
    let parsed: serde_json::Value = serde_json::from_str(response.trim()).expect("rejection JSON");
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("does not match"),
        "rejection message must mention token mismatch, got {parsed:?}"
    );

    sigterm(&daemon);
    let _ = timeout(Duration::from_secs(10), daemon.wait()).await;
}

/// Token rotation mid-session preserves existing connections (per
/// ADR-0012 D3/D4) but takes effect for the NEXT new connection.
/// Pins the no-drop semantic that the `mneme auth rotate`
/// subcommand promises in its user-facing message.
///
/// Sequence:
///   1. Start daemon → token T1 generated.
///   2. Connect client A with T1, send initialize, capture
///      response. Don't close — leave it idle.
///   3. Run `mneme auth rotate` against the same data dir →
///      token becomes T2.
///   4. Connect client B with the OLD T1 → must be rejected
///      (TokenMismatch).
///   5. Connect client C with the NEW T2 → must be accepted +
///      get a normal initialize response.
///   6. Drop client A → daemon's accept loop continues unaffected.
///   7. SIGTERM, verify clean exit.
///
/// Together these prove: rotation doesn't break client A, the
/// new token is required immediately for new connections, the
/// daemon stays responsive throughout.
#[tokio::test]
async fn token_rotation_mid_session_preserves_existing_connection() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme daemon");
    wait_for_socket(&socket).await;

    // Step 2: Client A authenticates with T1 + sends initialize.
    let token_t1 = read_auth_token(data_dir);
    let mut client_a = UnixStream::connect(&socket).await.unwrap();
    client_a
        .write_all(format!("MNEME-AUTH: {token_t1}\n").as_bytes())
        .await
        .unwrap();
    client_a
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"client-A","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
        )
        .await
        .unwrap();
    let (a_read, a_write) = client_a.into_split();
    let mut a_reader = BufReader::new(a_read);
    let mut a_init_response = String::new();
    timeout(
        Duration::from_secs(5),
        a_reader.read_line(&mut a_init_response),
    )
    .await
    .expect("client A init response within 5s")
    .expect("client A read ok");
    assert!(
        a_init_response.contains("\"protocolVersion\""),
        "client A initialize must succeed pre-rotation, got: {a_init_response}"
    );

    // Step 3: Rotate the token via the CLI subcommand. Idle the
    // existing connection to avoid races between the rotate's
    // file write and a concurrent in-flight read.
    let rotate_status = Command::new(BINARY)
        .arg("auth")
        .arg("rotate")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("auth rotate spawn");
    assert!(rotate_status.success(), "auth rotate must succeed");

    let token_t2 = read_auth_token(data_dir);
    assert_ne!(token_t1, token_t2, "rotate must produce a different token");

    // Step 4: Client B presents the OLD T1 → must be rejected.
    let mut client_b = UnixStream::connect(&socket).await.unwrap();
    client_b
        .write_all(format!("MNEME-AUTH: {token_t1}\n").as_bytes())
        .await
        .unwrap();
    let (b_read, _b_write) = client_b.into_split();
    let mut b_reader = BufReader::new(b_read);
    let mut b_response = String::new();
    timeout(Duration::from_secs(5), b_reader.read_line(&mut b_response))
        .await
        .expect("client B rejection within 5s")
        .expect("client B read ok");
    let b_parsed: serde_json::Value =
        serde_json::from_str(b_response.trim()).expect("rejection JSON");
    assert!(
        b_parsed["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("does not match"),
        "client B (old token) must be rejected with mismatch, got {b_parsed:?}"
    );

    // Step 5: Client C presents the NEW T2 → must be accepted.
    let mut client_c = UnixStream::connect(&socket).await.unwrap();
    client_c
        .write_all(format!("MNEME-AUTH: {token_t2}\n").as_bytes())
        .await
        .unwrap();
    client_c
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"client-C","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
        )
        .await
        .unwrap();
    let (c_read, c_write) = client_c.into_split();
    let mut c_reader = BufReader::new(c_read);
    let mut c_init_response = String::new();
    timeout(
        Duration::from_secs(5),
        c_reader.read_line(&mut c_init_response),
    )
    .await
    .expect("client C init response within 5s")
    .expect("client C read ok");
    assert!(
        c_init_response.contains("\"protocolVersion\""),
        "client C (new token) must succeed, got: {c_init_response}"
    );

    // Drop A and C explicitly so the daemon sees their EOFs +
    // decrements active counter cleanly before SIGTERM.
    drop(a_write);
    drop(a_reader);
    drop(c_write);
    drop(c_reader);

    sigterm(&daemon);
    let exit_status = timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("daemon exited within 10s of SIGTERM")
        .expect("daemon wait succeeded");
    assert!(exit_status.success(), "{exit_status:?}");
}

/// D12 spawn protocol: the daemon can be auto-started by the client
/// on connect failure. This test validates the spawn mechanism that
/// `mneme client` uses — start daemon detached, wait for socket,
/// then connect — without pre-starting the daemon manually.
#[tokio::test]
async fn daemon_is_spawned_by_detach_primitive() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    // Don't manually start the daemon — simulate what the client does
    // (spawn_daemon_detached: daemon --foreground, null stdio, setsid).
    // We use the binary directly since integration tests can't call
    // the crate-internal current_exe pattern reliably.
    let daemon = Command::new(BINARY)
        .arg("daemon")
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme daemon via D12 primitive");

    // Don't wait on the child — it's detached (setsid in daemon.rs).
    // Dropping the handle means we can't SIGTERM it by PID, but the
    // daemon runs until killed externally. We'll find its PID from the
    // lockfile or via pkill.
    let daemon_pid = daemon.id().expect("daemon has a PID");
    drop(daemon);

    // Wait for socket with exponential backoff (same as client).
    wait_for_socket(&socket).await;

    // Connect with auth handshake and verify MCP works.
    let mut stream = UnixStream::connect(&socket)
        .await
        .expect("client connect to daemon socket");
    stream
        .write_all(auth_line(data_dir).as_bytes())
        .await
        .unwrap();
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "d12-spawn-test", "version": "1" },
        },
    })
    .to_string();
    stream.write_all(initialize.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    })
    .to_string();
    stream.write_all(initialized.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();

    let payload: serde_json::Value = {
        let (read, write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut response = String::new();
        timeout(Duration::from_secs(5), reader.read_line(&mut response))
            .await
            .expect("response within 5 s")
            .expect("read response line");
        drop(write);
        drop(reader);
        serde_json::from_str(response.trim()).expect("response is valid JSON")
    };
    assert_eq!(payload["jsonrpc"], "2.0");
    assert_eq!(payload["id"], 1);
    assert_eq!(payload["result"]["protocolVersion"], "2025-06-18");

    // Cleanup: SIGTERM the daemon spawned by the D12 primitive.
    let rc = unsafe { libc::kill(daemon_pid as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM, {daemon_pid}) must succeed");

    // Poll for the socket to disappear, proving the daemon exited.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !socket.exists() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(!socket.exists(), "socket must be removed after SIGTERM");
}

/// Regression: `mneme client` against an empty data dir must
/// auto-spawn the daemon AND deliver the agent's MCP initialize
/// response back over stdout before the spawn-wait budget expires.
///
/// Pre-fix history: ADR-0012 D12 capped [`super::SPAWN_WAIT_DEADLINE`]
/// at 5 s, which was tight enough that a cold daemon boot (bge-m3
/// model_loader's HuggingFace HEAD probe + HNSW snapshot replay +
/// first-boot upgrade audit) routinely exceeded it. The client then
/// returned `daemon did not start within 5 s` and exited. Claude
/// Code (and other MCP hosts) saw the subprocess die and required
/// the user to manually `/mcp` reconnect; the second attempt
/// succeeded only because the orphaned daemon was now up.
///
/// This test exercises the actual `mneme client` binary end-to-end
/// (no daemon pre-started), proving the auto-spawn → connect →
/// auth → byte-pipe path produces a valid initialize response on
/// stdout. It uses `MNEME_EMBEDDER=stub` to keep the test fast and
/// deterministic — the bug class we're guarding against is
/// "client gives up before daemon is ready," which is independent
/// of which embedder is loaded.
#[tokio::test]
async fn client_auto_spawns_daemon_on_first_connect() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");
    let lockfile = data_dir.join(".lock");

    // No daemon pre-started — `mneme client`'s D12 path must spawn
    // it. Pipe stdio so the test can act as the MCP host.
    let mut client = Command::new(BINARY)
        .arg("client")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme client");

    let mut client_stdin = client.stdin.take().expect("client stdin open");
    let mut client_stdout = BufReader::new(client.stdout.take().expect("client stdout open"));

    // Send the agent's MCP initialize as soon as the subprocess
    // exists. The bridge buffers it during the spawn-wait window
    // and forwards once connected.
    client_stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"d12-autospawn-test","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
        )
        .await
        .expect("write initialize to client stdin");

    // 30 s matches the new SPAWN_WAIT_DEADLINE so the test fails
    // (rather than hangs) if the budget regresses.
    let mut response = String::new();
    timeout(
        Duration::from_secs(30),
        client_stdout.read_line(&mut response),
    )
    .await
    .expect("initialize response within 30 s of mneme client spawn")
    .expect("read initialize response line");

    let parsed: serde_json::Value =
        serde_json::from_str(response.trim()).expect("initialize response is valid JSON");
    assert_eq!(parsed["jsonrpc"], "2.0", "response is JSON-RPC 2.0");
    assert_eq!(parsed["id"], 1, "response correlates with our request id");
    assert!(
        parsed["result"]["protocolVersion"].is_string(),
        "initialize result carries protocolVersion: {parsed:?}"
    );
    assert!(
        parsed["result"]["capabilities"]["tools"].is_object(),
        "initialize result advertises the tools capability: {parsed:?}"
    );

    // Cleanup: close the client's stdin so it exits, then SIGTERM
    // the auto-spawned daemon via the PID it wrote to the lockfile.
    drop(client_stdin);
    let _ = timeout(Duration::from_secs(5), client.wait()).await;

    let daemon_pid: i32 = std::fs::read_to_string(&lockfile)
        .expect("daemon wrote its PID to the lockfile")
        .trim()
        .parse()
        .expect("lockfile contains a numeric PID");
    let rc = unsafe { libc::kill(daemon_pid, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM, {daemon_pid}) must succeed");

    // Wait for the socket to disappear so the next test on the same
    // CI worker doesn't trip the stale-socket cleanup probe.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !socket.exists() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Per-connection isolation per ADR-0012 D8: a client crashing
/// mid-conversation does NOT affect other clients connected to
/// the same daemon. Each connection runs in its own tokio task;
/// dropping one task's transport must not poison the storage
/// seam or wedge the accept loop.
///
/// Sequence:
///   1. Start daemon.
///   2. Client A connects + authenticates + sends initialize.
///   3. Client A's stream is dropped abruptly without closing
///      the MCP session (simulates `kill -9` on the client side
///      or a network partition).
///   4. Client B connects + authenticates + sends initialize +
///      `tools/call stats` and gets a normal response. Proves the
///      daemon is unimpaired by client A's crash.
///   5. SIGTERM, verify clean exit.
#[tokio::test]
async fn client_crash_does_not_affect_other_clients() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        // `--foreground` keeps the spawned process attached so the
        // test's `Child` handle controls the actual daemon's
        // lifetime. Without this flag, ADR-0012 D9's self-detach
        // would have the parent exit immediately and leave the
        // daemon as an orphan the test has no handle to.
        .arg("--foreground")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme daemon");
    wait_for_socket(&socket).await;

    let auth = auth_line(data_dir);

    // Step 2: Client A — authenticate, send initialize, then
    // drop the stream abruptly without reading the response.
    {
        let mut client_a = UnixStream::connect(&socket).await.unwrap();
        client_a.write_all(auth.as_bytes()).await.unwrap();
        client_a
            .write_all(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"client-A-crash","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
            )
            .await
            .unwrap();
        // Step 3: Drop without `shutdown()` — simulates a
        // crashed client that vanished without closing cleanly.
        // The OS drops the fd; the daemon sees EOF on its read
        // half some indeterminate time later and tears the
        // connection task down.
        drop(client_a);
    }

    // Give the daemon a moment to process the dropped connection
    // (decrement active counter, log the disconnect). 200 ms is
    // generous on local IPC.
    sleep(Duration::from_millis(200)).await;

    // Step 4: Client B — full handshake + tools/call stats. The
    // response proves the daemon is unimpaired.
    let mut client_b = UnixStream::connect(&socket).await.unwrap();
    client_b.write_all(auth.as_bytes()).await.unwrap();
    client_b
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"client-B-survivor","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"stats","arguments":{}}}
"#,
        )
        .await
        .unwrap();

    let (b_read, b_write) = client_b.into_split();
    let mut b_reader = BufReader::new(b_read);

    // Read the initialize response (id=1).
    let mut line = String::new();
    timeout(Duration::from_secs(5), b_reader.read_line(&mut line))
        .await
        .expect("client B initialize within 5s")
        .expect("client B read ok");
    let init: serde_json::Value = serde_json::from_str(line.trim()).expect("init JSON");
    assert_eq!(init["id"], 1);
    assert!(init["result"]["protocolVersion"].is_string());

    // Read the stats response (id=2).
    let mut line = String::new();
    timeout(Duration::from_secs(5), b_reader.read_line(&mut line))
        .await
        .expect("client B stats within 5s")
        .expect("client B stats read ok");
    let stats: serde_json::Value = serde_json::from_str(line.trim()).expect("stats JSON");
    assert_eq!(stats["id"], 2);
    assert!(
        stats["result"]["content"][0]["text"].is_string(),
        "stats response shape unchanged after client A crash, got {stats:?}"
    );

    drop(b_write);
    drop(b_reader);

    sigterm(&daemon);
    let exit_status = timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("daemon exited within 10s of SIGTERM")
        .expect("daemon wait succeeded");
    assert!(
        exit_status.success(),
        "daemon must exit cleanly post-client-crash: {exit_status:?}"
    );
}

/// Regression: with a healthy idle client connected (sitting in
/// `read_frame.await`), SIGTERM must drain promptly via the
/// active-drain broadcast — not wait the full `DRAIN_DEADLINE`
/// (30 s) for an EOF that never comes. Before the broadcast wiring,
/// this test would time out at ~30+ s; after, the daemon exits in
/// well under 5 s because the per-connection task observes the
/// `watch::changed()` notification, drops the socket halves, and the
/// drain counter falls to zero.
///
/// Also asserts log invariants that catch a class of `ConnectionGuard`
/// lifetime regressions (where the guard's `Drop` would fire
/// synchronously around `tokio::spawn` instead of at task exit,
/// leaving the counter perpetually at zero):
///   - "client accepted" with `active=1`
///   - "shutdown signal received" with `active=1` (counter still
///     reflects the connected client at drain time)
///   - "client disconnected" with `active=0` (guard's Drop ran
///     INSIDE the spawned task, not in the accept-arm scope)
#[tokio::test]
async fn daemon_drains_idle_clients_promptly_on_sigterm() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let socket = data_dir.join("run").join("mneme.sock");

    let mut daemon = Command::new(BINARY)
        .arg("daemon")
        .arg("--foreground")
        // Enable INFO logging on stderr so the test can assert on
        // the active-counter values surfaced by the daemon's
        // "client accepted active=N" / "shutdown signal received
        // active=N" tracing lines. NO_COLOR disables ANSI escape
        // codes (per the no-color.org convention, honored by
        // tracing-subscriber) so substring matches don't have to
        // navigate `active\x1b[2m=\x1b[0m1`.
        .env("MNEME_LOG", "info,mneme=info")
        .env("NO_COLOR", "1")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mneme daemon");
    wait_for_socket(&socket).await;

    let auth = auth_line(data_dir);

    // Connect a client and complete the handshake so it's accounted
    // for in `active_clients`. Then do nothing — the client sits in
    // its read loop while the server sits in its read loop. This is
    // exactly the dogfood scenario (a Claude Code MCP bridge holding
    // a connection idle between user prompts).
    let mut client = UnixStream::connect(&socket).await.unwrap();
    client.write_all(auth.as_bytes()).await.unwrap();
    client
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"idle-drain-regression","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
        )
        .await
        .unwrap();

    // Drain the initialize response so the daemon-side write
    // completes cleanly before we SIGTERM; otherwise the abort
    // could land mid-write and obscure the drain-time signal we
    // care about.
    let (read, write) = client.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("initialize response within 5 s")
        .expect("initialize read ok");
    let init: serde_json::Value = serde_json::from_str(line.trim()).expect("init JSON");
    assert_eq!(init["id"], 1);

    let started = std::time::Instant::now();
    sigterm(&daemon);

    // Capture stderr concurrently with the wait so we don't deadlock
    // on a full pipe buffer for the long-lived "INFO" stream.
    let stderr = daemon.stderr.take().expect("daemon stderr was piped");
    let stderr_buf = tokio::spawn(async move {
        let mut out = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut out).await;
        out
    });

    let exit_status = timeout(Duration::from_secs(5), daemon.wait())
        .await
        .expect("daemon must exit within 5 s of SIGTERM (active-drain broadcast)")
        .expect("daemon wait succeeded");
    let elapsed = started.elapsed();
    let log = stderr_buf.await.expect("stderr collector finished");

    assert!(
        exit_status.success(),
        "daemon exited non-zero on SIGTERM: {exit_status:?}\nlog:\n{log}"
    );
    // Generous upper bound — the broadcast path completes in
    // ~tens of ms locally, but CI can be slow. Anything under 5 s
    // proves we're not on the 30 s deadline path.
    assert!(
        elapsed < Duration::from_secs(5),
        "drain took {elapsed:?}; expected <5 s with active-drain broadcast\nlog:\n{log}"
    );
    assert!(
        !socket.exists(),
        "socket file {} should be unlinked after daemon exit\nlog:\n{log}",
        socket.display()
    );

    // ConnectionGuard lifetime invariants — these fail if the guard
    // is created in the accept arm but the spawned task doesn't own
    // it (synchronous create+drop would leave active=0 throughout).
    let saw_accepted_with_one = log
        .lines()
        .any(|l| l.contains("client accepted") && l.contains("active=1"));
    assert!(
        saw_accepted_with_one,
        "expected a 'client accepted active=1' line; ConnectionGuard \
         may be created at the wrong scope.\nlog:\n{log}"
    );
    let saw_shutdown_with_one = log
        .lines()
        .any(|l| l.contains("shutdown signal received") && l.contains("active=1"));
    assert!(
        saw_shutdown_with_one,
        "expected 'shutdown signal received active=1' (counter must \
         still reflect the connected client at drain time, NOT \
         active=0).\nlog:\n{log}"
    );
    let saw_disconnect_with_zero = log
        .lines()
        .any(|l| l.contains("client disconnected") && l.contains("active=0"));
    assert!(
        saw_disconnect_with_zero,
        "expected 'client disconnected active=0' from the guard's \
         Drop running inside the spawned task.\nlog:\n{log}"
    );

    drop(write);
    drop(reader);
}
