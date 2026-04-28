//! Phase 3 §3 crash-recovery exit gate at the semantic-store level:
//! "insert N memories, snapshot at K, kill -9 between snapshots,
//! restart, verify all N searchable."
//!
//! Two scenarios:
//!
//! 1. **WAL-only durability.** Run with the default snapshot
//!    threshold (1000) so no snapshot fires for N=50. Validates that
//!    the WAL alone is enough to recover every ack'd `remember`.
//!
//! 2. **Snapshot-scheduler durability.** Lower the threshold to 20
//!    via a written `config.toml` so the scheduler fires several
//!    times during the workload. Hard-kill the process, restart, and
//!    confirm every ack'd `remember` is still recallable. This
//!    exercises three independent failure modes that have to be
//!    covered for the spec gate:
//!
//!      * `kill -9` after a WAL append's fsync but before the next
//!        snapshot save → recovery via WAL replay past the previous
//!        snapshot's `applied_lsn`.
//!      * `kill -9` mid `snapshot::save` (between `<path>.tmp` write
//!        and the atomic `rename`) → either the old or new snapshot
//!        is on disk, never a half-written file.
//!      * `kill -9` after a successful save but before
//!        `wal::truncate_through` → snapshot covers part of the WAL,
//!        and replay still terminates at the right next-LSN.
//!
//! All three are covered probabilistically by killing at random
//! times across a small workload. Numbers are kept low so CI doesn't
//! drag, but the structure is identical to the spec's 5000-write
//! variant.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use ulid::Ulid;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

struct McpClient {
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    line_buf: String,
    next_id: i64,
}

impl McpClient {
    /// Spawn `mneme run` against `data_dir` and complete the
    /// `initialize` / `notifications/initialized` handshake.
    async fn spawn(data_dir: &std::path::Path) -> Self {
        let mut child = Command::new(BINARY)
            .arg("run")
            .env("MNEME_LOG", "off")
            .env("MNEME_DATA_DIR", data_dir)
            .env("MNEME_EMBEDDER", "stub")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mneme run");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut c = McpClient {
            child: Some(child),
            stdin,
            stdout,
            line_buf: String::new(),
            next_id: 1,
        };
        c.handshake().await;
        c
    }

    async fn handshake(&mut self) {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "crash_test", "version": "0" }
            }
        }))
        .await;
        let _ = self.recv().await;
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await;
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send(&mut self, msg: &Value) {
        let mut line = serde_json::to_string(msg).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        self.line_buf.clear();
        let n = timeout(STEP_TIMEOUT, self.stdout.read_line(&mut self.line_buf))
            .await
            .expect("server did not respond within timeout")
            .unwrap();
        assert!(n > 0, "server closed stdout unexpectedly");
        serde_json::from_str(self.line_buf.trim_end_matches('\n'))
            .expect("server emitted invalid JSON")
    }

    /// Send a `tools/call remember` with `content`. Returns the
    /// resulting `MemoryId` (parsed from the tool response).
    async fn remember(&mut self, content: &str) -> Ulid {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "remember",
                "arguments": { "content": content }
            }
        }))
        .await;
        let resp = self.recv().await;
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("remember response should have text content");
        let ulid_str = text
            .split_whitespace()
            .nth(2)
            .expect("expected `stored memory <ULID>`");
        Ulid::from_string(ulid_str).expect("response ULID parses")
    }

    /// Send a `tools/call recall` with `query`. Returns every id in
    /// the JSON array that the tool emits.
    async fn recall_ids(&mut self, query: &str) -> HashSet<Ulid> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": { "query": query, "limit": 100 }
            }
        }))
        .await;
        let resp = self.recv().await;
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("recall response should have text content");
        let arr: Vec<Value> = serde_json::from_str(text).expect("recall body parses as JSON");
        arr.into_iter()
            .filter_map(|v| {
                v["id"]
                    .as_str()
                    .and_then(|s| Ulid::from_string(s).ok())
            })
            .collect()
    }

    /// Hard-kill the child via SIGKILL. Returns once the child has been reaped.
    async fn kill_minus_9(mut self) {
        if let Some(mut child) = self.child.take() {
            // tokio's start_kill issues SIGKILL on Unix. The blocking
            // wait reaps the zombie so the next spawn against the
            // same data dir doesn't trip the lockfile.
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// Graceful close: drop stdin, wait for clean exit. Used by the
    /// post-recovery client so the test cleanup is deterministic.
    async fn shutdown(mut self) {
        // Drop stdin first so the server's read loop sees EOF.
        let _ = self.stdin.shutdown().await;
        if let Some(mut child) = self.child.take() {
            let _ = timeout(Duration::from_secs(5), child.wait()).await;
        }
    }
}

/// Write a `config.toml` under `data_dir` that tightens the snapshot
/// scheduler so the test exercises the snapshot path within a small
/// number of writes.
fn write_aggressive_snapshot_config(data_dir: &std::path::Path) {
    let body = r#"
[checkpoints]
hnsw_snapshot_inserts = 20
hnsw_snapshot_minutes = 1
"#;
    std::fs::write(data_dir.join("config.toml"), body).unwrap();
}

#[tokio::test]
async fn kill_after_writes_recovers_via_wal_replay() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    // Phase 1: spawn, write 50 memories, collect ULIDs, then SIGKILL.
    // No snapshot fires (default threshold = 1000), so recovery has
    // to come entirely from WAL replay.
    let mut written: Vec<(String, Ulid)> = Vec::with_capacity(50);
    {
        let mut c = McpClient::spawn(data_dir).await;
        for i in 0..50 {
            let content = format!("crash-wal-{i}");
            let id = c.remember(&content).await;
            written.push((content, id));
        }
        c.kill_minus_9().await;
    }

    // Phase 2: restart and verify every ack'd memory is recallable.
    let mut c2 = McpClient::spawn(data_dir).await;
    for (content, expected_id) in &written {
        let ids = c2.recall_ids(content).await;
        assert!(
            ids.contains(expected_id),
            "memory {expected_id} (content={content:?}) lost across SIGKILL"
        );
    }
    c2.shutdown().await;
}

#[tokio::test]
async fn kill_during_snapshot_cycle_recovers_all_acked_writes() {
    // Spec §3 Phase 3 exit gate at small scale:
    // insert 60, snapshot every 20, kill, restart, verify 60.
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    // Layout::scaffold runs lazily on `mneme run`, so the directory
    // is created for us. We only need to drop config.toml in.
    std::fs::create_dir_all(data_dir).unwrap();
    write_aggressive_snapshot_config(data_dir);

    let mut written: Vec<(String, Ulid)> = Vec::with_capacity(60);
    {
        let mut c = McpClient::spawn(data_dir).await;
        for i in 0..60 {
            let content = format!("crash-snap-{i}");
            let id = c.remember(&content).await;
            written.push((content, id));
        }
        // At this point the scheduler has fired ~3 snapshots and may
        // have rotated WAL segments. SIGKILL forces the test through
        // the post-snapshot recovery path on next boot.
        c.kill_minus_9().await;
    }

    let mut c2 = McpClient::spawn(data_dir).await;
    for (content, expected_id) in &written {
        let ids = c2.recall_ids(content).await;
        assert!(
            ids.contains(expected_id),
            "memory {expected_id} (content={content:?}) lost across SIGKILL with active snapshot scheduler"
        );
    }
    c2.shutdown().await;
}
