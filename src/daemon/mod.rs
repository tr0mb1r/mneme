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

use std::process::{Child, Command, Stdio};

use crate::{MnemeError, Result};

pub mod auth;

#[cfg(unix)]
pub mod listener;

#[cfg(unix)]
pub use listener::{Listener, ListenerError, bind_listener, socket_path, wait_for_socket};

/// Spawn a detached `mneme daemon --foreground` child process.
///
/// The child is detached from the controlling terminal via `setsid(2)`
/// (Unix) or `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` (Windows)
/// so it survives the spawning process's exit. Stdio is redirected to
/// `/dev/null` (or `NUL` on Windows) — the daemon writes diagnostics
/// to `~/.mneme/logs/mneme.log`.
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

    #[cfg(unix)]
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

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .map_err(|e| MnemeError::Mcp(format!("failed to spawn detached daemon: {e}")))
}
