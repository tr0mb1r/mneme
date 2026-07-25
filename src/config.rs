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
    pub budgets: BudgetsConfig,
    #[serde(default)]
    pub checkpoints: CheckpointsConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    /// **Reserved — has no effect.** Port for a future SSE / Streamable
    /// HTTP transport. Only `transport = "stdio"` is implemented, and
    /// `transport` itself is not consulted. Accepted so existing
    /// `config.toml` files keep loading; `mneme init` no longer writes
    /// it. Tracked in `book/src/roadmap.md`.
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
fn default_hot_to_warm_days() -> u32 {
    28
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
        let data_dir = self.storage.data_dir.display();
        let log_file = self.logging.file.display();
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
data_dir = "{data_dir}"
# Soft ceiling, in GiB, reported by `mneme stats`.
max_size_gb = {max_size_gb}

[embeddings]
# "bge-m3" (~1.5 GB, multilingual, best recall) or "minilm-l6"
# (~80 MB, English, sub-second cold start). Changing this re-embeds
# every stored memory on the next boot.
model = "{embed_model}"
# "auto" | "cpu" | "metal" | "cuda".
device = "{embed_device}"
batch_size = {batch_size}

[scopes]
# Scope that write tools use when the agent passes none.
default = "{scope_default}"
# Derive the default scope per connection from the MCP client's
# workspace roots, so a host opened on ~/code/myproj writes into scope
# "myproj". Off by default: turning it on changes where new memories
# land, so facts stored beforehand stop showing up in a default-scoped
# recall. $MNEME_SCOPE overrides both.
derive_from_roots = {derive_from_roots}

[consolidation]
# L3 episodic tier transitions, in days.
hot_to_warm_days = {hot_to_warm_days}
warm_to_cold_days = {warm_to_cold_days}
# Only "idle" is implemented: the scheduler wakes every 5 minutes and
# fires only if nothing was written in the previous window.
schedule = "{schedule}"

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

[mcp]
# Only "stdio" is implemented.
transport = "{transport}"

[daemon]
# Stop after this many minutes with no clients connected. 0 disables.
idle_timeout_minutes = {idle_timeout_minutes}
# "default" inherits [logging] level; set to e.g. "debug" to make just
# the daemon verbose.
log_level = "{daemon_log_level}"

[logging]
level = "{log_level}"
file = "{log_file}"
max_size_mb = {max_size_mb}
max_files = {max_files}
"#,
            data_dir = data_dir,
            max_size_gb = self.storage.max_size_gb,
            embed_model = self.embeddings.model,
            embed_device = self.embeddings.device,
            batch_size = self.embeddings.batch_size,
            scope_default = self.scopes.default,
            derive_from_roots = self.scopes.derive_from_roots,
            hot_to_warm_days = self.consolidation.hot_to_warm_days,
            warm_to_cold_days = self.consolidation.warm_to_cold_days,
            schedule = self.consolidation.schedule,
            session_interval_secs = self.checkpoints.session_interval_secs,
            session_interval_turns = self.checkpoints.session_interval_turns,
            hnsw_snapshot_inserts = self.checkpoints.hnsw_snapshot_inserts,
            hnsw_snapshot_minutes = self.checkpoints.hnsw_snapshot_minutes,
            default_recall_limit = self.budgets.default_recall_limit,
            auto_context_token_budget = self.budgets.auto_context_token_budget,
            max_remember_chars = self.budgets.max_remember_chars,
            transport = self.mcp.transport,
            idle_timeout_minutes = self.daemon.idle_timeout_minutes,
            daemon_log_level = self.daemon.log_level,
            log_level = self.logging.level,
            log_file = log_file,
            max_size_mb = self.logging.max_size_mb,
            max_files = self.logging.max_files,
        )
    }
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
        assert_eq!(c.mcp.transport, "stdio");
        assert_eq!(c.mcp.sse_port, 7878);
        assert_eq!(c.budgets.default_recall_limit, 10);
        assert_eq!(c.budgets.auto_context_token_budget, 4000);
        assert_eq!(c.budgets.max_remember_chars, 10_000);
        assert_eq!(c.daemon.idle_timeout_minutes, 30);
        assert_eq!(c.daemon.log_level, "default");
        assert_eq!(c.checkpoints.session_interval_secs, 30);
        assert_eq!(c.checkpoints.session_interval_turns, 5);
        assert_eq!(c.checkpoints.hnsw_snapshot_inserts, 1000);
        assert_eq!(c.checkpoints.hnsw_snapshot_minutes, 60);
        assert!(!c.telemetry.enabled);
        assert_eq!(c.logging.level, "info");
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
}
