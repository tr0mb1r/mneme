//! `mneme daemon` — v1.1 daemon entry point per ADR-0012.
//!
//! Multi-client (M3 first commit per release-planning §3.9): bind
//! `<root>/run/mneme.sock` and run a long-running accept loop. Every
//! accepted connection is spawned as its own tokio task that builds
//! a `Server` over the socket halves and runs until EOF. The accept
//! loop terminates only on SIGTERM/Ctrl-C or process death.
//! Multiple clients are served concurrently; storage writes
//! serialise through the existing single-writer seam (ADR-0012 D8).
//!
//! Self-detachment (ADR-0012 D9): when `mneme daemon` is invoked
//! without `--foreground`, the entry point spawns a detached child
//! running `mneme daemon --foreground`, prints the child PID to
//! stdout, and exits the parent with status 0. The child detaches
//! from the controlling terminal so Ctrl-C in the spawning shell
//! and shell exit do not kill it. Service-manager users
//! (systemd / launchd) pass `--foreground` to skip the detach step
//! and keep mneme attached to the manager's lifecycle.
//!
//! Pending M3 follow-ups:
//!
//! - Idle-timeout shutdown (D6) — auto-exit after
//!   `[daemon].idle_timeout_minutes` with no clients.
//! - SSE keepalive frames (D7) — periodic comments + dead-peer
//!   detection.
//! - Graceful shutdown drain (currently SIGTERM aborts in-flight
//!   spawned tasks with the runtime).
//!
//! After M3:
//!
//! - M4: auth-token verification on every connection (D3) +
//!   Windows named-pipe support (D2/D9).
//!
//! All boot work (storage, embedder, schedulers, registries) is
//! shared with `mneme run` via [`crate::cli::run::execute_with_mode`].
//! The daemon and stdio paths differ only in the
//! [`crate::cli::run::TransportMode`] they pass.

use crate::Result;
use crate::cli::run::{TransportMode, execute_with_mode};

pub fn execute(foreground: bool) -> Result<()> {
    if foreground {
        return execute_with_mode(TransportMode::DaemonServeMany);
    }
    spawn_detached()
}

/// Spawn a detached child running `mneme daemon --foreground`,
/// print the child's PID to stdout, and return without waiting on
/// it. The child runs the actual daemon loop; the parent exits
/// immediately so the user's shell prompt returns.
///
/// Delegates the platform-specific detach logic to
/// [`crate::daemon::spawn_daemon_detached`] so the same primitive is
/// shared by `mneme client`'s auto-spawn protocol (ADR-0012 D12).
fn spawn_detached() -> Result<()> {
    let child = crate::daemon::spawn_daemon_detached()?;
    // The PID line is the contract `mneme client`'s spawn protocol
    // (ADR-0012 D12) reads to know what it spawned. Keep it as a
    // single line so callers can `cmd.spawn()...read_line()`
    // without extra parsing.
    println!("{}", child.id());
    Ok(())
}
