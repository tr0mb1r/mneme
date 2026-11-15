//! TOML-backed configuration mirroring spec §9.
//!
//! `Config::load(path)` reads `~/.mneme/config.toml` and merges any present
//! fields over the spec's defaults. Missing sections and fields fall back
//! to the defaults via `#[serde(default)]`.

use crate::{MnemeError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
    #[serde(default)]
    pub scopes: ScopesConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub budgets: BudgetsConfig,
    #[serde(default)]
    pub guardrails: GuardrailsConfig,
    #[serde(default)]
    pub checkpoints: CheckpointsConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub episodic: EpisodicConfig,
    #[serde(default)]
    pub auto_context: AutoContextConfig,
    #[serde(default)]
    pub working: WorkingConfig,
    #[serde(default)]
    pub recall: RecallConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: u64,
    /// **Reserved — has no effect.** Encryption at rest is gated by the
    /// presence of `keystore.json` (run `mneme encrypt`), not by this
    /// flag. Accepted so existing `config.toml` files keep loading;
    /// `mneme init` no longer writes it. Tracked in
    /// `book/src/roadmap.md`.
    #[serde(default)]
    pub encryption: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_embed_device")]
    pub device: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsolidationConfig {
    #[serde(default = "default_hot_to_warm_days")]
    pub hot_to_warm_days: u32,
    #[serde(default = "default_warm_to_cold_days")]
    pub warm_to_cold_days: u32,
    #[serde(default = "default_consolidation_schedule")]
    pub schedule: String,
    /// Episodic kinds that never leave the warm tier for the cold
    /// archive, so they stay in redb and stay searchable. `[]` restores
    /// the pre-1.6 behaviour (every kind ages into cold).
    #[serde(default = "default_cold_exempt_kinds")]
    pub cold_exempt_kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScopesConfig {
    #[serde(default = "default_scope")]
    pub default: String,
    /// Derive the per-connection default scope from the MCP client's
    /// declared workspace roots (see [`crate::scope::scope_from_roots`]),
    /// so a host opened on `~/code/myproj` writes into scope `myproj`
    /// without the agent calling `switch_scope`.
    ///
    /// **Defaults to `false`**, and deliberately so: turning it on
    /// changes where new memories land, which means facts stored before
    /// the switch stop appearing in a default-scoped `recall`. That is
    /// the right behaviour for someone who wants project isolation and a
    /// nasty surprise for someone who doesn't, so it is opt-in.
    ///
    /// An explicit `MNEME_SCOPE` in the environment wins over
    /// derivation — explicit beats inferred.
    #[serde(default)]
    pub derive_from_roots: bool,
}

/// Which successful `tools/call` invocations the server mirrors into
/// the L3 timeline as `tool_call` events (ADR-0009).
///
/// The classification of "read" vs "write" comes from each tool's own
/// `readOnlyHint` annotation, not a name list, so a newly added tool is
/// sorted automatically.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AutoLogToolCalls {
    /// Mirror nothing. `tool_call_failed` events are suppressed too.
    Off,
    /// Mirror every tool that is not `readOnlyHint: true`. `forget` is a
    /// write, so deletions stay auditable. Reads (`recall`, `stats`, ...)
    /// leave no event. The default.
    #[default]
    Writes,
    /// Mirror every successful call, reads included (pre-1.5 behaviour).
    All,
}

impl AutoLogToolCalls {
    /// Whether a *successful* call to a tool with the given
    /// `readOnlyHint` is mirrored as a `tool_call` event.
    ///
    /// Only `writes` mode consults `read_only`.
    pub fn logs_success(self, read_only: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Writes => !read_only,
            Self::All => true,
        }
    }

    /// Whether failures are mirrored as `tool_call_failed` events.
    /// Every mode except `off` keeps them: a failed call is rare and is
    /// exactly what you want on the timeline when debugging.
    pub fn logs_failures(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The config-file spelling, for the starter template.
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Writes => "writes",
            Self::All => "all",
        }
    }
}

/// How much of a tool call's arguments a `tool_call` event keeps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogToolCallValues {
    /// The argument values as passed (content included). The default,
    /// for auditability (ADR-0009); `forget` redacts matching rows.
    #[default]
    Full,
    /// Tool name, ids, scope, and a short hash + length of the content.
    Ids,
    /// Tool name only.
    Off,
}

impl LogToolCallValues {
    /// The config-file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Ids => "ids",
            Self::Off => "off",
        }
    }
}

/// `[episodic]` -- L3 timeline behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EpisodicConfig {
    /// Which tool calls are auto-logged as `tool_call` events. See
    /// [`AutoLogToolCalls`]. Defaults to `writes`.
    pub auto_log_tool_calls: AutoLogToolCalls,
    /// Event kinds whose text is indexed for keyword search
    /// (`recall(layers=[..,"episodic"])`, query-conditioned auto-context).
    /// `tool_call` rows are deliberately absent.
    pub index_kinds: Vec<String>,
    /// Whether episodic rows returned by `recall` refresh their
    /// `last_accessed`, so events that keep being used stay hot.
    pub touch_on_recall: bool,
    /// How much of the arguments a `tool_call` event keeps.
    pub log_tool_call_values: LogToolCallValues,
}

impl Default for EpisodicConfig {
    fn default() -> Self {
        Self {
            auto_log_tool_calls: AutoLogToolCalls::default(),
            index_kinds: [
                "decision",
                "milestone",
                "lesson",
                "summary",
                "resolution",
                "problem",
                "pivot",
                "observation",
                "archived",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            touch_on_recall: true,
            log_tool_call_values: LogToolCallValues::default(),
        }
    }
}

/// `[auto_context]` -- how `mneme://context` scores and packs layers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AutoContextConfig {
    /// Half-life, in days, of L4 freshness measured on `updated_at`.
    /// `0` turns L4 decay off. Auto-context only; `recall` is undecayed.
    pub semantic_half_life_days: f32,
    /// Lower bound of the L4 freshness factor, so age only breaks
    /// near-ties in relevance.
    pub semantic_recency_floor: f32,
    /// Largest share of the token budget episodic events may take.
    pub episodic_max_share: f32,
    /// Share of the budget reserved for semantic rows (capped by
    /// `semantic_min_items` rows, whichever is smaller in tokens).
    pub semantic_min_share: f32,
    /// Row count alternative to `semantic_min_share`.
    pub semantic_min_items: usize,
    /// Largest share of the budget working-session turns may take.
    pub working_max_share: f32,
    /// β: the weight of recency-only versus query match for events.
    pub episodic_query_mix: f32,
    /// Events are clipped to this many characters server-side and
    /// charged at the clipped size.
    pub event_clip_chars: usize,
    /// Events whose retrieval weight is at or below this are skipped.
    pub min_event_weight: f32,
    /// At most this many events of one kind.
    pub max_events_per_kind: usize,
    /// Procedural scopes always included alongside the requested ones.
    /// `[]` restores the pre-1.6 behaviour (pins of the requested scope only).
    pub pin_scopes: Vec<String>,
    /// Include memories demoted by a newer `refines`/`contradicts`.
    pub include_demoted: bool,
}

impl Default for AutoContextConfig {
    fn default() -> Self {
        Self {
            semantic_half_life_days: 90.0,
            semantic_recency_floor: 0.75,
            episodic_max_share: 0.35,
            semantic_min_share: 0.30,
            semantic_min_items: 5,
            working_max_share: 0.05,
            episodic_query_mix: 0.3,
            event_clip_chars: 400,
            min_event_weight: 0.3,
            max_events_per_kind: 6,
            pin_scopes: vec!["global".to_owned()],
            include_demoted: false,
        }
    }
}

/// Which successful tool calls push an L1 working turn.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecordToolTurns {
    /// Only tools that are not `readOnlyHint`. The default.
    #[default]
    Writes,
    /// Every successful call (pre-1.5.2 behaviour).
    All,
    /// None.
    Off,
}

impl RecordToolTurns {
    /// The config-file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Writes => "writes",
            Self::All => "all",
            Self::Off => "off",
        }
    }

    /// Whether a successful call to a tool with this `readOnlyHint`
    /// pushes a working turn.
    pub fn records(self, read_only: bool) -> bool {
        match self {
            Self::Writes => !read_only,
            Self::All => true,
            Self::Off => false,
        }
    }
}

/// `[working]` -- L1 working-session behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkingConfig {
    pub record_tool_turns: RecordToolTurns,
}

/// Keyword-index stemming.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Stemming {
    /// Exact tokens only. The default.
    #[default]
    Off,
    /// English stems, added as extra tokens for purely alphabetic words of
    /// four or more characters; ids and paths are unchanged.
    En,
}

/// `[recall]` -- `recall` ranking knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecallConfig {
    pub stemming: Stemming,
    /// Rank penalty (cosine scale) for a memory refined or contradicted
    /// by a current, newer memory.
    pub demote_margin: f32,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            stemming: Stemming::default(),
            demote_margin: 0.10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    /// **Reserved — has no effect.** Superseded by `[http] bind`, which
    /// configures the Streamable HTTP transport. `transport` itself is
    /// informational: the command (`run` / `daemon` / `serve`) picks the
    /// transport. Both keys are accepted so existing `config.toml` files
    /// keep loading; `mneme init` no longer writes `sse_port`.
    #[serde(default = "default_sse_port")]
    pub sse_port: u16,
}

/// `[daemon]` — daemon-mode tuning per ADR-0012. Fully wired since
/// v1.1.1: `mneme daemon` binds a Unix socket, gates each connection on
/// the auth handshake, serves many clients concurrently, drains on
/// SIGTERM, and auto-stops after `idle_timeout_minutes`. Windows
/// named-pipe support is still outstanding (see
/// `book/src/roadmap.md`), so on Windows only `mneme run` works.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonConfig {
    /// Idle-timeout shutdown threshold (ADR-0012 D6). The daemon
    /// stops after no clients have been connected for this long. `0`
    /// disables the timeout — the daemon then only stops via
    /// `mneme stop` / SIGTERM. Counted from "last client
    /// disconnected", not "last request seen".
    #[serde(default = "default_daemon_idle_timeout_minutes")]
    pub idle_timeout_minutes: u64,
    /// Daemon-only log level override. Falls back to the global
    /// `[logging] level` when set to `"default"`. Lets users turn
    /// the daemon up to `debug` without making the rest of the
    /// binary chatty.
    #[serde(default = "default_daemon_log_level")]
    pub log_level: String,
}

/// `[http]` — the MCP Streamable HTTP listener. `mneme serve` always
/// runs it; `mneme daemon` runs it next to the Unix socket only when
/// `enabled = true`. Each field can be overridden from the environment
/// (`MNEME_HTTP_BIND`, `MNEME_HTTP_TOKEN` / `MNEME_HTTP_TOKEN_FILE`,
/// `MNEME_HTTP_ALLOWED_ORIGINS`) so a container needs no config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpConfig {
    /// Also open the HTTP listener when running `mneme daemon`.
    #[serde(default)]
    pub enabled: bool,
    /// `host:port` to listen on. Loopback by default; a container
    /// sets `0.0.0.0:7878` so other services on its network can reach it.
    #[serde(default = "default_http_bind")]
    pub bind: String,
    /// File holding the bearer token clients must send. Empty means
    /// `<data_dir>/run/auth.token` (the daemon's token, created on
    /// first use). `$MNEME_HTTP_TOKEN` beats both.
    #[serde(default)]
    pub token_file: String,
    /// Browser `Origin` values to accept. Agents don't send `Origin`;
    /// leave empty unless a browser-based client needs access.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Cap on concurrently open MCP sessions.
    #[serde(default = "default_http_max_sessions")]
    pub max_sessions: usize,
    /// Drop sessions with no traffic for this many minutes. Clients
    /// then re-initialize transparently.
    #[serde(default = "default_http_session_idle_minutes")]
    pub session_idle_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetsConfig {
    #[serde(default = "default_recall_limit")]
    pub default_recall_limit: usize,
    #[serde(default = "default_auto_context_budget")]
    pub auto_context_token_budget: usize,
    /// Hard ceiling on `remember` / `update` content length, in
    /// characters. Above this, the tool returns a structured error
    /// (release-planning v2.1 §5.4) suggesting the agent extract a
    /// key insight or summarize. Existing oversized memories remain
    /// readable — the verbatim principle is preserved; only new
    /// writes/updates are rejected.
    #[serde(default = "default_max_remember_chars")]
    pub max_remember_chars: usize,
}

/// `[guardrails]` — write-time checks applied by `remember`, `update`,
/// `pin` and `record_event`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GuardrailsConfig {
    /// What to do when a write contains a credential in a
    /// high-confidence format (AWS key id, PEM private key, JWT, GitHub
    /// / Slack / OpenAI / Anthropic / Google / Stripe tokens, age
    /// secret key). `reject` (default) refuses the write with a
    /// `secret_detected` error; `warn` stores it and adds a
    /// `secret_advisory`; `off` skips the scan entirely.
    #[serde(default)]
    pub secrets: SecretsMode,
}

/// `[guardrails] secrets` setting.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretsMode {
    /// Refuse the write. Default.
    #[default]
    Reject,
    /// Store the write and attach a `secret_advisory`.
    Warn,
    /// No scan.
    Off,
}

impl SecretsMode {
    /// The value as written in `config.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            SecretsMode::Reject => "reject",
            SecretsMode::Warn => "warn",
            SecretsMode::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckpointsConfig {
    #[serde(default = "default_session_interval_secs")]
    pub session_interval_secs: u64,
    #[serde(default = "default_session_interval_turns")]
    pub session_interval_turns: u32,
    #[serde(default = "default_hnsw_snapshot_inserts")]
    pub hnsw_snapshot_inserts: u64,
    #[serde(default = "default_hnsw_snapshot_minutes")]
    pub hnsw_snapshot_minutes: u32,
}

/// `[telemetry]` — **reserved, has no effect.** No telemetry subsystem
/// exists; these fields are accepted and round-tripped but nothing reads
/// them, and mneme makes no network calls on any code path. Accepted so
/// existing `config.toml` files keep loading; `mneme init` no longer
/// writes the section. Tracked in `book/src/roadmap.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_file")]
    pub file: PathBuf,
    #[serde(default = "default_log_max_size_mb")]
    pub max_size_mb: u32,
    #[serde(default = "default_log_max_files")]
    pub max_files: u32,
}

// ---------- Defaults ----------

fn default_data_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".mneme")
}
fn default_max_size_gb() -> u64 {
    10
}
fn default_embed_model() -> String {
    "bge-m3".into()
}
fn default_embed_device() -> String {
    "auto".into()
}
fn default_batch_size() -> usize {
    32
}
/// Default `[consolidation] hot_to_warm_days`. Public so the
/// episodic store can fall back to it when no config is threaded in.
pub const DEFAULT_HOT_TO_WARM_DAYS: u32 = 28;

fn default_cold_exempt_kinds() -> Vec<String> {
    ["archived", "summary", "milestone", "lesson", "decision"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}
fn default_hot_to_warm_days() -> u32 {
    DEFAULT_HOT_TO_WARM_DAYS
}
fn default_warm_to_cold_days() -> u32 {
    180
}
fn default_consolidation_schedule() -> String {
    "idle".into()
}
fn default_scope() -> String {
    "global".into()
}
fn default_mcp_transport() -> String {
    "stdio".into()
}
fn default_sse_port() -> u16 {
    7878
}
fn default_recall_limit() -> usize {
    10
}
fn default_auto_context_budget() -> usize {
    4000
}
fn default_max_remember_chars() -> usize {
    10_000
}
fn default_daemon_idle_timeout_minutes() -> u64 {
    30
}
fn default_daemon_log_level() -> String {
    "default".into()
}
fn default_http_bind() -> String {
    "127.0.0.1:7878".into()
}
fn default_http_max_sessions() -> usize {
    256
}
fn default_http_session_idle_minutes() -> u64 {
    24 * 60
}
fn default_session_interval_secs() -> u64 {
    30
}
fn default_session_interval_turns() -> u32 {
    5
}
fn default_hnsw_snapshot_inserts() -> u64 {
    1000
}
fn default_hnsw_snapshot_minutes() -> u32 {
    60
}
fn default_log_level() -> String {
    "info".into()
}
fn default_log_file() -> PathBuf {
    // Honor MNEME_DATA_DIR by going through layout::default_root —
    // a tester / power user pointing the data dir elsewhere expects
    // logs to follow. Falls back to `~/.mneme/logs/mneme.log` only
    // if the home dir lookup itself fails.
    crate::storage::layout::default_root()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".mneme"))
        .join("logs/mneme.log")
}
fn default_log_max_size_mb() -> u32 {
    100
}
fn default_log_max_files() -> u32 {
    5
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            max_size_gb: default_max_size_gb(),
            encryption: false,
        }
    }
}
impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: default_embed_model(),
            device: default_embed_device(),
            batch_size: default_batch_size(),
        }
    }
}
impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            hot_to_warm_days: default_hot_to_warm_days(),
            warm_to_cold_days: default_warm_to_cold_days(),
            schedule: default_consolidation_schedule(),
            cold_exempt_kinds: default_cold_exempt_kinds(),
        }
    }
}
impl Default for ScopesConfig {
    fn default() -> Self {
        Self {
            default: default_scope(),
            derive_from_roots: false,
        }
    }
}
impl Default for McpConfig {
    fn default() -> Self {
        Self {
            transport: default_mcp_transport(),
            sse_port: default_sse_port(),
        }
    }
}
impl Default for BudgetsConfig {
    fn default() -> Self {
        Self {
            default_recall_limit: default_recall_limit(),
            auto_context_token_budget: default_auto_context_budget(),
            max_remember_chars: default_max_remember_chars(),
        }
    }
}
impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_minutes: default_daemon_idle_timeout_minutes(),
            log_level: default_daemon_log_level(),
        }
    }
}
impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_http_bind(),
            token_file: String::new(),
            allowed_origins: Vec::new(),
            max_sessions: default_http_max_sessions(),
            session_idle_minutes: default_http_session_idle_minutes(),
        }
    }
}
impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self {
            session_interval_secs: default_session_interval_secs(),
            session_interval_turns: default_session_interval_turns(),
            hnsw_snapshot_inserts: default_hnsw_snapshot_inserts(),
            hnsw_snapshot_minutes: default_hnsw_snapshot_minutes(),
        }
    }
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: default_log_file(),
            max_size_mb: default_log_max_size_mb(),
            max_files: default_log_max_files(),
        }
    }
}

/// Render `s` as a correctly-quoted TOML string value, quotes included.
///
/// Delegates to `toml::Value`'s own writer rather than wrapping the text
/// in `"` by hand, because the right *form* depends on the content:
/// `toml` emits a literal string (`'C:\Users\me\.mneme'`) when the value
/// contains backslashes, a multi-line literal when it also contains an
/// apostrophe, and a basic string otherwise. Hand-quoting produced a
/// `config.toml` that mneme could not read back on Windows.
fn toml_value(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

// ---------- I/O ----------

impl Config {
    /// Load from disk. Missing file returns `Default::default()`. Missing
    /// sections fall back to defaults via serde.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| MnemeError::Config(format!("{path:?}: {e}")))
    }

    /// Like [`Config::load`], but also reports whether the file was
    /// actually present (`true`) or absent (`false` — in which case the
    /// returned config is all built-in defaults).
    ///
    /// The daemon uses the `present` flag to emit a boot-time warning
    /// when it's silently running on defaults (see the troubleshooting
    /// note "config.toml is missing"). Plain [`Config::load`] stays
    /// side-effect-free so the pre-subscriber logging bootstrap
    /// (`main::load_logging_config`) and `stats` / `inspect` don't warn.
    pub fn load_reporting(path: &Path) -> Result<(Self, bool)> {
        let present = path.exists();
        Ok((Self::load(path)?, present))
    }

    /// The effective file-log level. When running the daemon
    /// (`is_daemon`) and `[daemon] log_level` is set to something other
    /// than the `"default"` sentinel, that override wins; otherwise the
    /// global `[logging] level` applies. Lets the daemon run verbose
    /// without making short-lived CLI commands (`stats`, `inspect`, …)
    /// chatty.
    pub fn effective_file_log_level(&self, is_daemon: bool) -> &str {
        let daemon_level = self.daemon.log_level.trim();
        if is_daemon && !daemon_level.is_empty() && daemon_level != "default" {
            daemon_level
        } else {
            self.logging.level.as_str()
        }
    }

    /// Serialize the full config (with all defaults made explicit) to disk.
    ///
    /// Emits *every* field, including the reserved ones that have no
    /// effect. Kept for round-tripping and programmatic use; `mneme init`
    /// uses [`write_starter`](Self::write_starter) instead so users don't
    /// get handed live-looking settings that do nothing.
    pub fn write(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| MnemeError::Config(format!("serialize: {e}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Write the commented starter `config.toml` that `mneme init` drops
    /// for a new user.
    ///
    /// Differs from [`write`](Self::write) in two ways that matter:
    ///
    /// 1. **Reserved keys are omitted.** `[telemetry]`,
    ///    `[mcp] sse_port`, and `[storage] encryption` are accepted on
    ///    load but do nothing, and shipping them in every user's config
    ///    invited "I set `telemetry.enabled = false` and it still…"
    ///    reports. Deserialization still accepts them, so existing files
    ///    keep working untouched.
    /// 2. **It carries comments.** A serialized struct cannot; a
    ///    hand-written template can say what each knob does.
    ///
    /// Values are interpolated from `self` rather than hardcoded, so the
    /// template cannot drift from the real defaults — and
    /// `starter_template_matches_defaults` fails the build if the shape
    /// ever does.
    pub fn write_starter(&self, path: &Path) -> Result<()> {
        let text = self.starter_toml();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    fn starter_toml(&self) -> String {
        // Every string value goes through `toml_value`, never a bare
        // `format!("\"{x}\"")`. A Windows `data_dir` such as
        // `C:\Users\me\.mneme` inside a TOML *basic* string makes
        // `\U` an 8-digit unicode escape, so the file mneme itself just
        // wrote fails to parse on the next boot. Caught by
        // `starter_template_survives_windows_style_paths`.
        let data_dir = toml_value(&self.storage.data_dir.display().to_string());
        let log_file = toml_value(&self.logging.file.display().to_string());
        let embed_model = toml_value(&self.embeddings.model);
        let embed_device = toml_value(&self.embeddings.device);
        let scope_default = toml_value(&self.scopes.default);
        let schedule = toml_value(&self.consolidation.schedule);
        let auto_log_tool_calls = toml_value(self.episodic.auto_log_tool_calls.as_str());
        let str_list = |items: &[String]| {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|i| toml_value(i))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let index_kinds = str_list(&self.episodic.index_kinds);
        let cold_exempt_kinds = str_list(&self.consolidation.cold_exempt_kinds);
        let pin_scopes = str_list(&self.auto_context.pin_scopes);
        let log_tool_call_values = toml_value(self.episodic.log_tool_call_values.as_str());
        let record_tool_turns = toml_value(self.working.record_tool_turns.as_str());
        let transport = toml_value(&self.mcp.transport);
        let daemon_log_level = toml_value(&self.daemon.log_level);
        let http_bind = toml_value(&self.http.bind);
        let http_token_file = toml_value(&self.http.token_file);
        let http_allowed_origins = format!(
            "[{}]",
            self.http
                .allowed_origins
                .iter()
                .map(|o| toml_value(o))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let log_level = toml_value(&self.logging.level);
        format!(
            r#"# mneme configuration. Every value below is the built-in
# default, written out so you can see and edit it.
#
# Settings that are accepted but have NO EFFECT in this release are
# deliberately not listed here; see book/src/roadmap.md for what is
# reserved and why.

[storage]
# Where mneme keeps everything. Override with $MNEME_DATA_DIR to point
# a test or a second profile elsewhere.
data_dir = {data_dir}
# Soft ceiling, in GiB, reported by `mneme stats`.
max_size_gb = {max_size_gb}

[embeddings]
# "bge-m3" (~1.5 GB, multilingual, best recall) or "minilm-l6"
# (~80 MB, English, sub-second cold start). Changing this re-embeds
# every stored memory on the next boot.
model = {embed_model}
# "auto" | "cpu" | "metal" | "cuda".
device = {embed_device}
batch_size = {batch_size}

[scopes]
# Scope that write tools use when the agent passes none.
default = {scope_default}
# Derive the default scope per connection from the MCP client's
# workspace roots, so a host opened on ~/code/myproj writes into scope
# "myproj". Off by default: turning it on changes where new memories
# land, so facts stored beforehand stop showing up in a default-scoped
# recall. $MNEME_SCOPE overrides both.
derive_from_roots = {derive_from_roots}

[episodic]
# Which tool calls are mirrored into the timeline as `tool_call` events.
#   "writes" - every tool except read-only ones (recall, stats, ...);
#              `forget` and other writes stay auditable. Default.
#   "all"    - also log reads.
#   "off"    - log nothing, failures included.
# Failed calls (`tool_call_failed`) are logged in "writes" and "all".
auto_log_tool_calls = {auto_log_tool_calls}
# How much of a call's arguments a `tool_call` event keeps.
#   "full" - the values as passed, `content` included. Default. `forget`
#            redacts the events that mirrored a forgotten memory.
#   "ids"  - tool name, ids and scope; `content` is replaced by its
#            length and an 8-hex-digit SHA-256 prefix. Queries, kinds and
#            limits are dropped.
#   "off"  - the tool name only.
log_tool_call_values = {log_tool_call_values}
# Event kinds whose text is indexed for keyword search, so `recall` with
# `layers` and query-seeded auto-context can find them. `tool_call` rows
# are deliberately not indexed.
index_kinds = {index_kinds}
# Refresh `last_accessed` on events that `recall` returns, so events that
# keep being used stay in the hot tier. Set false for read-only runs
# (evaluations, CI) that must not write.
touch_on_recall = {touch_on_recall}

[working]
# Which successful tool calls add a turn to the L1 working session.
#   "writes" - tools that change something. Default. Lookups (recall,
#              recall_recent, stats, ...) are not conversation.
#   "all"    - every call (the behaviour up to 1.5.1).
#   "off"    - none; only messages recorded with `record_event` are turns.
record_tool_turns = {record_tool_turns}

[auto_context]
# How mneme://context scores and packs the layers. Shares are fractions
# of [budgets] auto_context_token_budget.
#
# Long-term memories lose rank with age, measured from their last
# update: half-life in days (0 = no decay) and the floor the factor
# never goes below, so age only breaks near-ties in relevance. `recall`
# itself is not decayed.
semantic_half_life_days = {semantic_half_life_days}
semantic_recency_floor = {semantic_recency_floor}
# Largest share events (L3) may take, and the share reserved for
# long-term memories (L4) -- or `semantic_min_items` rows, whichever is
# smaller in tokens.
episodic_max_share = {episodic_max_share}
semantic_min_share = {semantic_min_share}
semantic_min_items = {semantic_min_items}
# Largest share working-session turns (L1) may take.
working_max_share = {working_max_share}
# With a query: weight of plain recency (0.0 = rank events by query
# match only, 1.0 = by recency only).
episodic_query_mix = {episodic_query_mix}
# Events are clipped to this many characters and charged at that size.
event_clip_chars = {event_clip_chars}
# Events at or below this retrieval weight are left out.
min_event_weight = {min_event_weight}
# At most this many events of one kind, so one chatty kind cannot fill
# the section.
max_events_per_kind = {max_events_per_kind}
# Pin scopes always included next to the requested scope, so a scoped
# read keeps the global rules. [] = only the requested scope's pins.
pin_scopes = {pin_scopes}

[consolidation]
# L3 episodic tier transitions, in days.
hot_to_warm_days = {hot_to_warm_days}
warm_to_cold_days = {warm_to_cold_days}
# Only "idle" is implemented: the scheduler wakes every 5 minutes and
# fires only if nothing was written in the previous window.
schedule = {schedule}
# Event kinds that stay in the warm tier instead of moving to the cold
# archive, so they remain searchable and reachable by `forget`.
# [] = every kind ages into cold.
cold_exempt_kinds = {cold_exempt_kinds}

[checkpoints]
# L1 working-session flush cadence: whichever trigger fires first.
session_interval_secs = {session_interval_secs}
session_interval_turns = {session_interval_turns}
# L4 HNSW snapshot cadence: whichever fires first. Snapshots bound how
# much WAL the next boot has to replay.
hnsw_snapshot_inserts = {hnsw_snapshot_inserts}
hnsw_snapshot_minutes = {hnsw_snapshot_minutes}

[budgets]
# Default `limit` for the recall tool.
default_recall_limit = {default_recall_limit}
# Token ceiling for the mneme://context resource.
auto_context_token_budget = {auto_context_token_budget}
# Hard ceiling on remember/update content length, in characters.
# Writes above this are rejected; existing longer memories stay
# readable.
max_remember_chars = {max_remember_chars}

[guardrails]
# What `remember`, `update`, `pin` and `record_event` do when a write
# contains a credential in a high-confidence format (AWS key id, PEM
# private key, JWT, GitHub / Slack / OpenAI / Anthropic / Google /
# Stripe token, age secret key).
#   "reject" - refuse the write with a `secret_detected` error that
#              names the pattern and offset, never the secret.
#   "warn"   - store it and add a `secret_advisory` to the reply.
#   "off"    - no scan at all.
secrets = {secrets}

[mcp]
# Informational. Transports are picked by command: `mneme run`
# (stdio), `mneme daemon` (Unix socket, plus HTTP when [http] enabled),
# `mneme serve` (HTTP only).
transport = {transport}

[daemon]
# Stop after this many minutes with no clients connected. 0 disables.
idle_timeout_minutes = {idle_timeout_minutes}
# "default" inherits [logging] level; set to e.g. "debug" to make just
# the daemon verbose.
log_level = {daemon_log_level}

[http]
# MCP Streamable HTTP listener, for agents that connect over the
# network (e.g. another container). `mneme serve` always runs it;
# `mneme daemon` runs it alongside the Unix socket when enabled.
enabled = {http_enabled}
bind = {http_bind}
# Bearer-token file. Empty = <data_dir>/run/auth.token. The
# MNEME_HTTP_TOKEN env var overrides it.
token_file = {http_token_file}
allowed_origins = {http_allowed_origins}
max_sessions = {http_max_sessions}
session_idle_minutes = {http_session_idle_minutes}

[logging]
level = {log_level}
file = {log_file}
max_size_mb = {max_size_mb}
max_files = {max_files}
"#,
            data_dir = data_dir,
            max_size_gb = self.storage.max_size_gb,
            embed_model = embed_model,
            embed_device = embed_device,
            batch_size = self.embeddings.batch_size,
            scope_default = scope_default,
            derive_from_roots = self.scopes.derive_from_roots,
            auto_log_tool_calls = auto_log_tool_calls,
            log_tool_call_values = log_tool_call_values,
            index_kinds = index_kinds,
            touch_on_recall = self.episodic.touch_on_recall,
            record_tool_turns = record_tool_turns,
            semantic_half_life_days = toml_float(self.auto_context.semantic_half_life_days),
            semantic_recency_floor = toml_float(self.auto_context.semantic_recency_floor),
            episodic_max_share = toml_float(self.auto_context.episodic_max_share),
            semantic_min_share = toml_float(self.auto_context.semantic_min_share),
            semantic_min_items = self.auto_context.semantic_min_items,
            working_max_share = toml_float(self.auto_context.working_max_share),
            episodic_query_mix = toml_float(self.auto_context.episodic_query_mix),
            event_clip_chars = self.auto_context.event_clip_chars,
            min_event_weight = toml_float(self.auto_context.min_event_weight),
            max_events_per_kind = self.auto_context.max_events_per_kind,
            pin_scopes = pin_scopes,
            cold_exempt_kinds = cold_exempt_kinds,
            hot_to_warm_days = self.consolidation.hot_to_warm_days,
            warm_to_cold_days = self.consolidation.warm_to_cold_days,
            schedule = schedule,
            session_interval_secs = self.checkpoints.session_interval_secs,
            session_interval_turns = self.checkpoints.session_interval_turns,
            hnsw_snapshot_inserts = self.checkpoints.hnsw_snapshot_inserts,
            hnsw_snapshot_minutes = self.checkpoints.hnsw_snapshot_minutes,
            default_recall_limit = self.budgets.default_recall_limit,
            auto_context_token_budget = self.budgets.auto_context_token_budget,
            max_remember_chars = self.budgets.max_remember_chars,
            secrets = toml_value(self.guardrails.secrets.as_str()),
            transport = transport,
            idle_timeout_minutes = self.daemon.idle_timeout_minutes,
            daemon_log_level = daemon_log_level,
            http_enabled = self.http.enabled,
            http_bind = http_bind,
            http_token_file = http_token_file,
            http_allowed_origins = http_allowed_origins,
            http_max_sessions = self.http.max_sessions,
            http_session_idle_minutes = self.http.session_idle_minutes,
            log_level = log_level,
            log_file = log_file,
            max_size_mb = self.logging.max_size_mb,
            max_files = self.logging.max_files,
        )
    }
}

/// Render an `f32` as a TOML float that parses back to the same `f32`.
/// `{:?}` gives the shortest such decimal and always carries a `.` or an
/// exponent, so `90.0` does not become the integer `90`.
fn toml_float(v: f32) -> String {
    format!("{v:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.storage.max_size_gb, 10);
        assert_eq!(c.embeddings.model, "bge-m3");
        assert_eq!(c.embeddings.device, "auto");
        assert_eq!(c.embeddings.batch_size, 32);
        assert_eq!(c.consolidation.hot_to_warm_days, 28);
        assert_eq!(c.consolidation.warm_to_cold_days, 180);
        assert_eq!(c.scopes.default, "global");
        assert_eq!(c.episodic.auto_log_tool_calls, AutoLogToolCalls::Writes);
        assert_eq!(c.mcp.transport, "stdio");
        assert_eq!(c.mcp.sse_port, 7878);
        assert_eq!(c.budgets.default_recall_limit, 10);
        assert_eq!(c.budgets.auto_context_token_budget, 4000);
        assert_eq!(c.budgets.max_remember_chars, 10_000);
        assert_eq!(c.guardrails.secrets, SecretsMode::Reject);
        assert_eq!(c.daemon.idle_timeout_minutes, 30);
        assert_eq!(c.daemon.log_level, "default");
        assert!(!c.http.enabled);
        assert_eq!(c.http.bind, "127.0.0.1:7878");
        assert_eq!(c.http.token_file, "");
        assert!(c.http.allowed_origins.is_empty());
        assert_eq!(c.http.max_sessions, 256);
        assert_eq!(c.http.session_idle_minutes, 1440);
        assert_eq!(c.checkpoints.session_interval_secs, 30);
        assert_eq!(c.checkpoints.session_interval_turns, 5);
        assert_eq!(c.checkpoints.hnsw_snapshot_inserts, 1000);
        assert_eq!(c.checkpoints.hnsw_snapshot_minutes, 60);
        assert!(!c.telemetry.enabled);
        assert_eq!(c.logging.level, "info");
    }

    #[test]
    fn auto_log_tool_calls_policy() {
        use AutoLogToolCalls::{All, Off, Writes};
        assert!(!Off.logs_success(false));
        assert!(!Off.logs_success(true));
        assert!(Writes.logs_success(false));
        assert!(!Writes.logs_success(true));
        assert!(All.logs_success(false));
        assert!(All.logs_success(true));
        assert!(!Off.logs_failures());
        assert!(Writes.logs_failures());
        assert!(All.logs_failures());
    }

    #[test]
    fn auto_log_tool_calls_parses_every_mode_and_round_trips() {
        for (text, want) in [
            ("off", AutoLogToolCalls::Off),
            ("writes", AutoLogToolCalls::Writes),
            ("all", AutoLogToolCalls::All),
        ] {
            let c: Config =
                toml::from_str(&format!("[episodic]\nauto_log_tool_calls = \"{text}\"\n")).unwrap();
            assert_eq!(c.episodic.auto_log_tool_calls, want);

            let mut c = Config::default();
            c.episodic.auto_log_tool_calls = want;
            let tmp = TempDir::new().unwrap();
            let p = tmp.path().join("config.toml");
            c.write_starter(&p).unwrap();
            assert_eq!(Config::load(&p).unwrap(), c, "starter round-trip: {text}");
            c.write(&p).unwrap();
            assert_eq!(
                Config::load(&p).unwrap(),
                c,
                "full write round-trip: {text}"
            );
        }
    }

    #[test]
    fn auto_log_tool_calls_rejects_unknown_value() {
        let r: std::result::Result<Config, _> =
            toml::from_str("[episodic]\nauto_log_tool_calls = \"sometimes\"\n");
        assert!(r.is_err());
    }

    #[test]
    fn missing_episodic_section_defaults_to_writes() {
        let c: Config = toml::from_str("[scopes]\ndefault = \"x\"\n").unwrap();
        assert_eq!(c.episodic.auto_log_tool_calls, AutoLogToolCalls::Writes);
    }

    #[test]
    fn guardrails_secrets_parses_every_mode_and_rejects_unknown() {
        for (text, want) in [
            ("reject", SecretsMode::Reject),
            ("warn", SecretsMode::Warn),
            ("off", SecretsMode::Off),
        ] {
            let c: Config = toml::from_str(&format!("[guardrails]\nsecrets = \"{text}\"\n"))
                .expect("valid mode parses");
            assert_eq!(c.guardrails.secrets, want);
            assert_eq!(want.as_str(), text);
        }
        assert!(toml::from_str::<Config>("[guardrails]\nsecrets = \"nope\"\n").is_err());
        // Absent section or key falls back to reject.
        let c: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(c.guardrails.secrets, SecretsMode::Reject);
    }

    #[test]
    fn guardrails_non_default_mode_round_trips_through_starter() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        let mut c = Config::default();
        c.guardrails.secrets = SecretsMode::Warn;
        c.write_starter(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    #[test]
    fn load_missing_file_uses_defaults() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("absent.toml");
        let c = Config::load(&p).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn load_reporting_flags_missing_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("absent.toml");
        let (c, present) = Config::load_reporting(&p).unwrap();
        assert!(!present, "absent file must report present=false");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn load_reporting_flags_present_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        Config::default().write(&p).unwrap();
        let (c, present) = Config::load_reporting(&p).unwrap();
        assert!(present, "existing file must report present=true");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn daemon_log_level_overrides_file_level_only_for_daemon() {
        let mut c = Config::default();
        c.logging.level = "info".into();
        c.daemon.log_level = "debug".into();
        assert_eq!(c.effective_file_log_level(true), "debug");
        assert_eq!(c.effective_file_log_level(false), "info");
    }

    #[test]
    fn daemon_log_level_default_sentinel_inherits_global() {
        let mut c = Config::default();
        c.logging.level = "warn".into();
        c.daemon.log_level = "default".into();
        assert_eq!(c.effective_file_log_level(true), "warn");
        assert_eq!(c.effective_file_log_level(false), "warn");
    }

    #[test]
    fn round_trip_write_then_load() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        let c = Config::default();
        c.write(&p).unwrap();
        let loaded = Config::load(&p).unwrap();
        assert_eq!(loaded, c);
    }

    /// The starter template is hand-written, so it could drift from the
    /// real defaults. This makes drift a build failure: parse the
    /// template and demand it round-trips to `Config::default()`.
    #[test]
    fn starter_template_matches_defaults() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        let defaults = Config::default();
        defaults.write_starter(&p).unwrap();
        let loaded = Config::load(&p).unwrap();
        assert_eq!(
            loaded, defaults,
            "the starter config.toml no longer parses back to Config::default() — \
             a field was added, renamed, or given a new default without updating \
             Config::starter_toml"
        );
    }

    /// Regression: the starter template must survive a Windows path.
    ///
    /// `starter_toml` interpolated paths into TOML *basic* strings, where
    /// `\U` in `C:\Users\...` is an 8-digit unicode escape — so
    /// `mneme init` on Windows wrote a `config.toml` that the next
    /// `mneme run` refused to parse ("invalid unicode 8-digit hex code").
    ///
    /// Injects the backslash paths explicitly instead of relying on the
    /// host's real defaults, so this fails on Linux and macOS too. The
    /// original `starter_template_matches_defaults` only caught it on a
    /// Windows runner, which is a whole CI round-trip too late.
    /// `allowed_origins` is the only list-valued key in the template;
    /// make sure a non-empty one renders as valid TOML.
    #[test]
    fn starter_template_round_trips_http_settings() {
        let mut c = Config::default();
        c.http.enabled = true;
        c.http.bind = "0.0.0.0:9000".into();
        c.http.token_file = "/run/secrets/mneme_token".into();
        c.http.allowed_origins = vec!["http://localhost:3000".into(), "https://a\"b".into()];

        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        c.write_starter(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    /// Every 1.6 key the template lists must carry a non-default value
    /// through a write and a load, lists and floats included.
    #[test]
    fn starter_template_round_trips_the_1_6_sections() {
        let mut c = Config::default();
        c.episodic.log_tool_call_values = LogToolCallValues::Ids;
        c.episodic.index_kinds = vec!["decision".into(), "it's".into()];
        c.episodic.touch_on_recall = false;
        c.working.record_tool_turns = RecordToolTurns::All;
        c.consolidation.cold_exempt_kinds = vec![];
        c.auto_context = AutoContextConfig {
            semantic_half_life_days: 0.0,
            semantic_recency_floor: 0.6,
            episodic_max_share: 0.25,
            semantic_min_share: 0.4,
            semantic_min_items: 7,
            working_max_share: 0.1,
            episodic_query_mix: 1.0,
            event_clip_chars: 200,
            min_event_weight: 0.05,
            max_events_per_kind: 3,
            pin_scopes: vec![],
            include_demoted: false,
        };

        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        c.write_starter(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    /// The template documents each 1.6 section, and leaves out the keys
    /// that do nothing until 1.7 (relations) or are still experimental.
    #[test]
    fn starter_template_lists_the_1_6_sections() {
        let text = Config::default().starter_toml();
        for expected in [
            "[working]",
            "record_tool_turns = \"writes\"",
            "[auto_context]",
            "pin_scopes = [\"global\"]",
            "semantic_half_life_days = 90.0",
            "semantic_recency_floor = 0.75",
            "log_tool_call_values = \"full\"",
            "touch_on_recall = true",
            "index_kinds = [\"decision\"",
            "cold_exempt_kinds = [\"archived\"",
        ] {
            assert!(text.contains(expected), "starter config lacks `{expected}`");
        }
        for reserved in ["include_demoted", "demote_margin", "[recall]", "stemming"] {
            assert!(
                !text.contains(reserved),
                "starter config lists `{reserved}`, which has no effect yet"
            );
        }
    }

    #[test]
    fn dispatch_enums_parse_their_config_spellings() {
        for (text, values, turns) in [
            ("full", LogToolCallValues::Full, RecordToolTurns::Writes),
            ("ids", LogToolCallValues::Ids, RecordToolTurns::All),
            ("off", LogToolCallValues::Off, RecordToolTurns::Off),
        ] {
            let doc = format!(
                "[episodic]\nlog_tool_call_values = \"{text}\"\n\
                 [working]\nrecord_tool_turns = \"{}\"\n",
                turns.as_str()
            );
            let c: Config = toml::from_str(&doc).unwrap();
            assert_eq!(c.episodic.log_tool_call_values, values);
            assert_eq!(values.as_str(), text);
            assert_eq!(c.working.record_tool_turns, turns);
        }
        assert!(toml::from_str::<Config>("[working]\nrecord_tool_turns = \"reads\"\n").is_err());
        assert!(RecordToolTurns::Writes.records(false) && !RecordToolTurns::Writes.records(true));
        assert!(RecordToolTurns::All.records(true) && !RecordToolTurns::Off.records(false));
    }

    #[test]
    fn starter_template_survives_windows_style_paths() {
        let mut c = Config::default();
        c.storage.data_dir = PathBuf::from(r"C:\Users\runneradmin\.mneme");
        c.logging.file = PathBuf::from(r"C:\Users\runneradmin\.mneme\logs\mneme.log");

        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        c.write_starter(&p).unwrap();
        let loaded = Config::load(&p).expect("starter config must parse back");

        assert_eq!(loaded.storage.data_dir, c.storage.data_dir);
        assert_eq!(loaded.logging.file, c.logging.file);
        assert_eq!(loaded, c);
    }

    /// Same hazard, nastier input: an apostrophe rules out a plain TOML
    /// literal string, so the writer has to reach for a multi-line
    /// literal. A user called `O'Brien` is not a hypothetical.
    #[test]
    fn starter_template_survives_paths_with_quotes_and_backslashes() {
        let mut c = Config::default();
        c.storage.data_dir = PathBuf::from(r"C:\Users\O'Brien\.mneme");
        c.logging.file = PathBuf::from(r"C:\Users\O'Brien\.mneme\logs\mneme.log");

        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        c.write_starter(&p).unwrap();
        let loaded = Config::load(&p).expect("starter config must parse back");
        assert_eq!(loaded, c);
    }

    #[test]
    fn toml_value_picks_a_form_that_round_trips() {
        for raw in [
            r"C:\Users\runneradmin\.mneme",
            r"C:\Users\O'Brien\.mneme",
            "/home/user/.mneme",
            "has \"double quotes\"",
            "has\ttab",
        ] {
            let rendered = toml_value(raw);
            let doc = format!("v = {rendered}");
            let parsed: toml::Value =
                toml::from_str(&doc).unwrap_or_else(|e| panic!("{raw:?} → {rendered} → {e}"));
            assert_eq!(
                parsed["v"].as_str().unwrap(),
                raw,
                "{raw:?} did not round-trip through {rendered}"
            );
        }
    }

    /// Reserved settings must not appear in what `mneme init` writes —
    /// handing a user a knob that does nothing is worse than omitting it.
    #[test]
    fn starter_template_omits_reserved_settings() {
        let text = Config::default().starter_toml();
        for reserved in ["[telemetry]", "sse_port", "encryption ="] {
            assert!(
                !text.contains(reserved),
                "starter config leaks reserved setting `{reserved}`"
            );
        }
    }

    /// …but they must still *load*, so an existing config.toml written
    /// by v1.0-v1.2 keeps working untouched.
    #[test]
    fn reserved_settings_still_deserialize() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("legacy.toml");
        std::fs::write(
            &p,
            "[storage]\nencryption = true\n\n[mcp]\nsse_port = 9999\n\n\
             [telemetry]\nenabled = true\nendpoint = \"https://example.invalid\"\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.storage.encryption);
        assert_eq!(c.mcp.sse_port, 9999);
        assert!(c.telemetry.enabled);
        // And unrelated fields still fall back to defaults.
        assert_eq!(c.embeddings.model, "bge-m3");
    }

    #[test]
    fn partial_file_inherits_defaults() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("partial.toml");
        std::fs::write(&p, "[storage]\nmax_size_gb = 50\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.storage.max_size_gb, 50);
        // Other fields fall back to defaults.
        assert_eq!(c.embeddings.model, "bge-m3");
        assert_eq!(c.scopes.default, "global");
    }

    #[test]
    fn v1_6_sections_default_and_parse() {
        let c = Config::default();
        assert_eq!(c.auto_context.semantic_recency_floor, 0.75);
        assert_eq!(c.auto_context.pin_scopes, vec!["global".to_owned()]);
        assert_eq!(c.working.record_tool_turns, RecordToolTurns::Writes);
        assert_eq!(c.episodic.log_tool_call_values, LogToolCallValues::Full);
        assert!(c.episodic.touch_on_recall);
        assert!(c.episodic.index_kinds.iter().any(|k| k == "decision"));
        assert!(!c.episodic.index_kinds.iter().any(|k| k == "tool_call"));
        assert_eq!(c.recall.stemming, Stemming::Off);

        let parsed: Config = toml::from_str(
            "[auto_context]\npin_scopes = []\nevent_clip_chars = 200\n\
             [working]\nrecord_tool_turns = \"all\"\n\
             [episodic]\nlog_tool_call_values = \"ids\"\n\
             [recall]\nstemming = \"en\"\n\
             [consolidation]\ncold_exempt_kinds = []\n",
        )
        .expect("parse");
        assert!(parsed.auto_context.pin_scopes.is_empty());
        assert_eq!(parsed.auto_context.event_clip_chars, 200);
        assert_eq!(parsed.auto_context.episodic_max_share, 0.35);
        assert_eq!(parsed.working.record_tool_turns, RecordToolTurns::All);
        assert_eq!(parsed.episodic.log_tool_call_values, LogToolCallValues::Ids);
        assert_eq!(
            parsed.episodic.auto_log_tool_calls,
            AutoLogToolCalls::Writes
        );
        assert_eq!(parsed.recall.stemming, Stemming::En);
        assert!(parsed.consolidation.cold_exempt_kinds.is_empty());
        assert!(RecordToolTurns::Writes.records(false));
        assert!(!RecordToolTurns::Writes.records(true));
    }
}
