use std::path::PathBuf;

use crate::Result;
use clap::{Parser, Subcommand};

pub mod backup;
pub mod daemon;
pub mod export;
pub mod init;
pub mod inspect;
pub mod restore;
pub mod run;
pub mod stats;
pub mod stop;

#[derive(Parser, Debug)]
#[command(
    name = "mneme",
    version,
    about = "Persistent memory tool for AI agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scaffold `~/.mneme/`, or install mneme into a specific
    /// agent (`claude-code` is fully wired today; the other Tier-1
    /// agents land per release-planning §4.7 B.M3-M4).
    Init {
        /// Target agent. Omit for v1.0 scaffold-only behaviour
        /// (writes ~/.mneme/, schema migration, default config).
        #[arg(value_enum)]
        agent: Option<crate::init::agents::Agent>,
        /// Re-run the install, overwriting mneme-owned files. Same
        /// effect as plain install today (per §4.6 the install is
        /// the upgrade); the flag exists so a future behaviour
        /// split is easy to land.
        #[arg(long, conflicts_with_all = ["uninstall", "show"])]
        upgrade: bool,
        /// Reverse the install: remove every artifact mneme created.
        /// Idempotent — safe to re-run.
        #[arg(long, conflicts_with_all = ["upgrade", "show"])]
        uninstall: bool,
        /// Print the install plan to stdout without writing
        /// anything. Useful for "what would `mneme init <agent>`
        /// do?" before committing.
        #[arg(long, conflicts_with_all = ["upgrade", "uninstall"])]
        show: bool,
    },
    /// Start the MCP server
    Run,
    /// Start the v1.1 daemon (M2-M5 of release-planning §3.9 land
    /// the SSE transport in stages; today this command shares the
    /// stdio runner with `mneme run` and exists primarily as the
    /// stable entry point for systemd/launchd unit files and for
    /// the future client-spawn-and-connect flow per ADR-0012 D12).
    Daemon,
    /// Show memory health and size
    Stats,
    /// Inspect a memory by id or query
    Inspect {
        /// Memory id (ULID)
        id: Option<String>,
        /// Or a search query
        #[arg(long)]
        query: Option<String>,
    },
    /// Export memories for backup
    Export {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// Stop a running mneme instance
    Stop,
    /// Tar+gzip the data directory to a single file
    Backup {
        /// Output path for the .tar.gz archive
        output: PathBuf,
        /// Include the model cache (~1-2 GB depending on the model)
        #[arg(long)]
        include_models: bool,
    },
    /// Restore a `mneme backup`-produced archive into the data directory
    Restore {
        /// Path to the .tar.gz archive
        input: PathBuf,
        /// Overwrite an already-populated data directory
        #[arg(long)]
        force: bool,
    },
}

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init {
            agent,
            upgrade,
            uninstall,
            show,
        } => init::execute(agent, upgrade, uninstall, show),
        Command::Run => run::execute(),
        Command::Daemon => daemon::execute(),
        Command::Stats => stats::execute(),
        Command::Inspect { id, query } => inspect::execute(id, query),
        Command::Export { scope, format } => export::execute(scope, format),
        Command::Stop => stop::execute(),
        Command::Backup {
            output,
            include_models,
        } => backup::execute(output, include_models),
        Command::Restore { input, force } => restore::execute(input, force),
    }
}
