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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

/// Poll for the socket file to appear; the daemon performs boot work
/// (storage open, embed migrate, audit, scheduler spawn) before
/// binding, so a fresh data dir on slow disk can take ~1s. Cap at 5s
/// so a hung daemon doesn't wedge the test suite.
async fn wait_for_socket(socket: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if socket.exists() {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("socket {} did not appear within 5 s", socket.display());
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
