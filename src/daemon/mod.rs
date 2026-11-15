//! v1.1 daemon infrastructure per ADR-0012.
//!
//! This module owns the socket-binding lifecycle: opening the
//! per-platform listener, detecting + cleaning up orphaned sockets
//! from prior crashed daemons, applying owner-only file permissions,
//! and unbinding cleanly on shutdown. The MCP-over-socket serve loop
//! that consumes the listener lives in
//! [`crate::cli::run::execute_with_mode`] under
//! [`TransportMode::DaemonServeMany`](crate::cli::run::TransportMode),
//! which shares its entire boot path with `mneme run`.
//!
//! The listener is a Unix domain socket; mneme supports Linux and
//! macOS only.

use std::process::{Child, Command, Stdio};

use crate::{MnemeError, Result};

pub mod auth;

pub mod listener;

pub use listener::{Listener, ListenerError, bind_listener, socket_path, wait_for_socket};

/// Spawn a detached `mneme daemon --foreground` child process.
///
/// The child is detached from the controlling terminal via `setsid(2)`
/// so it survives the spawning process's exit. Stdio is redirected to
/// `/dev/null` — the daemon writes diagnostics to
/// `~/.mneme/logs/mneme.log`.
///
/// Returns the [`Child`] handle. The caller should immediately detach
/// by [`drop`]ping it (or in daemon mode, printing the PID and
/// exiting) — waiting on the handle would block indefinitely since
/// the daemon runs until SIGTERM / idle-timeout.
///
/// This is the shared primitive underlying both `mneme daemon`'s
/// self-detach (ADR-0012 D9) and `mneme client`'s auto-spawn protocol
/// (ADR-0012 D12).
pub fn spawn_daemon_detached() -> Result<Child> {
    let exe = std::env::current_exe().map_err(MnemeError::Io)?;
    let mut cmd = Command::new(&exe);
    cmd.arg("daemon")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.spawn()
        .map_err(|e| MnemeError::Mcp(format!("failed to spawn detached daemon: {e}")))
}
