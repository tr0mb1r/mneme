//! `mneme daemon` — v1.1 daemon entry point per ADR-0012.
//!
//! Today this command is a thin wrapper around [`crate::cli::run::execute`],
//! so functionally `mneme daemon` ≡ `mneme run` (stdio MCP server).
//! It exists at the CLI surface from the first M2 commit so:
//!
//! 1. systemd / launchd unit files can reference a stable command
//!    name (`ExecStart=/usr/local/bin/mneme daemon`) without churn
//!    as M2-M5 land the SSE transport in stages.
//! 2. ADR-0012 D12's spawn-and-connect flow has a target to invoke
//!    once the SSE transport lands — `mneme run` (no flags) will
//!    spawn `mneme daemon` via the same binary path then poll the
//!    socket.
//! 3. Documentation, tests, and integrations can be written against
//!    the final command surface even though the implementation is
//!    iterating underneath.
//!
//! Subsequent A.M2 commits replace the body with:
//!
//! - Daemonization (D9: double-fork-and-setsid on Unix,
//!   `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows).
//! - Orphaned socket cleanup (D5).
//! - Listener bind on `~/.mneme/run/mneme.sock` (Unix) or
//!   `\\.\pipe\mneme-{user_sid}` (Windows) (D2).
//! - SSE transport (D1) instead of stdio.
//! - Auth-token verification on every client connection (D3).
//! - Idle-timeout shutdown (D6) and SSE keepalive (D7).
//!
//! For now: stdio runner. Sufficient to land the surface today and
//! iterate on the transport in following commits without breaking
//! the public CLI shape.

use crate::Result;

pub fn execute() -> Result<()> {
    tracing::info!(
        "`mneme daemon` is currently a stdio passthrough; SSE transport \
         lands in subsequent A.M2 commits per ADR-0012"
    );
    crate::cli::run::execute()
}
