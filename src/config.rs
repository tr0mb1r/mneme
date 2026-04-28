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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    #[serde(default = "default_sse_port")]
    pub sse_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetsConfig {
    #[serde(default = "default_recall_limit")]
    pub default_recall_limit: usize,
    #[serde(default = "default_auto_context_budget")]
    pub auto_context_token_budget: usize,
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
    "personal".into()
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
    dirs::home_dir()
        .unwrap_or_default()
        .join(".mneme/logs/mneme.log")
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

    /// Serialize the full config (with all defaults made explicit) to disk.
    /// Used by `mneme init` to drop a starter `config.toml` next to the
    /// user, where they can edit it.
    pub fn write(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| MnemeError::Config(format!("serialize: {e}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
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
        assert_eq!(c.scopes.default, "personal");
        assert_eq!(c.mcp.transport, "stdio");
        assert_eq!(c.mcp.sse_port, 7878);
        assert_eq!(c.budgets.default_recall_limit, 10);
        assert_eq!(c.budgets.auto_context_token_budget, 4000);
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
    fn round_trip_write_then_load() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        let c = Config::default();
        c.write(&p).unwrap();
        let loaded = Config::load(&p).unwrap();
        assert_eq!(loaded, c);
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
        assert_eq!(c.scopes.default, "personal");
    }
}
