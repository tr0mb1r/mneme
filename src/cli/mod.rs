use std::path::PathBuf;

use crate::Result;
use clap::{Parser, Subcommand};

pub mod auth;
pub mod backup;
pub mod client;
pub mod daemon;
pub mod demo;
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
pub enum AuthCommand {
    /// Generate a fresh auth token at `~/.mneme/run/auth.token`,
    /// atomically replacing any existing token. Agents that
    /// reference the file by path keep working; existing daemon
    /// connections stay valid until they re-handshake.
    Rotate,
    /// Print the auth-token path. Useful for agent config
    /// snippets that want to embed the path string.
    ShowPath,
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
    /// Start the MCP server (stdio). Right pick when the host
    /// (Claude Desktop, Cursor, etc.) spawns mneme directly as a
    /// subprocess with no shared daemon. For multi-session sharing,
    /// use `mneme daemon` + `mneme client` instead.
    Run,
    /// Start the v1.1 daemon (ADR-0012). Binds
    /// `~/.mneme/run/mneme.sock`, accepts many concurrent client
    /// connections (each gated by the `MNEME-AUTH:` handshake against
    /// `~/.mneme/run/auth.token`), serves them through the same
    /// in-process MCP `Server`. Storage writes serialise through
    /// the existing single-writer seam (D8). Auto-shuts-down after
    /// `[daemon].idle_timeout_minutes`. Pair with `mneme client`
    /// in each agent's MCP config so multiple agents share one daemon.
    ///
    /// By default the daemon **self-detaches** from the controlling
    /// terminal (ADR-0012 D9): the parent process spawns a detached
    /// child, prints the child PID, and exits 0 — the shell prompt
    /// returns immediately and Ctrl-C / shell exit do not kill the
    /// daemon. Pass `--foreground` to keep the daemon attached for
    /// systemd/launchd unit files (which manage their own lifecycle)
    /// and for debugging.
    Daemon {
        /// Run the daemon in the foreground, attached to the
        /// controlling terminal. Skips the self-detach step. Use
        /// when running under a service manager (systemd, launchd)
        /// that expects the process to stay in the foreground, or
        /// when debugging.
        #[arg(long)]
        foreground: bool,
    },
    /// Stdio↔unix-socket bridge to a running `mneme daemon`. Spawned
    /// by MCP hosts as their per-session subprocess; reads the auth
    /// token from `~/.mneme/run/auth.token`, presents it to the
    /// daemon, then byte-pipes stdin↔socket and socket↔stdout. The
    /// agent sees a normal stdio MCP server; the daemon sees a
    /// normal authenticated client. Token never lands in any agent
    /// config file (Invariant 3).
    Client,
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
    /// Print a 4-pattern walkthrough of the v1.1 memory surface
    /// (remember/recall, record_event, pin, mneme://context).
    /// Pure text — pair it with a real Claude Code session to
    /// see the patterns work end-to-end. Complements
    /// `mneme init claude-code`'s post-install prompt.
    Demo,
    /// Daemon auth-token administration (ADR-0012 D3/D4).
    /// `mneme auth rotate` regenerates the token at
    /// `~/.mneme/run/auth.token` (atomic; existing connections
    /// stay valid, the new token fires at next handshake).
    /// `mneme auth show-path` prints the path agents reference.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
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
        Command::Daemon { foreground } => daemon::execute(foreground),
        Command::Client => client::execute(),
        Command::Stats => stats::execute(),
        Command::Inspect { id, query } => inspect::execute(id, query),
        Command::Export { scope, format } => export::execute(scope, format),
        Command::Stop => stop::execute(),
        Command::Backup {
            output,
            include_models,
        } => backup::execute(output, include_models),
        Command::Demo => demo::execute(),
        Command::Auth { command } => match command {
            AuthCommand::Rotate => auth::rotate(),
            AuthCommand::ShowPath => auth::show_path(),
        },
        Command::Restore { input, force } => restore::execute(input, force),
    }
}
