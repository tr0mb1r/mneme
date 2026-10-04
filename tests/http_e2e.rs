//! End-to-end tests for the Streamable HTTP transport: spawn the real
//! binary (`mneme serve`, and `mneme daemon` with `[http] enabled`) and
//! talk to it over TCP the way a containerised agent such as Hermes
//! does.
//!
//! The HTTP client is a deliberately tiny HTTP/1.1 implementation over
//! `TcpStream` (one request per connection, `Connection: close`) so the
//! test needs no extra dev-dependency.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");
const TOKEN: &str = "e2e-test-token";

struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpReply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "body is not JSON ({e}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> HttpReply {
    let payload = body
        .map(|b| serde_json::to_vec(b).unwrap())
        .unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if body.is_some() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str("Accept: application/json, text/event-stream\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", payload.len()));

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(&payload).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"))
    {
        body = dechunk(&body);
    }
    HttpReply {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n").unwrap();
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..eol]).unwrap().trim(), 16).unwrap();
        data = &data[eol + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_for_health(addr: SocketAddr, child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("mneme exited before serving: {status}");
        }
        if TcpStream::connect(addr).await.is_ok() {
            let r = http(addr, "GET", "/healthz", &[], None).await;
            if r.status == 200 {
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("HTTP listener on {addr} did not become healthy within 20 s");
}

fn spawn(args: &[&str], data_dir: &Path, envs: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(BINARY);
    cmd.args(args)
        .env("MNEME_LOG", "off")
        .env("MNEME_DATA_DIR", data_dir)
        .env("MNEME_EMBEDDER", "stub")
        .env_remove("MNEME_HTTP_TOKEN")
        .env_remove("MNEME_HTTP_TOKEN_FILE")
        .env_remove("MNEME_HTTP_BIND")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn mneme")
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

/// `initialize` + `notifications/initialized`; returns the session id.
async fn open_session(addr: SocketAddr, token: &str) -> String {
    let bearer = auth(token);
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "http-e2e", "version": "0"}
        }
    });
    let r = http(
        addr,
        "POST",
        "/mcp",
        &[("Authorization", &bearer)],
        Some(&init),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["result"]["serverInfo"]["name"], "mneme");
    let sid = r
        .header("mcp-session-id")
        .expect("initialize returns Mcp-Session-Id")
        .to_owned();
    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let r = http(
        addr,
        "POST",
        "/mcp",
        &[("Authorization", &bearer), ("Mcp-Session-Id", &sid)],
        Some(&note),
    )
    .await;
    assert_eq!(r.status, 202);
    sid
}

async fn call(
    addr: SocketAddr,
    token: &str,
    sid: &str,
    id: i64,
    method: &str,
    params: Value,
) -> Value {
    let bearer = auth(token);
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let r = http(
        addr,
        "POST",
        "/mcp",
        &[("Authorization", &bearer), ("Mcp-Session-Id", sid)],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = r.json();
    assert_eq!(v["id"], id);
    v
}

#[cfg(unix)]
async fn sigterm_and_wait(mut child: Child) {
    let pid = child.id().expect("pid") as i32;
    // SAFETY: signalling the child we spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("mneme exits within 20 s of SIGTERM")
        .unwrap();
    assert!(status.success(), "unclean exit: {status}");
}

#[tokio::test]
async fn serve_round_trips_memory_across_sessions() {
    let tmp = TempDir::new().unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let bind = addr.to_string();
    let mut child = spawn(
        &["serve", "--bind", &bind],
        tmp.path(),
        &[("MNEME_HTTP_TOKEN", TOKEN)],
    );
    wait_for_health(addr, &mut child).await;

    // Unauthenticated and wrongly authenticated requests are refused.
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let r = http(addr, "POST", "/mcp", &[], Some(&ping)).await;
    assert_eq!(r.status, 401);
    let r = http(
        addr,
        "POST",
        "/mcp",
        &[("Authorization", "Bearer wrong")],
        Some(&ping),
    )
    .await;
    assert_eq!(r.status, 401);

    // Session A stores a memory.
    let a = open_session(addr, TOKEN).await;
    let tools = call(addr, TOKEN, &a, 2, "tools/list", json!({})).await;
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "recall")
    );
    let rem = call(
        addr,
        TOKEN,
        &a,
        3,
        "tools/call",
        json!({"name": "remember", "arguments": {"content": "hermes reaches mneme over http"}}),
    )
    .await;
    let text = rem["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("stored memory "), "{text}");

    // Session B (a different agent / reconnect) sees it.
    let b = open_session(addr, TOKEN).await;
    assert_ne!(a, b);
    let rec = call(
        addr,
        TOKEN,
        &b,
        4,
        "tools/call",
        json!({"name": "recall", "arguments": {"query": "hermes reaches mneme over http"}}),
    )
    .await;
    let text = rec["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("hermes reaches mneme over http"), "{text}");

    // Resources are reachable too (Hermes wraps them as tools).
    let res = call(
        addr,
        TOKEN,
        &b,
        5,
        "resources/read",
        json!({"uri": "mneme://procedural"}),
    )
    .await;
    assert!(res["result"]["contents"].is_array(), "{res}");

    // DELETE ends the session.
    let bearer = auth(TOKEN);
    let r = http(
        addr,
        "DELETE",
        "/mcp",
        &[("Authorization", &bearer), ("Mcp-Session-Id", &a)],
        None,
    )
    .await;
    assert_eq!(r.status, 204);

    #[cfg(unix)]
    sigterm_and_wait(child).await;
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
}

#[tokio::test]
async fn serve_defaults_to_the_daemon_token_file() {
    let tmp = TempDir::new().unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let bind = addr.to_string();
    let mut child = spawn(&["serve", "--bind", &bind], tmp.path(), &[]);
    wait_for_health(addr, &mut child).await;

    let token = std::fs::read_to_string(tmp.path().join("run").join("auth.token"))
        .expect("serve generates run/auth.token when no token is configured");
    let token = token.trim();
    open_session(addr, token).await;

    let _ = child.kill().await;
}

/// Bare-metal shape: one daemon serving Claude Code over the Unix
/// socket and Hermes (or anything else) over HTTP, same store.
#[cfg(unix)]
#[tokio::test]
async fn daemon_with_http_enabled_serves_both_transports() {
    let tmp = TempDir::new().unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    std::fs::write(
        tmp.path().join("config.toml"),
        format!("[http]\nenabled = true\nbind = \"{addr}\"\n"),
    )
    .unwrap();
    let mut child = spawn(
        &["daemon", "--foreground"],
        tmp.path(),
        &[("MNEME_HTTP_TOKEN", TOKEN)],
    );
    wait_for_health(addr, &mut child).await;

    let socket = tmp.path().join("run").join("mneme.sock");
    assert!(socket.exists(), "the Unix socket must still be served");

    let sid = open_session(addr, TOKEN).await;
    let rem = call(
        addr,
        TOKEN,
        &sid,
        2,
        "tools/call",
        json!({"name": "remember", "arguments": {"content": "stored via daemon http"}}),
    )
    .await;
    assert!(
        rem["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("stored memory ")
    );

    // The socket transport still answers, behind its own handshake.
    let token = std::fs::read_to_string(tmp.path().join("run").join("auth.token")).unwrap();
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream
        .write_all(format!("MNEME-AUTH: {}\n", token.trim()).as_bytes())
        .await
        .unwrap();
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    stream
        .write_all(format!("{init}\n").as_bytes())
        .await
        .unwrap();
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line),
    )
    .await
    .expect("socket answers")
    .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "mneme");
    drop(reader);

    sigterm_and_wait(child).await;
}
