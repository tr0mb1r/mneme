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

pub fn execute() -> Result<()> {
    execute_with_mode(TransportMode::DaemonServeMany)
}
