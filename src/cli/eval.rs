//! `mneme eval` — measure recall and auto-context quality against a
//! set of questions with known answers, without changing the store.
//!
//! # How it talks to the store
//!
//! It starts a server and speaks MCP to it, the way an agent does:
//! by default a private `mneme run` (stdio) on the data directory, or
//! with `--daemon` a `mneme client` bridge to the daemon that is
//! already running. Nothing is opened in-process. That is deliberate:
//! the numbers describe the same code path, configuration, embedder
//! and ranking an agent gets, and a new `recall` argument is picked up
//! from the server's tool schema instead of being re-implemented here.
//! The private server holds the data-dir lock for the length of the
//! run, so nobody else can write in the meantime.
//!
//! # Read-only
//!
//! The run calls one tool, `recall`, and reads two resources,
//! `mneme://context` and `mneme://stats`. Any other tool name is
//! refused before it is sent ([`READ_ONLY_TOOLS`]). The store's counts,
//! the semantic index's applied LSN and the session's `turns_total` are
//! read before and after; if any of them moved, the command writes its
//! output, says which one moved, and fails.
//!
//! One thing does get written, by the server and not by the run: a
//! private `mneme run` records its own `session_start` and
//! `session_end` events, as every server start does. They fall outside
//! the before/after window. `--daemon` adds none.
//!
//! # Input
//!
//! `questions.jsonl`, one JSON object per line (blank lines and lines
//! starting with `#` are skipped):
//!
//! ```json
//! {"id":"q01","question":"…","type":"exact","scope":"ops",
//!  "gold":["<memory id>"],"gold_l3":["<event id>"],"stale":["<memory id>"]}
//! ```
//!
//! `gold` are L4 memory ids that answer the question, `gold_l3` are L3
//! event ids that do, `stale` are superseded memories that must not
//! rank above a current one. Only `id` and `question` are required.
//!
//! # What is measured
//!
//! `recall` is run for every question in every cell of
//! mode (hybrid, semantic, keyword) × scope filter (off, on) × layers
//! (semantic; semantic + episodic when the server's `recall` has a
//! `layers` argument). Per cell:
//!
//! - **hit@1, hit@5, MRR** over the questions with L4 gold (`l4`) and
//!   over all questions with any gold (`all`, where `gold_l3` counts
//!   too, so an L3-only question is a miss for a semantic-only cell);
//! - **stale over current**: questions with `stale` ids where a stale
//!   memory ranks above every current one;
//! - **off-scope@5**: the mean share of the top five rows that are not
//!   in the question's scope;
//! - **latency** p50 / p95.
//!
//! Then `mneme://context?q=<question>` is replayed per question (with
//! the question's scope, if it has one): **context gold** is the share
//! of questions whose context holds one of their gold ids; **layer
//! shares** are each section's estimated tokens over the budget; stale
//! over current is counted on the semantic section as for `recall`.
//!
//! The headline numbers, and the optional `--min-*` / `--max-*`
//! thresholds that turn the command into a gate, come from one mode
//! (`--gate-mode`, default hybrid), unscoped.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{MnemeError, Result};

/// The only tools `mneme eval` will call.
pub const READ_ONLY_TOOLS: &[&str] = &["recall"];

const MODES: [&str; 3] = ["hybrid", "semantic", "keyword"];
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayersArg {
    /// Both layer sets if the server's `recall` accepts `layers`,
    /// otherwise semantic only.
    Auto,
    /// L4 only; never send `layers`.
    Semantic,
    /// Require `layers` support; fail if the server lacks it.
    Both,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Hybrid,
    Semantic,
    Keyword,
}

impl GateMode {
    fn as_str(self) -> &'static str {
        match self {
            GateMode::Hybrid => "hybrid",
            GateMode::Semantic => "semantic",
            GateMode::Keyword => "keyword",
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct EvalArgs {
    /// Questions file (JSON Lines).
    pub questions: PathBuf,
    /// Directory for `metrics-<tag>.json` and `summary-<tag>.md`.
    #[arg(long, default_value = ".")]
    pub out_dir: PathBuf,
    /// Label for this run; part of the output file names.
    #[arg(long, default_value = "run")]
    pub tag: String,
    /// Data directory to evaluate. Defaults to `$MNEME_DATA_DIR`, then
    /// `~/.mneme`.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Evaluate through the running daemon (`mneme client`) instead of
    /// starting a private server. Other clients can write during the
    /// run, which the no-write check will report as a failure.
    #[arg(long)]
    pub daemon: bool,
    /// Which `recall` layer sets to run.
    #[arg(long, value_enum, default_value = "auto")]
    pub layers: LayersArg,
    /// Rows to ask `recall` for. MRR is computed over this many.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Skip the `mneme://context` replays.
    #[arg(long)]
    pub no_context: bool,
    /// The recall mode the headline numbers and the thresholds below
    /// are taken from. `hybrid` is what agents get by default. With the
    /// stub embedder vectors carry no meaning, so a model-free run
    /// should gate on `keyword`.
    #[arg(long, value_enum, default_value = "hybrid")]
    pub gate_mode: GateMode,
    /// Fail if hit@5 over L4 questions (gate mode, unscoped, semantic
    /// layer) is below this.
    #[arg(long)]
    pub min_l4_hit5: Option<f64>,
    /// Fail if hit@5 over all questions (gate mode, unscoped, widest
    /// layer set) is below this.
    #[arg(long)]
    pub min_all_hit5: Option<f64>,
    /// Fail if more than this many questions rank a stale memory above
    /// the current one in `recall` (gate mode, unscoped).
    #[arg(long)]
    pub max_stale_over_current: Option<u64>,
    /// Fail if more than this many questions have a stale memory above
    /// the current one in their auto-context.
    #[arg(long)]
    pub max_context_stale_over_current: Option<u64>,
    /// Fail if the share of questions whose auto-context holds a gold
    /// id is below this.
    #[arg(long)]
    pub min_context_gold: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub gold: Vec<String>,
    #[serde(default)]
    pub gold_l3: Vec<String>,
    #[serde(default)]
    pub stale: Vec<String>,
}

impl Question {
    fn any_gold(&self) -> BTreeSet<&str> {
        self.gold
            .iter()
            .chain(&self.gold_l3)
            .map(String::as_str)
            .collect()
    }
}

/// Parse a questions file. Errors name the line.
pub fn parse_questions(text: &str) -> Result<Vec<Question>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let q: Question = serde_json::from_str(line)
            .map_err(|e| MnemeError::Config(format!("questions line {}: {e}", i + 1)))?;
        if q.question.trim().is_empty() {
            return Err(MnemeError::Config(format!(
                "questions line {}: `question` is empty",
                i + 1
            )));
        }
        if !seen.insert(q.id.clone()) {
            return Err(MnemeError::Config(format!(
                "questions line {}: duplicate id `{}`",
                i + 1,
                q.id
            )));
        }
        out.push(q);
    }
    if out.is_empty() {
        return Err(MnemeError::Config("no questions in the file".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Metrics (pure)
// ---------------------------------------------------------------------

/// One returned row: its id and scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub scope: Option<String>,
}

/// 1-based rank of the first row whose id is in `targets`.
fn first_rank(rows: &[Row], targets: &BTreeSet<&str>) -> Option<usize> {
    rows.iter()
        .position(|r| targets.contains(r.id.as_str()))
        .map(|i| i + 1)
}

#[derive(Debug, Default, Clone)]
struct HitStats {
    n: usize,
    hit1: usize,
    hit5: usize,
    rr_sum: f64,
}

impl HitStats {
    fn add(&mut self, rank: Option<usize>) {
        self.n += 1;
        if let Some(r) = rank {
            if r == 1 {
                self.hit1 += 1;
            }
            if r <= 5 {
                self.hit5 += 1;
            }
            self.rr_sum += 1.0 / r as f64;
        }
    }

    fn json(&self) -> Value {
        json!({
            "n": self.n,
            "hit_at_1": ratio(self.hit1, self.n),
            "hit_at_5": ratio(self.hit5, self.n),
            "mrr": if self.n == 0 { Value::Null } else { json!(round4(self.rr_sum / self.n as f64)) },
        })
    }
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// `a / b` rounded, or null when there is nothing to divide by: a
/// metric over zero questions is unknown, not zero.
fn ratio(a: usize, b: usize) -> Value {
    if b == 0 {
        Value::Null
    } else {
        json!(round4(a as f64 / b as f64))
    }
}

/// Nearest-rank percentile of `values` (milliseconds).
fn percentile(values: &[f64], p: f64) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let idx = ((p / 100.0) * v.len() as f64).ceil() as usize;
    json!((v[idx.clamp(1, v.len()) - 1] * 100.0).round() / 100.0)
}

/// True when a stale memory ranks above every current one (or a stale
/// one is returned and no current one is).
fn stale_over_current(rows: &[Row], q: &Question) -> bool {
    let stale: BTreeSet<&str> = q.stale.iter().map(String::as_str).collect();
    match (first_rank(rows, &stale), first_rank(rows, &q.any_gold())) {
        (Some(s), Some(c)) => s < c,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Share of the top five rows outside `scope`. `None` with no rows.
fn off_scope_at_5(rows: &[Row], scope: &str) -> Option<f64> {
    let top: Vec<&Row> = rows.iter().take(5).collect();
    if top.is_empty() {
        return None;
    }
    let off = top
        .iter()
        .filter(|r| r.scope.as_deref() != Some(scope))
        .count();
    Some(off as f64 / top.len() as f64)
}

/// Accumulates one grid cell.
#[derive(Debug, Default)]
struct Cell {
    l4: HitStats,
    all: HitStats,
    stale_n: usize,
    stale_over: usize,
    stale_ids: Vec<String>,
    off_scope: Vec<f64>,
    latency_ms: Vec<f64>,
    errors: Vec<String>,
    /// Per question `type`, over any gold.
    by_type: BTreeMap<String, HitStats>,
    /// Questions with gold and none of it in the top five.
    missed_at_5: Vec<String>,
}

impl Cell {
    fn add(&mut self, q: &Question, rows: &[Row], ms: f64) {
        self.latency_ms.push(ms);
        if !q.gold.is_empty() {
            let gold: BTreeSet<&str> = q.gold.iter().map(String::as_str).collect();
            self.l4.add(first_rank(rows, &gold));
        }
        let any = q.any_gold();
        if !any.is_empty() {
            let rank = first_rank(rows, &any);
            self.all.add(rank);
            self.by_type
                .entry(q.kind.clone().unwrap_or_else(|| "untyped".into()))
                .or_default()
                .add(rank);
            if rank.is_none_or(|r| r > 5) {
                self.missed_at_5.push(q.id.clone());
            }
        }
        if !q.stale.is_empty() {
            self.stale_n += 1;
            if stale_over_current(rows, q) {
                self.stale_over += 1;
                self.stale_ids.push(q.id.clone());
            }
        }
        if let Some(scope) = &q.scope
            && let Some(share) = off_scope_at_5(rows, scope)
        {
            self.off_scope.push(share);
        }
    }

    fn json(&self) -> Value {
        let off = if self.off_scope.is_empty() {
            Value::Null
        } else {
            json!(round4(
                self.off_scope.iter().sum::<f64>() / self.off_scope.len() as f64
            ))
        };
        json!({
            "l4": self.l4.json(),
            "all": self.all.json(),
            "stale_over_current": {
                "n": self.stale_n,
                "count": self.stale_over,
                "questions": self.stale_ids,
            },
            "off_scope_at_5": off,
            "latency_ms": {
                "p50": percentile(&self.latency_ms, 50.0),
                "p95": percentile(&self.latency_ms, 95.0),
            },
            "by_type": self
                .by_type
                .iter()
                .map(|(k, v)| (k.clone(), v.json()))
                .collect::<serde_json::Map<_, _>>(),
            "missed_at_5": self.missed_at_5,
            "errors": self.errors,
        })
    }
}

/// `chars / 4`, the estimate the orchestrator budgets with.
fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Percent-encode a query-string value.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Rows out of a `recall` reply: an array of objects with `id`, or an
/// object wrapping such an array.
fn rows_from_reply(v: &Value) -> Vec<Row> {
    let list = match v {
        Value::Array(a) => a.as_slice(),
        Value::Object(o) => ["results", "hits", "memories", "rows"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_array))
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };
    list.iter()
        .filter_map(|r| {
            Some(Row {
                id: r.get("id")?.as_str()?.to_owned(),
                scope: r.get("scope").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// MCP over a child process
// ---------------------------------------------------------------------

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    how: &'static str,
}

impl Server {
    fn spawn(args: &EvalArgs) -> Result<Self> {
        let exe = std::env::current_exe().map_err(MnemeError::Io)?;
        let sub = if args.daemon { "client" } else { "run" };
        let mut cmd = Command::new(exe);
        cmd.arg(sub)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(dir) = &args.data_dir {
            cmd.env("MNEME_DATA_DIR", dir);
        }
        if std::env::var_os("MNEME_LOG").is_none() {
            cmd.env("MNEME_LOG", "warn");
        }
        let mut child = cmd.spawn().map_err(MnemeError::Io)?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut server = Server {
            child,
            stdin,
            stdout,
            next_id: 1,
            how: sub,
        };
        let init = server.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "mneme-eval", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        if init.get("serverInfo").is_none() {
            return Err(MnemeError::Mcp(format!(
                "unexpected initialize reply: {init}"
            )));
        }
        server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(server)
    }

    fn send(&mut self, frame: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| MnemeError::Mcp("server stdin already closed".into()))?;
        writeln!(stdin, "{frame}").map_err(MnemeError::Io)?;
        stdin.flush().map_err(MnemeError::Io)
    }

    /// One request; returns `result`, or an error carrying the
    /// JSON-RPC error message.
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).map_err(MnemeError::Io)?;
            if n == 0 {
                let hint = if self.how == "run" {
                    "is another mneme instance using this data directory? \
                     Stop it, or pass --daemon to evaluate through it"
                } else {
                    "is the daemon running?"
                };
                return Err(MnemeError::Mcp(format!(
                    "`mneme {}` exited during `{method}` ({hint})",
                    self.how
                )));
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if v.get("id") != Some(&json!(id)) {
                // A notification or a server-side request: not ours.
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(MnemeError::Mcp(format!(
                    "{method}: {}",
                    err["message"].as_str().unwrap_or("error")
                )));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// `tools/call` for a tool on the read-only allow-list. Returns the
    /// reply text parsed as JSON.
    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if !READ_ONLY_TOOLS.contains(&name) {
            return Err(MnemeError::Mcp(format!(
                "`mneme eval` is read-only and will not call `{name}`"
            )));
        }
        let result = self.request("tools/call", json!({"name": name, "arguments": args}))?;
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        if result["isError"] == json!(true) {
            return Err(MnemeError::Mcp(format!("{name}: {text}")));
        }
        serde_json::from_str(text)
            .map_err(|e| MnemeError::Mcp(format!("{name} reply is not JSON: {e}")))
    }

    fn read_resource(&mut self, uri: &str) -> Result<Value> {
        let result = self.request("resources/read", json!({"uri": uri}))?;
        let text = result["contents"][0]["text"].as_str().unwrap_or_default();
        serde_json::from_str(text).map_err(|e| MnemeError::Mcp(format!("{uri} is not JSON: {e}")))
    }

    /// Close stdin and wait for the server to exit.
    fn shutdown(mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// The numbers that must not move during a read-only run.
fn write_fingerprint(stats: &Value) -> Value {
    let episodic = &stats["memories"]["episodic"];
    json!({
        "semantic": stats["memories"]["semantic"],
        "procedural": stats["memories"]["procedural"],
        // Hot and warm together: consolidation may move an event from
        // one tier to the other while we run, which is not a write of
        // ours and not a change of content.
        "episodic_hot_plus_warm": episodic["hot"].as_u64().unwrap_or(0)
            + episodic["warm"].as_u64().unwrap_or(0),
        "applied_lsn": stats["semantic_index"]["applied_lsn"],
        "turns_total": stats["working"]["turns_total"],
    })
}

// ---------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------

pub fn execute(args: EvalArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.questions).map_err(|e| {
        MnemeError::Config(format!("cannot read {}: {e}", args.questions.display()))
    })?;
    let questions = parse_questions(&text)?;
    if args.limit == 0 {
        return Err(MnemeError::Config("--limit must be at least 1".into()));
    }
    std::fs::create_dir_all(&args.out_dir).map_err(MnemeError::Io)?;

    let mut server = Server::spawn(&args)?;
    let outcome = run(&mut server, &args, &questions);
    server.shutdown();
    let (metrics, summary) = outcome?;

    let metrics_path = args.out_dir.join(format!("metrics-{}.json", args.tag));
    let summary_path = args.out_dir.join(format!("summary-{}.md", args.tag));
    let pretty = serde_json::to_string_pretty(&metrics)
        .map_err(|e| MnemeError::Storage(format!("encode metrics: {e}")))?;
    write_file(&metrics_path, &pretty)?;
    write_file(&summary_path, &summary)?;
    println!("{summary}");
    println!("wrote {}", metrics_path.display());
    println!("wrote {}", summary_path.display());

    let failures = failures(&metrics, &args);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(MnemeError::CheckFailed(format!(
            "evaluation failed:\n  - {}",
            failures.join("\n  - ")
        )))
    }
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, format!("{}\n", text.trim_end())).map_err(MnemeError::Io)
}

fn run(server: &mut Server, args: &EvalArgs, questions: &[Question]) -> Result<(Value, String)> {
    // Does this server's `recall` take `layers`?
    let tools = server.request("tools/list", json!({}))?;
    let recall_schema = tools["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "recall"))
        .ok_or_else(|| MnemeError::Mcp("the server lists no `recall` tool".into()))?;
    let has_layers = recall_schema["inputSchema"]["properties"]
        .get("layers")
        .is_some();
    let layer_sets: Vec<(&str, Option<Value>)> = match (args.layers, has_layers) {
        (LayersArg::Semantic, _) | (LayersArg::Auto, false) => vec![("semantic", None)],
        (LayersArg::Both, false) => {
            return Err(MnemeError::Config(
                "--layers both: this server's `recall` has no `layers` argument".into(),
            ));
        }
        (_, true) => vec![
            ("semantic", Some(json!(["semantic"]))),
            ("semantic+episodic", Some(json!(["semantic", "episodic"]))),
        ],
    };

    let before = server.read_resource("mneme://stats")?;

    // The grid.
    let mut cells = Vec::new();
    for mode in MODES {
        for scoped in [false, true] {
            for (layer_name, layers) in &layer_sets {
                let mut cell = Cell::default();
                for q in questions {
                    if scoped && q.scope.is_none() {
                        continue;
                    }
                    let mut call = json!({"query": q.question, "limit": args.limit, "mode": mode});
                    if scoped {
                        call["scope"] = json!(q.scope);
                    }
                    if let Some(l) = layers {
                        call["layers"] = l.clone();
                    }
                    let started = Instant::now();
                    let reply = server.call_tool("recall", call);
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    match reply {
                        Ok(v) => cell.add(q, &rows_from_reply(&v), ms),
                        Err(e) => {
                            cell.errors.push(format!("{}: {e}", q.id));
                            cell.add(q, &[], ms);
                        }
                    }
                }
                let mut v = cell.json();
                v["mode"] = json!(mode);
                v["scoped"] = json!(scoped);
                v["layers"] = json!(layer_name);
                cells.push(v);
            }
        }
    }

    // Context replays.
    let context = if args.no_context {
        Value::Null
    } else {
        replay_context(server, questions)?
    };

    let after = server.read_resource("mneme://stats")?;
    let (fp_before, fp_after) = (write_fingerprint(&before), write_fingerprint(&after));

    let find = |mode: &str, scoped: bool, layers: &str| {
        cells
            .iter()
            .find(|c| c["mode"] == mode && c["scoped"] == scoped && c["layers"] == layers)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let widest = layer_sets.last().map(|l| l.0).unwrap_or("semantic");
    let gate = args.gate_mode.as_str();
    let base = find(gate, false, "semantic");
    let wide = find(gate, false, widest);
    let scoped_base = find(gate, true, "semantic");
    let headline = json!({
        "mode": gate,
        "cell": "unscoped; L4 numbers from layers=semantic, `all` from the widest layer set",
        "l4_hit_at_1": base["l4"]["hit_at_1"],
        "l4_hit_at_5": base["l4"]["hit_at_5"],
        "l4_mrr": base["l4"]["mrr"],
        "all_hit_at_5": wide["all"]["hit_at_5"],
        "all_layers": widest,
        "stale_over_current": base["stale_over_current"]["count"],
        "stale_questions": base["stale_over_current"]["n"],
        "off_scope_at_5": base["off_scope_at_5"],
        "off_scope_at_5_scoped": scoped_base["off_scope_at_5"],
        "context_gold": context["gold_rate"],
        "context_stale_over_current": context["stale_over_current"]["count"],
        "context_layer_shares": context["layer_shares"],
        "recall_latency_ms": base["latency_ms"],
        "context_latency_ms": context["latency_ms"],
    });

    let metrics = json!({
        "tag": args.tag,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "mneme_version": env!("CARGO_PKG_VERSION"),
        "questions_file": args.questions.display().to_string(),
        "questions": {
            "total": questions.len(),
            "with_l4_gold": questions.iter().filter(|q| !q.gold.is_empty()).count(),
            "l3_only": questions.iter().filter(|q| q.gold.is_empty() && !q.gold_l3.is_empty()).count(),
            "with_stale": questions.iter().filter(|q| !q.stale.is_empty()).count(),
            "with_scope": questions.iter().filter(|q| q.scope.is_some()).count(),
        },
        "server": {
            "via": if args.daemon { "mneme client (running daemon)" } else { "mneme run (private, holds the data-dir lock)" },
            "recall_layers_supported": has_layers,
            "embed_dim": before["semantic_index"]["embed_dim"],
            "recall_limit": args.limit,
        },
        "headline": headline,
        "grid": cells,
        "context": context,
        "no_write_check": {
            "ok": fp_before == fp_after,
            "before": fp_before,
            "after": fp_after,
        },
    });
    let summary = render_summary(&metrics);
    Ok((metrics, summary))
}

fn replay_context(server: &mut Server, questions: &[Question]) -> Result<Value> {
    const LAYERS: [(&str, &str); 4] = [
        ("procedural", "content"),
        ("working", "content"),
        ("episodic", "payload"),
        ("semantic", "content"),
    ];
    let mut gold_n = 0usize;
    let mut gold_hits = 0usize;
    let mut missed = Vec::new();
    let mut stale_n = 0usize;
    let mut stale_over = Vec::new();
    let mut shares: Vec<Vec<f64>> = vec![Vec::new(); LAYERS.len()];
    let mut used = Vec::new();
    let mut latency = Vec::new();
    let mut event_sets = BTreeSet::new();
    let mut over_budget = Vec::new();

    for q in questions {
        let mut uri = format!("mneme://context?q={}", encode_query(&q.question));
        if let Some(scope) = &q.scope {
            uri.push_str(&format!("&scope={}", encode_query(scope)));
        }
        let started = Instant::now();
        let body = server.read_resource(&uri)?;
        latency.push(started.elapsed().as_secs_f64() * 1000.0);

        let ids_of = |section: &str| -> Vec<String> {
            body[section]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r["id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let semantic = ids_of("semantic");
        if !q.stale.is_empty() {
            stale_n += 1;
            // The semantic section is ranked: same rule as for recall.
            let ranked: Vec<Row> = semantic
                .iter()
                .map(|id| Row {
                    id: id.clone(),
                    scope: None,
                })
                .collect();
            if stale_over_current(&ranked, q) {
                stale_over.push(q.id.clone());
            }
        }
        let mut present: BTreeSet<String> = semantic.into_iter().collect();
        let events = ids_of("episodic");
        present.extend(events.iter().cloned());
        event_sets.insert(events);

        let gold = q.any_gold();
        if !gold.is_empty() {
            gold_n += 1;
            if gold.iter().any(|g| present.contains(*g)) {
                gold_hits += 1;
            } else {
                missed.push(q.id.clone());
            }
        }

        let max = body["max_tokens"].as_f64().unwrap_or(0.0);
        if max > 0.0 {
            for (i, (section, field)) in LAYERS.iter().enumerate() {
                let tokens: usize = body[*section]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|r| estimate_tokens(r[*field].as_str().unwrap_or_default()))
                            .sum()
                    })
                    .unwrap_or(0);
                shares[i].push(tokens as f64 / max);
            }
            let total = body["total_tokens"].as_f64().unwrap_or(0.0);
            used.push(total / max);
            if total > max {
                over_budget.push(q.id.clone());
            }
        }
    }

    let mean = |v: &[f64]| {
        if v.is_empty() {
            Value::Null
        } else {
            json!(round4(v.iter().sum::<f64>() / v.len() as f64))
        }
    };
    let max_of = |v: &[f64]| {
        v.iter()
            .copied()
            .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))
            .map_or(Value::Null, |m| json!(round4(m)))
    };
    let mut layer_shares = serde_json::Map::new();
    let mut layer_shares_max = serde_json::Map::new();
    for (i, (section, _)) in LAYERS.iter().enumerate() {
        layer_shares.insert((*section).to_owned(), mean(&shares[i]));
        layer_shares_max.insert((*section).to_owned(), max_of(&shares[i]));
    }
    Ok(json!({
        "n": questions.len(),
        "gold_n": gold_n,
        "gold_hits": gold_hits,
        "gold_rate": ratio(gold_hits, gold_n),
        "gold_missed": missed,
        "stale_over_current": {
            "n": stale_n,
            "count": stale_over.len(),
            "questions": stale_over,
        },
        "layer_shares": layer_shares,
        "layer_shares_max": layer_shares_max,
        "budget_used": mean(&used),
        "over_budget": over_budget,
        "distinct_event_sets": event_sets.len(),
        "latency_ms": {
            "p50": percentile(&latency, 50.0),
            "p95": percentile(&latency, 95.0),
        },
    }))
}

/// Why this run should exit non-zero. The no-write check and recall
/// errors always count; the thresholds only when given.
fn failures(metrics: &Value, args: &EvalArgs) -> Vec<String> {
    let mut out = Vec::new();
    let check = &metrics["no_write_check"];
    if check["ok"] != json!(true) {
        out.push(format!(
            "THE STORE CHANGED DURING A READ-ONLY RUN: before {} after {}. \
             If `turns_total` or the event count moved, the server is configured to record \
             reads ([working] record_tool_turns = \"all\", or [episodic] auto_log_tool_calls \
             = \"all\"); with --daemon, another client may have written",
            check["before"], check["after"]
        ));
    }
    let errors: usize = metrics["grid"]
        .as_array()
        .map(|g| {
            g.iter()
                .map(|c| c["errors"].as_array().map_or(0, Vec::len))
                .sum()
        })
        .unwrap_or(0);
    if errors > 0 {
        out.push(format!(
            "{errors} recall call(s) failed; see `errors` in the grid"
        ));
    }
    let h = &metrics["headline"];
    let mut below = |name: &str, value: &Value, min: Option<f64>| {
        if let Some(min) = min {
            match value.as_f64() {
                Some(v) if v >= min => {}
                Some(v) => out.push(format!("{name} is {v}, below the required {min}")),
                None => out.push(format!(
                    "{name} could not be measured (no such questions), required {min}"
                )),
            }
        }
    };
    below("L4 hit@5", &h["l4_hit_at_5"], args.min_l4_hit5);
    below("all hit@5", &h["all_hit_at_5"], args.min_all_hit5);
    below("context gold", &h["context_gold"], args.min_context_gold);
    if let Some(max) = args.max_stale_over_current {
        let n = h["stale_over_current"].as_u64().unwrap_or(0);
        if n > max {
            out.push(format!(
                "recall ranks a stale memory above the current one for {n} question(s), allowed {max}"
            ));
        }
    }
    if let Some(max) = args.max_context_stale_over_current {
        let n = h["context_stale_over_current"].as_u64().unwrap_or(0);
        if n > max {
            out.push(format!(
                "auto-context puts a stale memory above the current one for {n} question(s), allowed {max}"
            ));
        }
    }
    out
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "n/a".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => format!("{f:.3}"),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

fn render_summary(m: &Value) -> String {
    let h = &m["headline"];
    let q = &m["questions"];
    let mut s = String::new();
    s.push_str(&format!(
        "# mneme eval: {}\n\n",
        m["tag"].as_str().unwrap_or("")
    ));
    s.push_str(&format!(
        "mneme {} · {} · {} questions ({} with L4 gold, {} L3-only, {} with stale ids, {} scoped) · embed dim {} · via {}\n\n",
        m["mneme_version"].as_str().unwrap_or(""),
        m["generated_at"].as_str().unwrap_or(""),
        q["total"], q["with_l4_gold"], q["l3_only"], q["with_stale"], q["with_scope"],
        m["server"]["embed_dim"],
        m["server"]["via"].as_str().unwrap_or(""),
    ));
    s.push_str(&format!(
        "## Headline (mode: {}, unscoped)\n\n| Metric | Value |\n|---|---|\n",
        h["mode"].as_str().unwrap_or("")
    ));
    let rows = [
        ("L4 hit@1", show(&h["l4_hit_at_1"])),
        ("L4 hit@5", show(&h["l4_hit_at_5"])),
        ("L4 MRR", show(&h["l4_mrr"])),
        (
            "all hit@5",
            format!(
                "{} (layers: {})",
                show(&h["all_hit_at_5"]),
                h["all_layers"].as_str().unwrap_or("")
            ),
        ),
        (
            "stale over current, recall",
            format!("{} of {}", h["stale_over_current"], h["stale_questions"]),
        ),
        (
            "stale over current, auto-context",
            if m["context"].is_null() {
                "n/a".into()
            } else {
                format!(
                    "{} of {}",
                    m["context"]["stale_over_current"]["count"],
                    m["context"]["stale_over_current"]["n"]
                )
            },
        ),
        ("off-scope@5, unscoped", show(&h["off_scope_at_5"])),
        ("off-scope@5, scoped", show(&h["off_scope_at_5_scoped"])),
        (
            "context gold",
            format!(
                "{} ({} of {})",
                show(&h["context_gold"]),
                m["context"]["gold_hits"],
                m["context"]["gold_n"]
            ),
        ),
        (
            "recall latency p50 / p95 (ms)",
            format!(
                "{} / {}",
                show(&h["recall_latency_ms"]["p50"]),
                show(&h["recall_latency_ms"]["p95"])
            ),
        ),
        (
            "context latency p50 / p95 (ms)",
            format!(
                "{} / {}",
                show(&h["context_latency_ms"]["p50"]),
                show(&h["context_latency_ms"]["p95"])
            ),
        ),
    ];
    for (k, v) in rows {
        s.push_str(&format!("| {k} | {v} |\n"));
    }

    s.push_str("\n## Recall grid\n\n| Mode | Scope | Layers | L4 n | L4 hit@1 | L4 hit@5 | L4 MRR | all n | all hit@5 | stale>cur | off-scope@5 | p50 ms | p95 ms |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for c in m["grid"].as_array().into_iter().flatten() {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {} | {} | {} |\n",
            c["mode"].as_str().unwrap_or(""),
            if c["scoped"] == json!(true) {
                "on"
            } else {
                "off"
            },
            c["layers"].as_str().unwrap_or(""),
            c["l4"]["n"],
            show(&c["l4"]["hit_at_1"]),
            show(&c["l4"]["hit_at_5"]),
            show(&c["l4"]["mrr"]),
            c["all"]["n"],
            show(&c["all"]["hit_at_5"]),
            c["stale_over_current"]["count"],
            c["stale_over_current"]["n"],
            show(&c["off_scope_at_5"]),
            show(&c["latency_ms"]["p50"]),
            show(&c["latency_ms"]["p95"]),
        ));
    }
    // Per question type, for the headline mode.
    let type_cells: Vec<&Value> = m["grid"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["mode"] == h["mode"] && c["scoped"] == json!(false))
        .collect();
    for c in type_cells {
        s.push_str(&format!(
            "\n### By question type ({}, unscoped, layers: {})\n\n| Type | n | hit@1 | hit@5 | MRR |\n|---|---|---|---|---|\n",
            c["mode"].as_str().unwrap_or(""),
            c["layers"].as_str().unwrap_or("")
        ));
        for (t, v) in c["by_type"].as_object().into_iter().flatten() {
            s.push_str(&format!(
                "| {t} | {} | {} | {} | {} |\n",
                v["n"],
                show(&v["hit_at_1"]),
                show(&v["hit_at_5"]),
                show(&v["mrr"])
            ));
        }
    }
    if m["server"]["recall_layers_supported"] != json!(true) {
        s.push_str("\nThis server's `recall` has no `layers` argument, so only the semantic layer was run and L3-only questions count as misses in `all`.\n");
    }

    let c = &m["context"];
    if !c.is_null() {
        s.push_str("\n## Auto-context replay\n\n| Metric | Value |\n|---|---|\n");
        s.push_str(&format!(
            "| questions whose context holds a gold id | {} of {} |\n",
            c["gold_hits"], c["gold_n"]
        ));
        for layer in ["procedural", "working", "episodic", "semantic"] {
            s.push_str(&format!(
                "| {layer} share of budget, mean / max | {} / {} |\n",
                show(&c["layer_shares"][layer]),
                show(&c["layer_shares_max"][layer])
            ));
        }
        s.push_str(&format!(
            "| budget used, mean | {} |\n",
            show(&c["budget_used"])
        ));
        s.push_str(&format!(
            "| distinct event sets | {} of {} reads |\n",
            c["distinct_event_sets"], c["n"]
        ));
        s.push_str(&format!(
            "| reads over budget | {} |\n",
            c["over_budget"].as_array().map_or(0, Vec::len)
        ));
    }

    let w = &m["no_write_check"];
    s.push_str(&format!(
        "\n## No-write check\n\n{}: before `{}`, after `{}`.\n",
        if w["ok"] == json!(true) {
            "Passed"
        } else {
            "**FAILED, the store changed during the run**"
        },
        w["before"],
        w["after"]
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(ids: &[(&str, &str)]) -> Vec<Row> {
        ids.iter()
            .map(|(id, scope)| Row {
                id: (*id).to_owned(),
                scope: Some((*scope).to_owned()),
            })
            .collect()
    }

    fn q(gold: &[&str], gold_l3: &[&str], stale: &[&str], scope: Option<&str>) -> Question {
        Question {
            id: "q".into(),
            question: "?".into(),
            kind: None,
            scope: scope.map(str::to_owned),
            gold: gold.iter().map(|s| (*s).to_owned()).collect(),
            gold_l3: gold_l3.iter().map(|s| (*s).to_owned()).collect(),
            stale: stale.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn parses_questions_and_skips_comments() {
        let text = "# header\n\n{\"id\":\"a\",\"question\":\"x?\",\"type\":\"exact\",\"scope\":\"ops\",\"gold\":[\"m1\"]}\n{\"id\":\"b\",\"question\":\"y?\",\"gold_l3\":[\"e1\"],\"stale\":[\"m0\"]}\n";
        let qs = parse_questions(text).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].scope.as_deref(), Some("ops"));
        assert_eq!(qs[0].kind.as_deref(), Some("exact"));
        assert_eq!(qs[1].gold_l3, vec!["e1"]);
        assert_eq!(qs[1].stale, vec!["m0"]);
        assert!(qs[1].gold.is_empty());
    }

    #[test]
    fn question_errors_name_the_line() {
        let err = parse_questions("{\"id\":\"a\",\"question\":\"x\"}\nnot json\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2"), "{err}");
        let err =
            parse_questions("{\"id\":\"a\",\"question\":\"x\"}\n{\"id\":\"a\",\"question\":\"y\"}")
                .unwrap_err()
                .to_string();
        assert!(err.contains("duplicate id `a`"), "{err}");
        assert!(parse_questions("# nothing\n").is_err());
        assert!(parse_questions("{\"id\":\"a\",\"question\":\" \"}").is_err());
    }

    #[test]
    fn hit_and_mrr_use_the_first_gold_rank() {
        let mut cell = Cell::default();
        let r = rows(&[("x", "s"), ("g2", "s"), ("g1", "s")]);
        cell.add(&q(&["g1", "g2"], &[], &[], None), &r, 1.0); // rank 2
        cell.add(&q(&["x"], &[], &[], None), &r, 3.0); // rank 1
        cell.add(&q(&["nope"], &[], &[], None), &r, 2.0); // miss
        let v = cell.json();
        assert_eq!(v["l4"]["n"], 3);
        assert_eq!(v["l4"]["hit_at_1"], json!(round4(1.0 / 3.0)));
        assert_eq!(v["l4"]["hit_at_5"], json!(round4(2.0 / 3.0)));
        assert_eq!(v["l4"]["mrr"], json!(0.5));
        assert_eq!(v["latency_ms"]["p50"], json!(2.0));
        assert_eq!(v["latency_ms"]["p95"], json!(3.0));
    }

    #[test]
    fn hit_at_5_stops_at_rank_five() {
        let mut cell = Cell::default();
        let r = rows(&[
            ("a", "s"),
            ("b", "s"),
            ("c", "s"),
            ("d", "s"),
            ("e", "s"),
            ("g", "s"),
        ]);
        cell.add(&q(&["g"], &[], &[], None), &r, 1.0);
        let v = cell.json();
        assert_eq!(v["l4"]["hit_at_5"], json!(0.0));
        assert_eq!(v["l4"]["mrr"], json!(round4(1.0 / 6.0)));
    }

    #[test]
    fn cells_break_hits_down_by_type_and_list_misses() {
        let mut cell = Cell::default();
        let mut a = q(&["g"], &[], &[], None);
        a.id = "a".into();
        a.kind = Some("exact".into());
        let mut b = q(&["zzz"], &[], &[], None);
        b.id = "b".into();
        cell.add(&a, &rows(&[("g", "s")]), 1.0);
        cell.add(&b, &rows(&[("g", "s")]), 1.0);
        let v = cell.json();
        assert_eq!(v["by_type"]["exact"]["hit_at_1"], json!(1.0));
        assert_eq!(v["by_type"]["untyped"]["hit_at_5"], json!(0.0));
        assert_eq!(v["missed_at_5"], json!(["b"]));
    }

    #[test]
    fn l3_only_questions_count_in_all_but_not_in_l4() {
        let mut cell = Cell::default();
        cell.add(&q(&[], &["e1"], &[], None), &rows(&[("m", "s")]), 1.0);
        cell.add(&q(&[], &["e1"], &[], None), &rows(&[("e1", "s")]), 1.0);
        let v = cell.json();
        assert_eq!(v["l4"]["n"], 0);
        assert!(
            v["l4"]["hit_at_5"].is_null(),
            "no L4 questions: unknown, not 0"
        );
        assert_eq!(v["all"]["n"], 2);
        assert_eq!(v["all"]["hit_at_5"], json!(0.5));
    }

    #[test]
    fn stale_over_current_cases() {
        let question = q(&["cur"], &[], &["old"], None);
        assert!(stale_over_current(
            &rows(&[("old", "s"), ("cur", "s")]),
            &question
        ));
        assert!(!stale_over_current(
            &rows(&[("cur", "s"), ("old", "s")]),
            &question
        ));
        assert!(stale_over_current(
            &rows(&[("x", "s"), ("old", "s")]),
            &question
        ));
        assert!(!stale_over_current(&rows(&[("cur", "s")]), &question));
        assert!(!stale_over_current(&[], &question));
    }

    #[test]
    fn off_scope_is_a_share_of_the_top_five() {
        let r = rows(&[
            ("a", "ops"),
            ("b", "global"),
            ("c", "ops"),
            ("d", "x"),
            ("e", "ops"),
            ("f", "x"),
        ]);
        assert_eq!(off_scope_at_5(&r, "ops"), Some(0.4));
        assert_eq!(off_scope_at_5(&[], "ops"), None);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), json!(10.0));
        assert_eq!(percentile(&v, 95.0), json!(19.0));
        assert_eq!(percentile(&[7.0], 95.0), json!(7.0));
        assert!(percentile(&[], 50.0).is_null());
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode_query("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(encode_query("ops_handler-45.rs~"), "ops_handler-45.rs~");
    }

    #[test]
    fn rows_are_read_from_an_array_or_a_wrapper() {
        let a = json!([{"id": "m1", "scope": "ops"}, {"content": "no id"}]);
        assert_eq!(rows_from_reply(&a), rows(&[("m1", "ops")]));
        let o = json!({"results": [{"id": "m2", "scope": "x"}]});
        assert_eq!(rows_from_reply(&o), rows(&[("m2", "x")]));
        assert!(rows_from_reply(&json!("text")).is_empty());
    }

    #[test]
    fn fingerprint_ignores_tier_moves_but_not_writes() {
        let stats = |hot: u64, warm: u64, sem: u64, turns: u64| {
            json!({"memories": {"semantic": sem, "procedural": 1,
                                "episodic": {"hot": hot, "warm": warm}},
                   "semantic_index": {"applied_lsn": 7},
                   "working": {"turns_total": turns}})
        };
        let base = write_fingerprint(&stats(5, 5, 3, 0));
        assert_eq!(base, write_fingerprint(&stats(4, 6, 3, 0)));
        assert_ne!(base, write_fingerprint(&stats(6, 5, 3, 0)));
        assert_ne!(base, write_fingerprint(&stats(5, 5, 4, 0)));
        assert_ne!(base, write_fingerprint(&stats(5, 5, 3, 1)));
    }

    fn args() -> EvalArgs {
        EvalArgs {
            questions: "q.jsonl".into(),
            out_dir: ".".into(),
            tag: "t".into(),
            data_dir: None,
            daemon: false,
            layers: LayersArg::Auto,
            limit: 10,
            no_context: false,
            gate_mode: GateMode::Hybrid,
            max_context_stale_over_current: None,
            min_l4_hit5: None,
            min_all_hit5: None,
            max_stale_over_current: None,
            min_context_gold: None,
        }
    }

    fn metrics(ok: bool, l4: f64, stale: u64, ctx: Value) -> Value {
        json!({
            "no_write_check": {"ok": ok, "before": {"turns_total": 0}, "after": {"turns_total": 1}},
            "grid": [{"errors": []}],
            "headline": {"l4_hit_at_5": l4, "all_hit_at_5": l4,
                         "stale_over_current": stale, "context_stale_over_current": stale,
                         "context_gold": ctx},
        })
    }

    #[test]
    fn a_write_always_fails_the_run() {
        let f = failures(&metrics(false, 1.0, 0, json!(1.0)), &args());
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("THE STORE CHANGED"), "{f:?}");
        assert!(failures(&metrics(true, 0.0, 9, json!(0.0)), &args()).is_empty());
    }

    #[test]
    fn thresholds_fail_only_when_given_and_missed() {
        let mut a = args();
        a.min_l4_hit5 = Some(0.95);
        a.max_stale_over_current = Some(0);
        a.max_context_stale_over_current = Some(1);
        a.min_context_gold = Some(0.5);
        assert!(failures(&metrics(true, 0.95, 0, json!(0.5)), &a).is_empty());
        let f = failures(&metrics(true, 0.9, 2, json!(0.25)), &a);
        assert_eq!(f.len(), 4, "{f:?}");
        // A metric that could not be measured does not pass a gate.
        let f = failures(&metrics(true, 1.0, 0, Value::Null), &a);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("could not be measured"));
    }

    #[test]
    fn recall_errors_fail_the_run() {
        let mut m = metrics(true, 1.0, 0, json!(1.0));
        m["grid"] = json!([{"errors": ["q1: bad argument"]}]);
        assert_eq!(failures(&m, &args()).len(), 1);
    }

    #[test]
    fn only_recall_is_on_the_allow_list() {
        assert_eq!(READ_ONLY_TOOLS, ["recall"]);
    }
}
