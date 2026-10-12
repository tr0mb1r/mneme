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
    start_serve_at(data, addr)
}

fn start_serve_at(data: &Path, addr: SocketAddr) -> Server {
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

/// Open an MCP session over HTTP and return its id.
fn open_session(addr: SocketAddr) -> String {
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
    sid
}

/// One `tools/call` in its own session; returns the whole JSON-RPC reply.
fn call_tool(addr: SocketAddr, name: &str, args: Value) -> Value {
    let sid = open_session(addr);
    let (_, r) = post(
        addr,
        &json!({"jsonrpc": "2.0", "id": 20, "method": "tools/call",
                "params": {"name": name, "arguments": args}}),
        Some(&sid),
    );
    r
}

/// The `summary` events mneme holds, oldest last; each `payload` is
/// parsed from the stored JSON string.
fn summaries_from(reply: &Value) -> Vec<Value> {
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("recall_recent failed: {reply}"));
    let events: Vec<Value> = serde_json::from_str(text).unwrap();
    events
        .into_iter()
        .map(|mut e| {
            let raw = e["payload"].as_str().unwrap_or("null").to_owned();
            e["payload"] = serde_json::from_str(&raw).unwrap_or(Value::Null);
            e
        })
        .collect()
}

fn recorded_summaries(addr: SocketAddr) -> Vec<Value> {
    summaries_from(&call_tool(
        addr,
        "recall_recent",
        json!({"kind": "summary", "limit": 50}),
    ))
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

/// Pretend the hook's last failed reload was longer ago than the retry
/// cooldown, by back-dating `failed_at` in the (single) state file.
fn age_failure(hermes_home: &Path) {
    let dir = hermes_home.join("mneme-hook-state");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    let stamp = state["failed_at"].as_f64().expect("failed_at in the state");
    state["failed_at"] = json!(stamp - 121.0);
    std::fs::write(&files[0], state.to_string()).unwrap();
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
fn hook_injects_long_pins_whole_with_framing_and_capability_hint() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    std::fs::write(
        hermes.path().join(".env"),
        format!("MNEME_HTTP_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let server = start_serve(data.path());
    seed(server.addr);

    // A binding rule well past the old 300-char clip, with line breaks,
    // ending in a marker that only survives if nothing was cut.
    let long_rule = format!(
        "Never touch production without a ticket.\n{}\nEND-OF-LONG-RULE",
        "Every change needs a reviewer and a rollback plan. ".repeat(12)
    );
    assert!(long_rule.chars().count() > 600);
    let pinned = call_tool(server.addr, "pin", json!({"content": long_rule}));
    assert!(pinned["result"].is_object(), "{pinned}");
    // An event: mneme hands its payload to the hook as a JSON string.
    let r = call_tool(
        server.addr,
        "record_event",
        json!({"kind": "decision", "payload": {"content": "chose the flyctl deploy path"}}),
    );
    assert!(r["result"].is_object(), "{r}");

    let run = run_hook(
        py,
        &http_args(server.addr),
        hermes.path(),
        &payload(vec![], true, "what stop-loss do we use for QQQ3?"),
    );
    let ctx = context_of(&run);

    // Pins arrive whole, line breaks kept (continuation lines indented).
    assert!(ctx.contains("END-OF-LONG-RULE"), "{ctx}");
    assert!(ctx.contains("Never touch production without a ticket."));
    assert!(!ctx.contains("rollback plan. …"), "pin was clipped: {ctx}");
    assert!(ctx.contains("\n  END-OF-LONG-RULE"), "{ctx}");

    // Trust framing: pins binding, the rest wrapped as data.
    assert!(
        ctx.contains("standing instructions and are binding"),
        "{ctx}"
    );
    let envelope = ctx.find("<mneme-memory trust=\"data\">").expect("envelope");
    assert!(ctx.contains("</mneme-memory>"), "{ctx}");
    assert!(
        ctx.find("END-OF-LONG-RULE").unwrap() < envelope,
        "pins must sit outside the data envelope: {ctx}"
    );
    assert!(ctx[envelope..].contains("6% trailing stop-loss"), "{ctx}");

    // Events are decoded, not printed as raw JSON.
    assert!(
        ctx.contains("decision: chose the flyctl deploy path"),
        "{ctx}"
    );
    assert!(!ctx.contains("{\"content\""), "{ctx}");

    // Capability hint in the footer.
    assert!(ctx.contains("mode: \"keyword\""), "{ctx}");
    assert!(ctx.contains("supersedes: \"<old id>\""), "{ctx}");
}

#[test]
fn hook_shedding_keeps_pins_whole_and_says_so() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    std::fs::write(
        hermes.path().join(".env"),
        format!("MNEME_HTTP_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let server = start_serve(data.path());
    seed(server.addr);
    let rule = format!("{} END-OF-BIG-RULE", "Rule text. ".repeat(100));
    let pinned = call_tool(server.addr, "pin", json!({"content": rule}));
    assert!(pinned["result"].is_object(), "{pinned}");
    let r = call_tool(
        server.addr,
        "record_event",
        json!({"kind": "decision", "payload": {"content": "an event to be shed"}}),
    );
    assert!(r["result"].is_object(), "{r}");

    let mut args = http_args(server.addr);
    args.extend(["--max-chars".into(), "600".into()]);
    let run = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(vec![], true, "what stop-loss do we use for QQQ3?"),
    );
    let ctx = context_of(&run);
    assert!(ctx.contains("END-OF-BIG-RULE"), "{ctx}");
    assert!(ctx.contains("Always answer with numbers first."), "{ctx}");
    assert!(!ctx.contains("an event to be shed"), "{ctx}");
    assert!(run.stderr.contains("shed"), "{}", run.stderr);
}

const COMPACTION_HEADER: &str =
    "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted into the summary below.";

fn compaction_history(body: &str) -> Vec<Value> {
    vec![
        msg("user", &format!("{COMPACTION_HEADER}\n\n{body}")),
        msg("user", "hi"),
        msg("assistant", "hello"),
    ]
}

#[test]
fn hook_records_each_compaction_summary_once() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    std::fs::write(
        hermes.path().join(".env"),
        format!("MNEME_HTTP_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let server = start_serve(data.path());
    seed(server.addr);
    let args = http_args(server.addr);

    // A session-start turn records nothing.
    let first = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(vec![], true, "stop-loss?"),
    );
    context_of(&first);
    assert!(recorded_summaries(server.addr).is_empty());

    // First compaction: injected, and the summary lands in the timeline.
    let body = "ZEBRA-SUMMARY-BODY: we migrated the deploy to flyctl and fixed the stop-loss bug.";
    let history = compaction_history(body);
    let turn = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(history.clone(), false, "continue"),
    );
    let ctx = context_of(&turn);
    assert!(ctx.contains("just compressed"), "{ctx}");
    assert!(
        !ctx.contains("ZEBRA-SUMMARY-BODY"),
        "the new summary must not be part of this injection: {ctx}"
    );
    assert!(!turn.stderr.contains("could not record"), "{}", turn.stderr);
    let found = recorded_summaries(server.addr);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["kind"], "summary");
    assert_eq!(found[0]["payload"]["source"], "hermes-compaction");
    assert!(
        found[0]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("ZEBRA-SUMMARY-BODY"),
        "{found:?}"
    );

    // The same compaction on a later turn is neither re-injected nor
    // re-recorded.
    let mut later = history.clone();
    later.push(msg("user", "more"));
    let quiet = run_hook(py, &args, hermes.path(), &payload(later, false, "next"));
    assert_eq!(quiet.stdout, json!({}), "{}", quiet.stderr);
    assert_eq!(recorded_summaries(server.addr).len(), 1);

    // History shrinks with the same summary still on top (no new
    // fingerprint): reloaded, but not recorded a second time.
    let shrunk = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(vec![history[0].clone()], false, "go on"),
    );
    assert!(context_of(&shrunk).contains("just compressed"));
    assert_eq!(recorded_summaries(server.addr).len(), 1);

    // A different compaction is a new summary event. This one is huge:
    // the hook trims it under the server's size limit rather than losing
    // the event.
    let huge = format!("SECOND-SUMMARY {}", "lorem \"ipsum\"\n".repeat(2000));
    let turn = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(compaction_history(&huge), false, "and then"),
    );
    context_of(&turn);
    assert!(!turn.stderr.contains("could not record"), "{}", turn.stderr);
    let found = recorded_summaries(server.addr);
    assert_eq!(found.len(), 2, "{found:?}");
}

/// A marker whose body is only a header, followed by an ordinary user
/// turn: the user turn is not the summary, so nothing is recorded (but
/// the reload still happens, once). A summary in the following
/// assistant message is recorded.
#[test]
fn hook_records_only_a_real_summary_after_a_header_only_marker() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    std::fs::write(
        hermes.path().join(".env"),
        format!("MNEME_HTTP_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let server = start_serve(data.path());
    seed(server.addr);
    let args = http_args(server.addr);

    let header_only = vec![
        msg("user", COMPACTION_HEADER),
        msg("user", "hi"),
        msg("assistant", "hello"),
    ];
    let turn = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(header_only.clone(), false, "continue"),
    );
    assert!(context_of(&turn).contains("just compressed"));
    assert!(!turn.stderr.contains("could not record"), "{}", turn.stderr);
    assert!(
        recorded_summaries(server.addr).is_empty(),
        "an ordinary user turn must not be recorded as the summary"
    );
    // The gap is visible in Hermes' hook log.
    assert!(
        turn.stderr.contains("compaction summary was empty"),
        "{}",
        turn.stderr
    );
    // Seen once: the same marker on the next turn is quiet.
    let mut later = header_only.clone();
    later.push(msg("user", "more"));
    let quiet = run_hook(py, &args, hermes.path(), &payload(later, false, "next"));
    assert_eq!(quiet.stdout, json!({}), "{}", quiet.stderr);
    assert!(
        !quiet.stderr.contains("summary was empty"),
        "{}",
        quiet.stderr
    );
    assert!(recorded_summaries(server.addr).is_empty());

    // A different compaction whose body is the next (assistant) message.
    let split = vec![
        msg("user", &format!("{COMPACTION_HEADER} (second)")),
        msg("assistant", "SPLIT-SUMMARY-BODY: deploy moved to flyctl."),
        msg("user", "hi"),
    ];
    let turn = run_hook(py, &args, hermes.path(), &payload(split, false, "and then"));
    context_of(&turn);
    assert!(!turn.stderr.contains("could not record"), "{}", turn.stderr);
    assert!(
        !turn.stderr.contains("summary was empty"),
        "{}",
        turn.stderr
    );
    let found = recorded_summaries(server.addr);
    assert_eq!(found.len(), 1, "{found:?}");
    let text = found[0]["payload"]["text"].as_str().unwrap();
    assert!(text.contains("SPLIT-SUMMARY-BODY"), "{text}");
    assert!(!text.contains("hi"), "{text}");
}

/// A compaction turn that hits a dead server delivers nothing and must
/// not use up the compaction: the same history against a live server
/// reloads and records.
#[test]
fn hook_retries_a_compaction_the_server_missed() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let data = TempDir::new().unwrap();
    let hermes = TempDir::new().unwrap();
    std::fs::write(
        hermes.path().join(".env"),
        format!("MNEME_HTTP_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let args = http_args(addr);
    let body = "RETRY-SUMMARY-BODY: we finished the migration and verified the stop-loss.";
    let history = compaction_history(body);

    // Session start against a live server, then the server goes away.
    {
        let server = start_serve_at(data.path(), addr);
        seed(server.addr);
        let first = run_hook(
            py,
            &args,
            hermes.path(),
            &payload(vec![], true, "stop-loss?"),
        );
        context_of(&first);
    }
    let missed = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(history.clone(), false, "continue"),
    );
    assert_eq!(missed.stdout, json!({}));
    assert!(missed.stderr.contains("mneme hook:"), "{}", missed.stderr);

    // Server back, and the retry cooldown over: the same history is still
    // a compression turn.
    age_failure(hermes.path());
    let server = start_serve_at(data.path(), addr);
    let turn = run_hook(
        py,
        &args,
        hermes.path(),
        &payload(history.clone(), false, "continue"),
    );
    assert!(context_of(&turn).contains("just compressed"));
    assert!(!turn.stderr.contains("could not record"), "{}", turn.stderr);
    let found = recorded_summaries(server.addr);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("RETRY-SUMMARY-BODY")
    );
    // And now it is done.
    let mut later = history;
    later.push(msg("user", "more"));
    let quiet = run_hook(py, &args, hermes.path(), &payload(later, false, "next"));
    assert_eq!(quiet.stdout, json!({}), "{}", quiet.stderr);
}

/// A server that accepts connections and hangs up costs one failed turn;
/// the turns right after it are quiet and never connect (the retry
/// cooldown), and the retry fires once the cooldown has passed.
#[test]
fn hook_waits_out_a_cooldown_after_a_failed_reload() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let hermes = TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (accepted, stop) = (accepted.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    // Dropped at once: the client sees a closed connection.
                    Ok(_) => {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
    };
    let args = http_args(addr);
    let turn = || run_hook(py, &args, hermes.path(), &payload(vec![], true, "hello"));

    let first = turn();
    assert_eq!(first.stdout, json!({}));
    assert!(first.stderr.contains("mneme hook:"), "{}", first.stderr);
    let attempts = accepted.load(Ordering::Relaxed);
    assert!(attempts >= 1, "the first turn must try the server");

    // Immediately after: quiet, no connection, no complaint.
    let started = std::time::Instant::now();
    let second = turn();
    assert_eq!(second.stdout, json!({}));
    assert_eq!(second.stderr, "", "a cooldown turn says nothing");
    assert_eq!(accepted.load(Ordering::Relaxed), attempts);
    assert!(started.elapsed() < Duration::from_secs(10));

    // The stamp is in the state file; age it past the cooldown.
    let state_file = || -> Value {
        let dir = hermes.path().join("mneme-hook-state");
        let entry = std::fs::read_dir(dir).unwrap().next().unwrap().unwrap();
        serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap()
    };
    let stamp = state_file()["failed_at"].as_f64().expect("failed_at");
    age_failure(hermes.path());

    let third = turn();
    assert_eq!(third.stdout, json!({}));
    assert!(third.stderr.contains("mneme hook:"), "{}", third.stderr);
    assert!(
        accepted.load(Ordering::Relaxed) > attempts,
        "after the cooldown the hook tries again"
    );
    let state = state_file();
    assert!(
        state["failed_at"].as_f64().unwrap() >= stamp,
        "the failure is stamped again: {state}"
    );

    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
}

/// Unit checks on the hook's internals (summary fingerprinting, state
/// restore, the time budget, the stdio deadline, the over-cap note),
/// run in Python against the real script. No server needed.
const UNIT_CHECKS: &str = r####"
import contextlib
import importlib.util
import io
import http.server
import json
import queue
import sys
import tempfile
import threading
import time

spec = importlib.util.spec_from_file_location("hook", sys.argv[1])
hook = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hook)

HEADER = hook.COMPACTION_MARKER + " — REFERENCE ONLY] Earlier turns were compacted."


def msg(role, content):
    return {"role": role, "content": content}


def stderr_of(fn, *a):
    buf = io.StringIO()
    with contextlib.redirect_stderr(buf):
        result = fn(*a)
    return result, buf.getvalue()


def payload(history, first=False):
    return {"session_id": "s", "extra": {"conversation_history": history, "is_first_turn": first}}


# P1: only a non-user message continues a header-only marker.
fp, body = hook.compaction_fingerprint([msg("user", HEADER), msg("user", "hi")])
assert fp and body == "", body
fp, body = hook.compaction_fingerprint([msg("user", HEADER), msg("assistant", "THE-SUMMARY")])
assert "THE-SUMMARY" in body, body
for role in ("system", "tool"):
    assert "X-BODY" in hook.compaction_fingerprint([msg("user", HEADER), msg(role, "X-BODY")])[1]
assert "PLAIN" in hook.compaction_fingerprint([HEADER, "PLAIN"])[1]
assert hook.compaction_fingerprint([msg("user", HEADER), {"content": "no role"}])[1] == ""
assert hook.compaction_fingerprint([msg("user", HEADER), msg("developer", "odd")])[1] == ""
long_body = "L" * 300
assert hook.compaction_fingerprint([msg("user", HEADER + "\n" + long_body), msg("assistant", "extra")])[1] == long_body

# P1 + P2: decide() keeps `previous` and never records an empty summary.
state_dir = tempfile.mkdtemp()
short = [msg("user", HEADER), msg("user", "hi")]
d = hook.decide(payload(short), state_dir)
assert d.reason == "compression" and not d.should_record and d.previous is None
assert d.state_path.exists()
assert d.empty_summary
hook.restore_state(d, state_dir)
# Nothing was there before, so only the failure stamp is left.
assert list(json.loads(d.state_path.read_text())) == ["failed_at"]

ordinary = [msg("user", "a"), msg("assistant", "b")]
state_dir = tempfile.mkdtemp()  # the old one is in its retry cooldown
d = hook.decide(payload(ordinary), state_dir)
assert d.reason == "session start" and d.previous is None
summary_history = [msg("user", HEADER + "\n\n" + "S" * 250)]
d = hook.decide(payload(summary_history), state_dir)
assert d.reason == "compression" and d.should_record
assert d.previous == {"fingerprint": "", "length": 2}, d.previous
hook.restore_state(d, state_dir)
restored = json.loads(d.state_path.read_text())
assert abs(restored.pop("failed_at") - time.time()) < 5
assert restored == {"fingerprint": "", "length": 2}

# T1: after a failure, the retry cooldown holds the next turn off without
# touching the state; once it has passed the retry happens and the stamp
# is cleared.
assert hook.RETRY_COOLDOWN_SECS == 120
cd = tempfile.mkdtemp()
d = hook.decide(payload([], first=True), cd)
assert d.reason == "session start" and d.previous is None
hook.restore_state(d, cd)
stamped = d.state_path.read_text()
quiet = hook.decide(payload([], first=True), cd)
assert quiet.reason is None and not quiet.should_record and not quiet.empty_summary
assert d.state_path.read_text() == stamped  # left alone: the retry stays pending
state = json.loads(stamped)
state["failed_at"] = time.time() - hook.RETRY_COOLDOWN_SECS - 1
d.state_path.write_text(json.dumps(state))
retry = hook.decide(payload([], first=True), cd)
assert retry.reason == "session start" and retry.previous is None
assert "failed_at" not in json.loads(d.state_path.read_text())  # cleared
# A failure with earlier state keeps that state next to the stamp.
cd = tempfile.mkdtemp()
hook.decide(payload([msg("user", "a")]), cd)  # first sight: state saved
steady = hook.decide(payload([msg("user", "a"), msg("assistant", "b")]), cd)
assert steady.reason is None
shrunk = hook.decide(payload([msg("user", "a")]), cd)
assert shrunk.reason == "compression"
hook.restore_state(shrunk, cd)
assert hook.decide(payload([msg("user", "a")]), cd).reason is None  # in cooldown
state = json.loads(shrunk.state_path.read_text())
assert state["length"] == 2 and "failed_at" in state, state
state["failed_at"] = time.time() + 3600  # from the future: ignored
shrunk.state_path.write_text(json.dumps(state))
assert hook.decide(payload([msg("user", "a")]), cd).reason == "compression"
state["failed_at"] = "junk"
shrunk.state_path.write_text(json.dumps(state))
assert hook.decide(payload([msg("user", "a")]), cd).reason == "compression"


# P4: out of budget is not an attempt; a real attempt (success or error) is.
class FakeConn:
    def __init__(self, reply=None, exc=None):
        self.calls, self.reply, self.exc = [], reply, exc

    def request(self, body, timeout=None):
        self.calls.append((body, timeout))
        if self.exc:
            raise self.exc
        return self.reply


ok = {"result": {"content": [{"type": "text", "text": "ok"}]}}
spent = lambda: time.monotonic() - hook.HOOK_BUDGET_SECS


def fresh_decision(history=None):
    sd = tempfile.mkdtemp()
    return sd, hook.decide(payload(history or summary_history), sd)


def recorded(decision):
    return json.loads(decision.state_path.read_text()).get("recorded_fingerprint")


sd, d = fresh_decision()
conn = FakeConn(ok)
_, err = stderr_of(hook.record_compaction, conn, d, sd, spent())
assert conn.calls == [] and recorded(d) is None and "out of time budget" in err, err

# The next compression turn with that summary still on top retries.
sd = tempfile.mkdtemp()
first = hook.decide(payload([msg("user", "x")] + summary_history), sd)
stderr_of(hook.record_compaction, FakeConn(ok), first, sd, spent())
again = hook.decide(payload(summary_history), sd)  # history shrank by one
assert again.reason == "compression" and again.should_record, (again.reason, again.recorded)
conn = FakeConn(ok)
stderr_of(hook.record_compaction, conn, again, sd, time.monotonic())
assert len(conn.calls) == 1 and recorded(again) == again.fingerprint
# ... and once attempted, a further shrink does not record again.
third = hook.decide(payload([]), sd)
assert third.reason == "compression" and not third.should_record

sd, d = fresh_decision()
conn = FakeConn(ok)
stderr_of(hook.record_compaction, conn, d, sd, time.monotonic())
assert len(conn.calls) == 1 and recorded(d) == d.fingerprint
assert conn.calls[0][1] <= hook.RECORD_TIMEOUT_SECS

sd, d = fresh_decision()
_, err = stderr_of(hook.record_compaction, FakeConn({"error": {"message": "boom"}}), d, sd, time.monotonic())
assert recorded(d) == d.fingerprint and "boom" in err

sd, d = fresh_decision()
_, err = stderr_of(hook.record_compaction, FakeConn(exc=OSError("down")), d, sd, time.monotonic())
assert recorded(d) == d.fingerprint and "down" in err

# P3: request timeouts follow the remaining budget.
assert hook.request_timeout(time.monotonic()) == hook.TIMEOUT_SECS
left = hook.request_timeout(time.monotonic() - hook.HOOK_BUDGET_SECS + 3)
assert 2 < left <= 3, left
try:
    hook.request_timeout(time.monotonic() - hook.HOOK_BUDGET_SECS + 0.5)
    raise AssertionError("expected out of budget")
except RuntimeError as e:
    assert "budget" in str(e)

# P3: a server that never answers `initialize` costs at most the budget.
saved_budget = hook.HOOK_BUDGET_SECS
hook.HOOK_BUDGET_SECS = 2
t0 = time.monotonic()
try:
    hook.StdioConnection([sys.executable, "-c", "import time; time.sleep(60)"], time.monotonic())
    raise AssertionError("expected a timeout")
except RuntimeError:
    pass
assert time.monotonic() - t0 < 6, time.monotonic() - t0
hook.HOOK_BUDGET_SECS = saved_budget

# T2: a server that trickles its response cannot hold a request past the
# budget, though every socket operation is well inside urllib's timeout.
class Trickle(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        try:
            self.send_response(200)
            self.send_header("Content-Length", "1000")
            self.end_headers()
            for _ in range(1000):
                self.wfile.write(b" ")
                self.wfile.flush()
                time.sleep(0.2)
        except OSError:
            pass

    def log_message(self, *a):
        pass


httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Trickle)
httpd.daemon_threads = True
threading.Thread(target=httpd.serve_forever, daemon=True).start()
conn = hook.HttpConnection.__new__(hook.HttpConnection)
conn.url = "http://127.0.0.1:%d/mcp" % httpd.server_address[1]
conn.headers = {"Content-Type": "application/json"}
conn.session = None
conn.started = time.monotonic()
t0 = time.monotonic()
try:
    conn._post({"jsonrpc": "2.0", "id": 1}, 1.0)
    raise AssertionError("expected a timeout")
except RuntimeError as e:
    assert "timed out" in str(e), e
assert time.monotonic() - t0 < 3, time.monotonic() - t0
httpd.shutdown()

# P5: lines for other ids do not keep _wait_for alive past its deadline.
conn = hook.StdioConnection.__new__(hook.StdioConnection)
conn.lines = queue.Queue()
stop = threading.Event()


def chatter():
    while not stop.is_set():
        conn.lines.put(json.dumps({"jsonrpc": "2.0", "id": 99}) + "\n")
        time.sleep(0.02)


threading.Thread(target=chatter, daemon=True).start()
t0 = time.monotonic()
try:
    conn._wait_for(1, 0.6)
    raise AssertionError("expected a timeout")
except RuntimeError as e:
    assert "timed out" in str(e)
elapsed = time.monotonic() - t0
stop.set()
assert 0.5 < elapsed < 2, elapsed

# P6: pins alone over the cap still produce a stderr note.
big = json.dumps({"procedural": [{"content": "rule " * 100}], "semantic": [], "episodic": []})
text, err = stderr_of(hook.render, big, "session start", 100)
assert len(text) > 100 and "over --max-chars 100" in err and "still" in err, err
text, err = stderr_of(hook.render, big, "session start", 100000)
assert err == ""

# P7: footer wording matches the skill.
assert "exact strings" not in hook.FOOTER and "not exact phrases" in hook.FOOTER
print("ok")
"####;

#[test]
fn hook_internals_hold() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let out = Command::new(py)
        .args(["-I", "-c", UNIT_CHECKS, SCRIPT])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "unit checks failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
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

    // Unreachable server: same (once the retry cooldown has passed).
    drop(server);
    age_failure(hermes.path());
    let run = run_hook(
        py,
        &["--url".into(), "http://127.0.0.1:9/mcp".into()],
        hermes.path(),
        &payload(vec![], true, "hello"),
    );
    assert_eq!(run.stdout, json!({}));
    assert!(run.stderr.contains("mneme hook:"), "{}", run.stderr);
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
    let run_stdio_hook = |payload: &Value| -> (Value, String) {
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
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let run = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&run.stderr).to_string();
        let out: Value = serde_json::from_slice(&run.stdout)
            .unwrap_or_else(|e| panic!("hook stdout not JSON ({e}) / {stderr}"));
        (out, stderr)
    };

    let (out, stderr) = run_stdio_hook(&payload(vec![], true, "stop-loss?"));
    let ctx = out["context"]
        .as_str()
        .unwrap_or_else(|| panic!("no injection: {out} / {stderr}"));
    assert!(ctx.contains("Always answer with numbers first."), "{ctx}");

    // A compaction turn over stdio: injected, and the summary is
    // recorded through the same `mneme client` connection.
    let body = "STDIO-SUMMARY-BODY: finished the migration.";
    let (out, stderr) = run_stdio_hook(&payload(compaction_history(body), false, "continue"));
    assert!(
        out["context"]
            .as_str()
            .is_some_and(|c| c.contains("just compressed")),
        "no injection: {out} / {stderr}"
    );
    assert!(!stderr.contains("could not record"), "{stderr}");

    // Read it back through `mneme client` (lines in, lines out).
    let mut client = Command::new(BINARY)
        .arg("client")
        .env("MNEME_DATA_DIR", data.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = client.stdin.take().unwrap();
    for m in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "recall_recent",
                          "arguments": {"kind": "summary"}}}),
    ] {
        writeln!(stdin, "{m}").unwrap();
    }
    stdin.flush().unwrap();
    // Read replies on a thread so a stuck client fails the test instead
    // of hanging it.
    let stdout = client.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v["id"] == 2
            {
                let _ = tx.send(v);
                return;
            }
        }
    });
    let reply = rx.recv_timeout(Duration::from_secs(20));
    drop(stdin);
    let _ = client.kill();
    let _ = client.wait();
    let _ = daemon.kill();
    let _ = daemon.wait();
    let reply = reply.expect("recall_recent reply");
    let found = summaries_from(&reply);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["payload"]["source"], "hermes-compaction");
    assert!(
        found[0]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("STDIO-SUMMARY-BODY")
    );
}
