//! End-to-end tests for the Hermes context-reload hook
//! (`templates/hermes/mneme-context.py`): a real `mneme` server, the real
//! script, and the JSON payloads Hermes pipes to a `pre_llm_call` shell
//! hook.
//!
//! Skipped (with a note) when `python3` isn't on the PATH.

#![cfg(unix)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");
const TOKEN: &str = "hook-test-token";
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/hermes/mneme-context.py"
);

fn python() -> Option<&'static str> {
    Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "python3")
}

struct Server {
    child: std::process::Child,
    addr: SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Minimal blocking HTTP/1.1 POST, one request per connection.
fn post(addr: SocketAddr, body: &Value, session: Option<&str>) -> (Option<String>, Value) {
    use std::io::Read;
    let payload = serde_json::to_vec(body).unwrap();
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        payload.len()
    );
    if let Some(s) = session {
        req.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    req.push_str("\r\n");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(&payload).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let sid = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
        })
        .map(|(_, v)| v.trim().to_owned());
    let body = &raw[split + 4..];
    let v = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body).unwrap_or(Value::Null)
    };
    (sid, v)
}

fn start_serve(data: &Path) -> Server {
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let mut child = Command::new(BINARY)
        .args(["serve", "--bind", &addr.to_string()])
        .env("MNEME_DATA_DIR", data)
        .env("MNEME_EMBEDDER", "stub")
        .env("MNEME_HTTP_TOKEN", TOKEN)
        .env("MNEME_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return Server { child, addr };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("mneme serve did not start");
}

/// Pin a rule and remember a fact through MCP.
fn seed(addr: SocketAddr) {
    let (sid, _) = post(
        addr,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        None,
    );
    let sid = sid.unwrap();
    post(
        addr,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        Some(&sid),
    );
    for (i, (name, args)) in [
        (
            "pin",
            json!({"content": "Always answer with numbers first."}),
        ),
        (
            "remember",
            json!({"content": "QQQ3 positions use a 6% trailing stop-loss."}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (_, r) = post(
            addr,
            &json!({"jsonrpc": "2.0", "id": 10 + i, "method": "tools/call",
                    "params": {"name": name, "arguments": args}}),
            Some(&sid),
        );
        assert!(r["result"].is_object(), "{name} failed: {r}");
    }
}

struct HookRun {
    stdout: Value,
    stderr: String,
}

fn run_hook(python: &str, args: &[String], hermes_home: &Path, payload: &Value) -> HookRun {
    let mut child = Command::new(python)
        .arg(SCRIPT)
        .args(args)
        .env("HERMES_HOME", hermes_home)
        .env_remove("MNEME_HTTP_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "hook must always exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    HookRun {
        stdout: serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("hook stdout not JSON ({e}): {text}")),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn payload(history: Vec<Value>, first: bool, message: &str) -> Value {
    json!({
        "hook_event_name": "pre_llm_call",
        "session_id": "sess-1",
        "extra": {
            "conversation_history": history,
            "is_first_turn": first,
            "user_message": message,
        }
    })
}

fn msg(role: &str, content: &str) -> Value {
    json!({"role": role, "content": content})
}

fn context_of(run: &HookRun) -> String {
    run.stdout["context"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an injection, got {} ({})", run.stdout, run.stderr))
        .to_owned()
}

fn http_args(addr: SocketAddr) -> Vec<String> {
    vec![
        "--url".into(),
        format!("http://{addr}/mcp"),
        "--token-env".into(),
        "MNEME_HTTP_TOKEN".into(),
    ]
}

#[test]
fn hook_reloads_context_at_session_start_and_after_compression_only() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    // The token reaches the hook through $HERMES_HOME/.env, the path
    // that survives Hermes' secret scrubbing on multiplexed gateways.
    std::fs::write(
        hermes.path().join(".env"),
        format!("OTHER=1\nMNEME_HTTP_TOKEN=\"{TOKEN}\"\n"),
    )
    .unwrap();
    let server = start_serve(data.path());
    seed(server.addr);
    let args = http_args(server.addr);

    // 1. First turn: inject pinned rules + relevant memories.
    let first = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(vec![], true, "what stop-loss do we use for QQQ3?"),
    );
    let ctx = context_of(&first);
    assert!(ctx.starts_with("[mneme] Session start."), "{ctx}");
    assert!(ctx.contains("Always answer with numbers first."), "{ctx}");
    assert!(ctx.contains("6% trailing stop-loss"), "{ctx}");

    // 2. An ordinary turn: nothing.
    let history = vec![msg("user", "hi"), msg("assistant", "hello")];
    let quiet = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(history.clone(), false, "next"),
    );
    assert_eq!(quiet.stdout, json!({}), "{}", quiet.stderr);

    // 3. Hermes compressed: a compaction summary appears.
    let mut compressed = vec![msg(
        "user",
        "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted into the summary below.",
    )];
    compressed.extend(history.clone());
    let after = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(compressed.clone(), false, "continue"),
    );
    let ctx = context_of(&after);
    assert!(ctx.contains("just compressed"), "{ctx}");
    assert!(ctx.contains("Always answer with numbers first."), "{ctx}");

    // 4. Same summary next turn: already reloaded, nothing.
    let mut longer = compressed.clone();
    longer.push(msg("assistant", "ok"));
    let quiet = run_hook(py, &args, hermes.path(), &payload(longer, false, "more"));
    assert_eq!(quiet.stdout, json!({}));

    // 5. History shrank without a recognisable summary (wording
    //    changed in some Hermes version): still treated as compression.
    let shrunk = vec![msg("user", "summary in some other words")];
    let again = run_hook(py, &args, hermes.path(), &payload(shrunk, false, "go on"));
    assert!(context_of(&again).contains("just compressed"));
}

#[test]
fn hook_fails_open_with_a_reason() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    std::fs::write(hermes.path().join(".env"), "MNEME_HTTP_TOKEN=wrong\n").unwrap();
    let server = start_serve(data.path());
    let run = run_hook(
        py,
        &http_args(server.addr),
        hermes.path(),
        &payload(vec![], true, "hello"),
    );
    assert_eq!(run.stdout, json!({}));
    assert!(run.stderr.contains("mneme hook:"), "{}", run.stderr);

    // Unreachable server: same.
    drop(server);
    let run = run_hook(
        py,
        &["--url".into(), "http://127.0.0.1:9/mcp".into()],
        hermes.path(),
        &payload(vec![], true, "hello"),
    );
    assert_eq!(run.stdout, json!({}));
}

#[test]
fn hook_speaks_stdio_through_mneme_client() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    // Seed through a throwaway HTTP server, then hand the store to a
    // daemon (both take the same lock, so one at a time).
    {
        let server = start_serve(data.path());
        seed(server.addr);
    }
    let mut daemon = Command::new(BINARY)
        .args(["daemon", "--foreground"])
        .env("MNEME_DATA_DIR", data.path())
        .env("MNEME_EMBEDDER", "stub")
        .env("MNEME_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket: PathBuf = data.path().join("run").join("mneme.sock");
    for _ in 0..300 {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // `mneme client`, spawned by the hook, inherits MNEME_DATA_DIR.
    let args = ["--command", BINARY, "--arg", "client"];
    let mut child = Command::new(py)
        .arg(SCRIPT)
        .args(args)
        .env("HERMES_HOME", hermes.path())
        .env("MNEME_DATA_DIR", data.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload(vec![], true, "stop-loss?").to_string().as_bytes())
        .unwrap();
    let run = child.wait_with_output().unwrap();
    let _ = daemon.kill();
    let _ = daemon.wait();

    let out: Value = serde_json::from_slice(&run.stdout).unwrap();
    let ctx = out["context"].as_str().unwrap_or_else(|| {
        panic!(
            "no injection: {out} / {}",
            String::from_utf8_lossy(&run.stderr)
        )
    });
    assert!(ctx.contains("Always answer with numbers first."), "{ctx}");
}
