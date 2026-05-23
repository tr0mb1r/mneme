//! `mneme init` — two modes:
//!
//! - **No agent** (v1.0 behaviour, retained): scaffold the on-disk
//!   `~/.mneme/` layout, write a default `config.toml`, run the
//!   schema migration. Idempotent.
//! - **`mneme init <agent>`** (v1.1 / B.M1): install mneme into
//!   the named agent's configuration via the
//!   [`crate::init::agents`] dispatch. `claude-code` is fully
//!   wired today; the other Tier-1 agents land per
//!   release-planning §4.7 B.M3-M4. `--upgrade`, `--uninstall`,
//!   and `--show` flags supported.

use crate::config::Config;
use crate::init::agents::{self, Agent, InstallMode};
use crate::storage::layout;
use crate::{MnemeError, Result, migrate};
use std::path::Path;

pub fn execute(agent: Option<Agent>, upgrade: bool, uninstall: bool, show: bool) -> Result<()> {
    match agent {
        None => {
            // v1.0 behaviour: scaffold only. Reject the flags that
            // only make sense per-agent so the user gets a clear
            // error rather than silent ignore.
            if upgrade || uninstall || show {
                return Err(MnemeError::Config(
                    "--upgrade / --uninstall / --show require an <agent> argument; \
                     run `mneme init claude-code --help` for the per-agent install"
                        .into(),
                ));
            }
            scaffold_only()
        }
        Some(agent) => {
            let mode = if uninstall {
                InstallMode::Uninstall
            } else if show {
                InstallMode::Show
            } else if upgrade {
                InstallMode::Upgrade
            } else {
                InstallMode::Install
            };
            install_agent(agent, mode)
        }
    }
}

fn scaffold_only() -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    init_at(&root)
}

fn install_agent(agent: Agent, mode: InstallMode) -> Result<()> {
    let home = agents::default_home_dir().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for agent install".into())
    })?;
    agents::run(agent, mode, &home).map_err(|e| MnemeError::Config(e.to_string()))
}

pub fn init_at(root: &Path) -> Result<()> {
    layout::scaffold(root)?;
    migrate::migrate_to(root, migrate::CURRENT_SCHEMA_VERSION)?;
    let config_path = root.join("config.toml");
    if !config_path.exists() {
        Config::default().write(&config_path)?;
        eprintln!("wrote default config to {}", config_path.display());
    } else {
        eprintln!("config already present at {}", config_path.display());
    }
    eprintln!(
        "mneme initialized at {} (schema_version={})",
        root.display(),
        migrate::CURRENT_SCHEMA_VERSION
    );
    Ok(())
}
