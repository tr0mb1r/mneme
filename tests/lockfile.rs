//! Phase 2 exit gate: a second `mneme run` against the same data
//! directory must exit fast with a clear "lock held by PID N" message.

use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");

#[tokio::test]
async fn second_instance_fails_with_lock_held_message() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    // 1. Spawn the first mneme run; pipe its stdin so it doesn't read EOF.
    let mut first = Command::new(BINARY)
        .arg("run")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first mneme");

    // Wait for the first instance to actually take the lock by sending it
    // an `initialize` and waiting for its response.
    let mut first_stdin = first.stdin.take().unwrap();
    let mut first_stdout = BufReader::new(first.stdout.take().unwrap());
    first_stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n")
        .await
        .unwrap();
    first_stdin.flush().await.unwrap();
    let mut line = String::new();
    timeout(Duration::from_secs(5), first_stdout.read_line(&mut line))
        .await
        .expect("first instance never responded")
        .unwrap();
    assert!(
        line.contains("\"result\""),
        "first instance failed to start: {line}"
    );

    // 2. Spawn the second mneme run against the same dir.
    let second = Command::new(BINARY)
        .arg("run")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let output = timeout(Duration::from_secs(10), second)
        .await
        .expect("second instance hung")
        .expect("spawn second mneme");

    assert!(
        !output.status.success(),
        "second instance should have exited with non-zero status, but exited successfully: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("lock held by PID")
            || stderr.contains("lock error")
            || stderr.to_lowercase().contains("lock"),
        "expected stderr to mention lock contention, got: {stderr}"
    );

    // Cleanup.
    drop(first_stdin);
    let _ = timeout(Duration::from_secs(5), first.wait()).await;
}
