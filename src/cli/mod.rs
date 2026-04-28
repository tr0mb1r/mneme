use crate::Result;
use clap::{Parser, Subcommand};

pub mod export;
pub mod init;
pub mod inspect;
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
    /// Scaffold ~/.mneme and download embedding model
    Init,
    /// Start the MCP server
    Run,
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
}

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => init::execute(),
        Command::Run => run::execute(),
        Command::Stats => stats::execute(),
        Command::Inspect { id, query } => inspect::execute(id, query),
        Command::Export { scope, format } => export::execute(scope, format),
        Command::Stop => stop::execute(),
    }
}
