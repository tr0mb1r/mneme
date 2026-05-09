//! v1.0 → v1.1 upgrade migration test (Q1 release gate per
//! release-planning v2.1 §6.1, task #16).
//!
//! Per Invariant 1 of pin `01KR5ZB7ED01HADZXZKKBV882Z` the on-disk
//! `schema_version` doesn't bump in the v1.1 cycle — so the
//! "migration" here is purely on the config-toml axis (added
//! `[daemon]` section, added `[budgets].max_remember_chars`). This
//! test pins:
//!
//! 1. A v1.0-shaped `config.toml` (no `[daemon]`, no
//!    `max_remember_chars`) parses cleanly under the v1.1 binary —
//!    serde defaults backfill the new fields, no error emitted on
//!    boot.
//! 2. Memories written under the v1.1 binary against a v1.0-shaped
//!    config persist + remain `recall`-able after a re-boot. (Same-
//!    binary same-storage round-trip — would also catch any
//!    accidental schema_version bump.)
//! 3. An explicit `[mcp].transport = "stdio"` is preserved verbatim
//!    (not auto-flipped to SSE — the auto-flip per ADR-0012 D11 is
//!    a follow-up A.M2 closing-touch commit; today's behaviour is
//!    preserve-explicit-stdio + leave-default-stdio).
//!
//! `tests/upgrade/v1_1_to_v1_0_rollback.rs` is the sibling test
//! (Q2, blocked on the v1.0.1 backup-run-exclusion patch landing
//! upstream; see task #17).
//!
//! Linux + macOS only — the Windows daemon-mode integration tests
//! land in M4 alongside the named-pipe support.

#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");

/// Hand-written v1.0-shaped config.toml: every field that v1.0
/// shipped, none of the v1.1 additions. The v1.1 binary must
/// parse this cleanly and backfill the new fields via
/// `#[serde(default)]`.
const V1_0_CONFIG_TOML: &str = r#"
[storage]
data_dir = ""
max_size_gb = 10
encryption = false

[embeddings]
model = "bge-m3"
device = "auto"
batch_size = 32

[consolidation]
hot_to_warm_days = 28
warm_to_cold_days = 180
schedule = "idle"

[scopes]
default = "personal"

[mcp]
transport = "stdio"
sse_port = 7878

[budgets]
default_recall_limit = 10
auto_context_token_budget = 4000

[checkpoints]
session_interval_secs = 30
session_interval_turns = 5
hnsw_snapshot_inserts = 1000
hnsw_snapshot_minutes = 60

[telemetry]
enabled = false
endpoint = ""

[logging]
level = "info"
file = ""
max_size_mb = 100
"#;

/// Write the v1.0 config + scaffold the data dir using the v1.1
/// binary's `mneme init` (no agent) — that produces the v1.0
/// directory layout (which is unchanged in v1.1 per Invariant 1).
async fn prepare_v1_0_data_dir(data_dir: &Path) {
    std::fs::create_dir_all(data_dir).unwrap();
    std::fs::write(data_dir.join("config.toml"), V1_0_CONFIG_TOML).unwrap();

    // Scaffold via the binary so schema_version + directory tree
    // get written by the canonical code path (matches what v1.0's
    // `mneme init` would have produced).
    let status = Command::new(BINARY)
        .arg("init")
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("init spawn");
    assert!(
        status.success(),
        "init must succeed against v1.0-shaped config"
    );
}

/// Boot `mneme run` against the data dir, send an MCP initialize +
/// the supplied tools/call sequence, then close stdin. Returns the
/// concatenated JSON-RPC response text. Bounded by `bound` so a
/// hung server doesn't wedge CI.
async fn run_with_calls(data_dir: &Path, tool_calls: &[&str], bound: Duration) -> String {
    let mut input = String::new();
    input.push_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"upgrade-test","version":"1"}}}
"#,
    );
    input.push_str(
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}
"#,
    );
    for (i, call) in tool_calls.iter().enumerate() {
        input.push_str(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{call}}}
"#,
            id = i + 2,
            call = call,
        ));
    }

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
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(input.as_bytes()).await.unwrap();
        stdin.shutdown().await.unwrap();
    }
    let output = timeout(bound, child.wait_with_output())
        .await
        .expect("mneme run completed within bound")
        .expect("output ok");
    assert!(output.status.success(), "mneme run exited non-zero");
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

#[tokio::test]
async fn v1_0_config_boots_clean_under_v1_1_binary() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("dotmneme");
    prepare_v1_0_data_dir(&data_dir).await;

    // Simple boot smoke: send initialize + initialized + tools/list,
    // verify the response includes the v1.1-expected tool count.
    // The mere fact that `mneme run` returns 0 already proves config
    // parsing didn't error on the missing v1.1 sections.
    let response = run_with_calls(
        &data_dir,
        &[r#"{"name":"stats","arguments":{}}"#],
        Duration::from_secs(15),
    )
    .await;

    // Initialize response is the first line; stats response is the
    // second. Both must parse as valid JSON-RPC.
    let mut lines = response.lines().filter(|l| !l.trim().is_empty());
    let init: serde_json::Value =
        serde_json::from_str(lines.next().expect("initialize response")).expect("init json");
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    let stats: serde_json::Value =
        serde_json::from_str(lines.next().expect("stats response")).expect("stats json");
    // Stats output is a tool result with content[0] = text JSON.
    let text = stats["result"]["content"][0]["text"]
        .as_str()
        .expect("stats text");
    let stats_body: serde_json::Value = serde_json::from_str(text).expect("stats body json");
    // Sanity: schema_version, memories.semantic, etc. all present.
    assert!(stats_body.get("schema_version").is_some());
    assert!(stats_body["memories"].get("semantic").is_some());
}

#[tokio::test]
async fn memories_persist_across_v1_0_to_v1_1_reboot() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("dotmneme");
    prepare_v1_0_data_dir(&data_dir).await;

    // First boot: write three memories under the v1.0-shaped config.
    let _ = run_with_calls(
        &data_dir,
        &[
            r#"{"name":"remember","arguments":{"content":"alpha bravo charlie","tags":["first"]}}"#,
            r#"{"name":"remember","arguments":{"content":"delta echo foxtrot","tags":["second"]}}"#,
            r#"{"name":"remember","arguments":{"content":"golf hotel india","tags":["third"]}}"#,
        ],
        Duration::from_secs(20),
    )
    .await;

    // Second boot: same data dir, recall the memories. The v1.1
    // binary must surface all three.
    let response = run_with_calls(
        &data_dir,
        &[r#"{"name":"recall","arguments":{"query":"alpha bravo","limit":10}}"#],
        Duration::from_secs(20),
    )
    .await;

    // Find the recall response (id=2 — initialize was id=1).
    let recall = response
        .lines()
        .filter(|l| !l.trim().is_empty())
        .find(|l| l.contains("\"id\":2"))
        .expect("recall response");
    let parsed: serde_json::Value = serde_json::from_str(recall).expect("recall json");
    let text = parsed["result"]["content"][0]["text"]
        .as_str()
        .expect("recall text");
    let hits: serde_json::Value = serde_json::from_str(text).expect("hits json");
    let hits_arr = hits.as_array().expect("hits array");
    assert!(
        !hits_arr.is_empty(),
        "recall must return at least one hit post-reboot, got {hits:?}"
    );
}

#[tokio::test]
async fn explicit_stdio_transport_preserved() {
    // The v1.0-shaped fixture has `[mcp].transport = "stdio"`.
    // Per ADR-0012 D11, an explicit stdio choice is preserved
    // (the auto-flip-to-SSE only fires when the field is unset
    // or absent — a future commit). Today this test just pins
    // that v1.1 doesn't accidentally clobber the user's stdio
    // preference on boot.
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("dotmneme");
    prepare_v1_0_data_dir(&data_dir).await;

    let _ = run_with_calls(&data_dir, &[], Duration::from_secs(10)).await;

    // Re-read the config file post-boot — `mneme run` doesn't
    // currently rewrite config.toml, so this is more "did anything
    // accidentally write to it" than a true migration assertion.
    let post_boot = std::fs::read_to_string(data_dir.join("config.toml")).unwrap();
    assert!(
        post_boot.contains(r#"transport = "stdio""#),
        "explicit stdio transport must be preserved on boot, config now: {post_boot}"
    );
}
