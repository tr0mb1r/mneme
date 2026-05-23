//! Per-agent install dispatch (release-planning v2.1 §4.4).
//!
//! Tier-1 agents per the planning doc:
//!
//! - `claude-code` — fully implemented as the reference (this
//!   commit, B.M1 closing). Installs MNEME.md, the
//!   `mcpServers.mneme` entry in `settings.json`, the three
//!   lifecycle hook scripts (SessionStart / PreCompact / Stop) +
//!   their wiring in `settings.json.hooks`, and upserts the
//!   marker block in `CLAUDE.md`.
//!
//! - `claude-desktop`, `cursor` — fully implemented; ride the same
//!   `json_config::upsert_file` + standalone-MNEME.md primitives as
//!   `claude-code` for atomic, idempotent, reversible installs.
//!
//! - `cline` — stubbed; install lands in B.M3 once its
//!   path conventions are validated end-to-end on a real install.
//!
//! - `codex`, `gemini-cli` — stubbed; install lands in B.M4.
//!
//! - `opencode` — fully implemented. The original Tier-2 conditional
//!   gate ("plugin API stabilisation") referred to OpenCode's
//!   TypeScript plugin convention; this installer doesn't touch
//!   that surface (only `mcp` + `instructions[]`, both stable per
//!   OpenCode docs as of 2026-05-16), so the gate was misaligned.
//!
//! The stubs return [`AgentError::NotYetImplemented`] with the
//! tracked task number so a user who tries an unimplemented
//! agent gets a useful pointer rather than silent acceptance.

pub mod claude_code;
pub mod claude_desktop;
pub mod cursor;
pub mod opencode;

use std::path::PathBuf;

use clap::ValueEnum;
use thiserror::Error;

use super::{json_config::ConfigError, marker::MarkerError};

/// Tier-1 agents `mneme init` knows about, plus OpenCode (formerly
/// Tier-2 conditional, promoted to Tier-1 once the gate was
/// re-validated against current OpenCode docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Agent {
    /// Claude Code — global `~/.claude/` install per §4.4.
    /// Reference implementation, fully wired in B.M1's closing
    /// commit.
    #[value(name = "claude-code")]
    ClaudeCode,
    /// Claude Desktop — `~/Library/Application Support/Claude/` on
    /// macOS, platform-equivalent paths elsewhere. B.M3.
    #[value(name = "claude-desktop")]
    ClaudeDesktop,
    /// Cursor — global `~/.cursor/mcp.json` install. Daemon-multiplex
    /// via the `mneme client` bridge, same pattern as Claude Desktop.
    #[value(name = "cursor")]
    Cursor,
    /// Cline — VS Code extension; MCP config path varies. B.M3.
    #[value(name = "cline")]
    Cline,
    /// Codex (OpenAI) — `AGENTS.md` in project / home. B.M4.
    #[value(name = "codex")]
    Codex,
    /// Gemini CLI — Google. B.M4.
    #[value(name = "gemini-cli")]
    GeminiCli,
    /// OpenCode — global `~/.config/opencode/opencode.json` install.
    /// Daemon-multiplex via the `mneme client` bridge; calibration
    /// guidance auto-loads through opencode.json's `instructions[]`.
    #[value(name = "opencode")]
    OpenCode,
}

/// What `mneme init <agent>` should do — install (the default),
/// upgrade an existing install, uninstall, or show the plan
/// without writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Default: write the install. Idempotent — re-running is the
    /// same as running once (per §4.6 verification).
    Install,
    /// Same code path as `Install`; the flag exists so a future
    /// behaviour split is easy to land. Today the upgrade story
    /// is "the install IS the upgrade — every write overwrites
    /// what's there."
    Upgrade,
    /// Remove every artifact this agent's install created. No-op
    /// if nothing was installed (idempotent).
    Uninstall,
    /// Print the plan to stdout and return without writing.
    /// Useful for "what would `mneme init` do?" before committing.
    Show,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent {0:?} install is not yet implemented; tracked as {tracked}",
            tracked = .1)]
    NotYetImplemented(Agent, &'static str),
    #[error("could not resolve home directory for agent install")]
    NoHomeDir,
    #[error("config-merge error: {0}")]
    Config(#[from] ConfigError),
    #[error("marker-block error: {0}")]
    Marker(#[from] MarkerError),
    #[error("io error during agent install: {0}")]
    Io(#[from] std::io::Error),
    /// JSON shape error (e.g. attempting to construct a non-object
    /// at a settings path that already holds something else).
    #[error("agent install error: {0}")]
    Generic(String),
}

/// Install / upgrade / uninstall / show for the given agent. Reads
/// `home_dir` instead of resolving it itself so tests can pass a
/// tempdir; the CLI handler resolves via `dirs::home_dir()`.
pub fn run(agent: Agent, mode: InstallMode, home_dir: &std::path::Path) -> Result<(), AgentError> {
    match agent {
        Agent::ClaudeCode => claude_code::run(mode, home_dir),
        Agent::ClaudeDesktop => claude_desktop::run(mode, home_dir),
        Agent::Cursor => cursor::run(mode, home_dir),
        Agent::Cline => Err(AgentError::NotYetImplemented(
            agent,
            "release-planning §4.7 B.M3",
        )),
        Agent::Codex => Err(AgentError::NotYetImplemented(
            agent,
            "release-planning §4.7 B.M4",
        )),
        Agent::GeminiCli => Err(AgentError::NotYetImplemented(
            agent,
            "release-planning §4.7 B.M4",
        )),
        Agent::OpenCode => opencode::run(mode, home_dir),
    }
}

/// Resolve `~` lazily so tests can override. Returns `None` if
/// `dirs::home_dir()` fails (rare — usually means a daemon-mode
/// run with no $HOME).
pub fn default_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}
