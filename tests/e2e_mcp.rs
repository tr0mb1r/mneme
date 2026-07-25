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

/// v1.3 auto-context surface, end to end against the real binary.
///
/// Three things this pins that unit tests cannot:
///   1. `resources/templates/list` is answered at all (it was
///      `method not found` before v1.3, so `mneme://session/{id}` was
///      undiscoverable to a spec-compliant client).
///   2. A parameterised `mneme://context?q=…` read routes through the
///      registry's prefix fallback rather than 404-ing.
///   3. A memory written with `remember` actually surfaces in that
///      read — the end of the chain the docs promised and the code
///      did not deliver.
#[tokio::test]
async fn auto_context_folds_in_remembered_memories() {
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
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0.0.1" }
            }
        }))
        .await;
    let _ = client.recv().await;
    client
        .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;

    // resources/templates/list must advertise both templates.
    client
        .send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/templates/list" }))
        .await;
    let templates = client.recv().await;
    let listed: Vec<&str> = templates["result"]["resourceTemplates"]
        .as_array()
        .expect("resourceTemplates array")
        .iter()
        .map(|t| t["uriTemplate"].as_str().unwrap())
        .collect();
    assert!(
        listed.contains(&"mneme://session/{id}"),
        "session template missing: {listed:?}"
    );
    assert!(
        listed.contains(&"mneme://context{?q,scope,limit}"),
        "context template missing: {listed:?}"
    );

    // Write a memory.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "remember",
                "arguments": { "content": "we deploy with flyctl, never docker push" }
            }
        }))
        .await;
    let remembered = client.recv().await;
    assert_eq!(
        remembered["result"]["isError"], false,
        "remember failed: {remembered}"
    );

    // A bare context read must leave semantic empty…
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "resources/read",
            "params": { "uri": "mneme://context" }
        }))
        .await;
    let bare = client.recv().await;
    let bare_body: Value =
        serde_json::from_str(bare["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        bare_body["semantic"].as_array().unwrap().is_empty(),
        "a query-less read has nothing to be similar to"
    );
    assert!(
        bare_body.get("working").is_some(),
        "the L1 section must be emitted"
    );

    // …and a seeded read must surface the memory.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "resources/read",
            "params": { "uri": "mneme://context?q=how%20do%20we%20deploy" }
        }))
        .await;
    let seeded = client.recv().await;
    assert_eq!(
        seeded["result"]["contents"][0]["uri"], "mneme://context?q=how%20do%20we%20deploy",
        "the reply must echo the requested URI so clients can correlate"
    );
    let seeded_body: Value =
        serde_json::from_str(seeded["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    let hits = seeded_body["semantic"].as_array().unwrap();
    assert_eq!(
        hits.len(),
        1,
        "auto-context dropped the memory: {seeded_body}"
    );
    assert_eq!(
        hits[0]["content"],
        "we deploy with flyctl, never docker push"
    );
    assert!(hits[0]["similarity"].is_number());

    drop(client.stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await.unwrap();
}

/// `recall` gained a `tags` filter and a `min_similarity` floor in
/// v1.3. Both are wire-visible, so pin them here.
#[tokio::test]
async fn recall_honours_tags_and_similarity_floor() {
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
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0.0.1" }
            }
        }))
        .await;
    let _ = client.recv().await;
    client
        .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;

    for (id, content, tags) in [
        (10, "postgres runs on port 5432", vec!["db", "prod"]),
        (11, "postgres runs on port 5433 in staging", vec!["db"]),
    ] {
        client
            .send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "remember",
                    "arguments": { "content": content, "tags": tags }
                }
            }))
            .await;
        let r = client.recv().await;
        assert_eq!(r["result"]["isError"], false, "remember failed: {r}");
    }

    // Both tags required ⇒ one hit.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": { "query": "postgres port", "tags": ["db", "prod"] }
            }
        }))
        .await;
    let resp = client.recv().await;
    let rows: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "tag filter ignored: {rows}"
    );
    assert!(
        rows[0]["similarity"].is_number(),
        "missing similarity field"
    );

    // An unreachable similarity floor filters everything out.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": { "query": "postgres port", "min_similarity": 1.0 }
            }
        }))
        .await;
    let resp = client.recv().await;
    let rows: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        rows.as_array().unwrap().len() <= 1,
        "similarity floor of 1.0 should admit only an exact match: {rows}"
    );

    // Out-of-range floor is a clean argument error, not a panic.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": { "query": "postgres port", "min_similarity": 5.0 }
            }
        }))
        .await;
    let resp = client.recv().await;
    assert!(
        resp["error"].is_object() || resp["result"]["isError"] == true,
        "out-of-range min_similarity should be rejected: {resp}"
    );

    drop(client.stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await.unwrap();
}
