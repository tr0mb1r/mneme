//! End-to-end MCP conformance test.
//!
//! Spawns the built `mneme` binary as a subprocess, drives it as an
//! MCP client over stdio, and asserts the protocol surface required
//! by the Phase 1 exit gate:
//!   - initialize / notifications/initialized handshake
//!   - tools/list returns three named tools
//!   - tools/call works for each of remember / recall / forget
//!   - resources/list returns mneme://stats
//!   - resources/read mneme://stats returns valid JSON
//!   - server exits cleanly when stdin is closed
//!
//! We use `cargo`'s `CARGO_BIN_EXE_<name>` env var so the test
//! always runs against the freshly compiled binary.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn `mneme run` against an isolated temp `MNEME_DATA_DIR`, so each
/// test owns its own ~/.mneme tree (and its own lockfile).
///
/// `MNEME_EMBEDDER=stub` keeps these tests offline-friendly — without
/// it the binary would try to download BGE-M3 (1.2 GB) from Hugging
/// Face, which both blows up CI runtime and flakes when the network
/// hiccups. The stub embedder is documented in `src/embed/stub.rs`.
fn spawn_isolated() -> (tokio::process::Child, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let child = Command::new(BINARY)
        .arg("run")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", tmp.path())
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mneme run");
    (child, tmp)
}

struct Client {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    line_buf: String,
}

impl Client {
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
}

#[tokio::test]
async fn full_mcp_handshake_and_tool_calls() {
    let (mut child, _tmp) = spawn_isolated();

    let mut client = Client {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        line_buf: String::new(),
    };

    // 1. initialize
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0.0.1" }
            }
        }))
        .await;
    let init = client.recv().await;
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "mneme");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["capabilities"]["resources"].is_object());

    // 2. notifications/initialized (no response)
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await;

    // 3. tools/list → three tools
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }))
        .await;
    let tools = client.recv().await;
    let tool_names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // Phase 6 + switch_scope (v0.15) + record_event (v0.2.4): 13 tools.
    assert_eq!(tool_names.len(), 13);
    for expected in [
        "remember",
        "recall",
        "forget",
        "update",
        "pin",
        "unpin",
        "recall_recent",
        "summarize_session",
        "stats",
        "list_scopes",
        "export",
        "switch_scope",
        "record_event",
    ] {
        assert!(tool_names.contains(&expected), "missing tool {expected:?}");
    }
    // Every tool has a description and inputSchema.
    for tool in tools["result"]["tools"].as_array().unwrap() {
        assert!(tool["description"].as_str().is_some());
        assert!(tool["inputSchema"].is_object());
    }

    // 4. tools/call remember
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "remember",
                "arguments": { "content": "the build is green" }
            }
        }))
        .await;
    let rem = client.recv().await;
    let text = rem["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("stored memory "));

    // 5. tools/call recall
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": { "query": "build" }
            }
        }))
        .await;
    let rec = client.recv().await;
    assert_eq!(rec["id"], 4);
    assert!(rec["result"]["content"][0]["text"].is_string());

    // 6. tools/call forget — pass a syntactically valid (but unknown)
    // ULID; the tool returns "no such memory" rather than erroring.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "forget",
                "arguments": { "id": "01H0000000000000000000000Z" }
            }
        }))
        .await;
    let f = client.recv().await;
    assert_eq!(f["id"], 5);
    assert!(f.get("error").is_none() || f["error"].is_null());
    let f_text = f["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        f_text.starts_with("no such memory"),
        "unexpected forget output: {f_text}"
    );

    // 7. resources/list
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "resources/list"
        }))
        .await;
    let rl = client.recv().await;
    let resources = rl["result"]["resources"].as_array().unwrap();
    // Phase 5 surface + L1 read-side fold-in: context, procedural,
    // recent, stats, session template.
    assert_eq!(resources.len(), 5);
    let uris: Vec<&str> = resources
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    for expected in [
        "mneme://stats",
        "mneme://procedural",
        "mneme://recent",
        "mneme://context",
        "mneme://session/{id}",
    ] {
        assert!(uris.contains(&expected), "missing resource {expected:?}");
    }

    // 8. resources/read
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "resources/read",
            "params": { "uri": "mneme://stats" }
        }))
        .await;
    let rr = client.recv().await;
    let contents = rr["result"]["contents"].as_array().unwrap();
    assert_eq!(contents[0]["mimeType"], "application/json");
    let body: Value = serde_json::from_str(contents[0]["text"].as_str().unwrap()).unwrap();
    // Phase 6: stats now reports real counts. Just assert the
    // shape; the values depend on whatever this test session wrote.
    assert!(body["schema_version"].is_number());
    assert!(body["memories"]["semantic"].is_number());
    assert!(body["memories"]["procedural"].is_number());
    assert!(body["memories"]["episodic"]["hot"].is_number());
    assert!(body["semantic_index"]["embed_dim"].is_number());

    // 9. close stdin → server exits cleanly within 2s
    drop(client.stdin);
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("server did not exit after stdin close")
        .unwrap();
    assert!(status.success(), "server exited with {status:?}");
}

#[tokio::test]
async fn malformed_json_returns_parse_error_then_continues() {
    let (mut child, _tmp) = spawn_isolated();

    let mut client = Client {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        line_buf: String::new(),
    };

    // Send garbage, then a valid initialize.
    client.stdin.write_all(b"{not json\n").await.unwrap();
    client.stdin.flush().await.unwrap();
    let parse_err = client.recv().await;
    assert_eq!(parse_err["error"]["code"], -32700);

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }))
        .await;
    let init = client.recv().await;
    assert_eq!(init["result"]["serverInfo"]["name"], "mneme");

    drop(client.stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await.unwrap();
}

#[tokio::test]
async fn pre_initialize_request_returns_not_initialized() {
    let (mut child, _tmp) = spawn_isolated();

    let mut client = Client {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        line_buf: String::new(),
    };

    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        }))
        .await;
    let resp = client.recv().await;
    assert_eq!(resp["error"]["code"], -32002);

    drop(client.stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await.unwrap();
}
