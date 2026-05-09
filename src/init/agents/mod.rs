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
//! - `claude-desktop`, `cursor`, `cline` — stubbed; install lands
//!   in B.M3 once each agent's path conventions are validated
//!   end-to-end on a real install.
//!
//! - `codex`, `gemini-cli` — stubbed; install lands in B.M4.
//!
//! - `opencode` — Tier-2 conditional per ADR-0012 / planning
//!   §4.4; deferred outright until the OpenCode plugin API
//!   stabilises.
//!
//! The stubs return [`AgentError::NotYetImplemented`] with the
//! tracked task number so a user who tries an unimplemented
//! agent gets a useful pointer rather than silent acceptance.

pub mod claude_code;

use std::path::PathBuf;

use clap::ValueEnum;
use thiserror::Error;

use super::{json_config::ConfigError, marker::MarkerError};

/// Tier-1 agents `mneme init` knows about. OpenCode is omitted —
/// it ships if-and-only-if its plugin API stabilises by the M4
/// milestone (ADR-0012 / release-planning §4.4 Tier-2 conditional).
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
    /// Cursor — global `~/.cursor/` or workspace `.cursor/`. B.M3.
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
        Agent::ClaudeDesktop => Err(AgentError::NotYetImplemented(
            agent,
            "release-planning §4.7 B.M3",
        )),
        Agent::Cursor => Err(AgentError::NotYetImplemented(
            agent,
            "release-planning §4.7 B.M3",
        )),
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
    }
}

/// Resolve `~` lazily so tests can override. Returns `None` if
/// `dirs::home_dir()` fails (rare — usually means a daemon-mode
/// run with no $HOME).
pub fn default_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}
