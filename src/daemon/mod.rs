//! v1.1 daemon infrastructure per ADR-0012.
//!
//! This module owns the socket-binding lifecycle: opening the
//! per-platform listener, detecting + cleaning up orphaned sockets
//! from prior crashed daemons, applying owner-only file permissions,
//! and unbinding cleanly on shutdown. The MCP-over-socket serve loop
//! that consumes the listener lands in a follow-up A.M2 commit (it
//! requires extracting `cli::run::async_main`'s server build into a
//! transport-generic helper).
//!
//! Cross-platform note: today only Unix domain sockets are
//! implemented (`#[cfg(unix)]`). Windows named-pipe support is M4
//! (release-planning §3.9). On a non-Unix build, [`bind_listener`]
//! returns an error pointing at the M4 milestone — the daemon is
//! Unix-only until then.

pub mod auth;

#[cfg(unix)]
pub mod listener;

#[cfg(unix)]
pub use listener::{Listener, ListenerError, bind_listener, socket_path};
