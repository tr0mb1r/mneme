//! `mneme daemon` — v1.1 daemon entry point per ADR-0012.
//!
//! Today (A.M2 mid-series) the command:
//!
//! 1. Binds the per-data-dir Unix domain socket at
//!    `<root>/run/mneme.sock` after the stale-cleanup probe (D5).
//! 2. Logs the bind result so operators can confirm the listener
//!    came up.
//! 3. Falls through to the stdio MCP server. Connections to the
//!    socket are accepted but immediately dropped — the
//!    accept-loop + per-connection serve wiring lands in the next
//!    commit, which extracts `cli::run::async_main`'s server build
//!    into a transport-generic helper and routes `tokio::net::UnixStream`
//!    halves through it.
//!
//! The split lets reviewers see the binding logic stand on its own
//! before it's tangled with the transport refactor. systemd / launchd
//! unit files can already reference `mneme daemon` even though the
//! socket isn't serving yet — the next commit fills it in without
//! the CLI shape changing.
//!
//! Remaining A.M2 commits per ADR-0012:
//!
//! - Refactor `cli::run::async_main` to be transport-generic; wire
//!   the listener into a single-shot accept-and-serve loop that runs
//!   the existing MCP stack over the socket connection.
//! - Spawn-and-connect from `mneme run` default mode (D12).
//! - M3 onward: long-running multi-client lifecycle, idle timeout
//!   (D6), keepalive (D7), graceful shutdown.
//! - M4: auth-token verification (D3) + Windows named-pipe support
//!   (D2/D9).

use crate::Result;
use crate::storage::layout;
use crate::{MnemeError, daemon};

pub fn execute() -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;

    let listener = runtime
        .block_on(daemon::bind_listener(&root))
        .map_err(|e| MnemeError::Config(format!("daemon listener: {e}")))?;

    tracing::info!(
        socket = %listener.path().map(|p| p.display().to_string()).unwrap_or_default(),
        "mneme daemon listener bound"
    );
    tracing::info!(
        "accept-and-serve wiring lands in the next A.M2 commit; \
         falling through to stdio MCP server so clients keep working"
    );

    // Drop the listener before falling through so its RAII unlink
    // fires now rather than racing the stdio runner's signal
    // handlers. Future commit replaces this with the actual
    // accept-and-serve loop.
    drop(listener);

    crate::cli::run::execute()
}
