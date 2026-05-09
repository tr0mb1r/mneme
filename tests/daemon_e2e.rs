//! v1.1 daemon end-to-end test (release-planning §3.9 M2 release gate).
//!
//! Spawns `mneme daemon` as a subprocess, connects via Unix domain
//! socket, runs the standard MCP initialize handshake, asserts the
//! response shape, closes the connection, and confirms the daemon
//! exits cleanly. This is the ADR-0012 "single client works
//! end-to-end" criterion that gates A.M2 from M3 work.
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
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");

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

    // Daemon should exit on its own once the client closes. Bound
    // wait so a hung shutdown is loud rather than silent.
    let exit_status = timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("daemon exited within 10 s")
        .expect("daemon wait succeeded");
    assert!(
        exit_status.success(),
        "daemon exited non-zero: {exit_status:?}"
    );

    // Post-exit: socket file must be gone (RAII unlink fires when
    // the listener was dropped pre-serve, but we verify here as a
    // belt-and-suspenders check).
    assert!(
        !socket.exists(),
        "socket file {} should not exist after daemon exit",
        socket.display()
    );
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

    // Connect a real client to the first daemon so its accept
    // returns and it shuts down — keeps the test from leaking a
    // process across runs.
    let stream = UnixStream::connect(&socket).await.expect("connect to first");
    drop(stream);
    let _ = timeout(Duration::from_secs(10), first.wait()).await;
}
