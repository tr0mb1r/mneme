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

    // Connect a client and run the standard MCP handshake.
    let mut stream = UnixStream::connect(&socket)
        .await
        .expect("client connect to daemon socket");
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

    async fn one_handshake(socket: &Path, client_label: &str) -> serde_json::Value {
        let mut stream = UnixStream::connect(socket)
            .await
            .unwrap_or_else(|_| panic!("client {client_label} connect"));
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
    let (a, b) = tokio::join!(
        one_handshake(&socket, "client-A"),
        one_handshake(&socket, "client-B")
    );
    assert_eq!(a["jsonrpc"], "2.0");
    assert_eq!(a["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(b["jsonrpc"], "2.0");
    assert_eq!(b["result"]["protocolVersion"], "2025-06-18");

    // Daemon stays alive after both clients close — verify by
    // connecting a third client. (Bounded so we don't hang if the
    // daemon shut down unexpectedly.)
    let third = timeout(Duration::from_secs(2), UnixStream::connect(&socket))
        .await
        .expect("third connect within 2 s")
        .expect("third connect ok");
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
