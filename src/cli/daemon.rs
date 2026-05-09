//! `mneme daemon` — v1.1 daemon entry point per ADR-0012.
//!
//! Single-client today (M2 scope per release-planning §3.9): bind
//! `<root>/run/mneme.sock`, accept ONE client connection, serve it
//! through the existing MCP stack, exit when the client closes. The
//! listener is dropped immediately after `accept` returns so the
//! socket file is unlinked before serving begins — subsequent
//! `mneme daemon` invocations see a clean filesystem and
//! systemd-style unit files restart cleanly.
//!
//! Remaining A.M2 commit per ADR-0012:
//!
//! - Spawn-and-connect from `mneme run` default mode (D12) so MCP
//!   hosts can keep invoking `mneme run` and have it transparently
//!   start / connect to the daemon.
//!
//! After M2 (release-planning §3.9):
//!
//! - M3: long-running multi-client lifecycle, idle timeout (D6),
//!   SSE keepalive (D7), graceful shutdown.
//! - M4: auth-token verification (D3) + Windows named-pipe support
//!   (D2/D9).
//!
//! All the boot work (storage, embedder, schedulers, etc.) is
//! shared with `mneme run` via [`crate::cli::run::execute_with_mode`];
//! the daemon and stdio paths differ only in the
//! [`crate::cli::run::TransportMode`] they pass.

use crate::Result;
use crate::cli::run::{TransportMode, execute_with_mode};

pub fn execute() -> Result<()> {
    execute_with_mode(TransportMode::DaemonAcceptOne)
}
