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

use crate::cli::run::{TransportMode, execute_with_mode};
use crate::{MnemeError, Result};
use std::process::{Command, Stdio};

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
/// Detachment is platform-specific: Unix uses `setsid(2)` via
/// `Command::pre_exec` so the child starts a new session with no
/// controlling terminal; Windows uses `DETACHED_PROCESS |
/// CREATE_NEW_PROCESS_GROUP` so the child does not inherit the
/// parent's console handles. Both paths redirect stdio to
/// `/dev/null` (or `NUL` on Windows) — the daemon writes its
/// diagnostics to `~/.mneme/logs/mneme.log` (post-v1.1.x logging
/// fix) so the user does not need stderr-on-terminal to see what
/// the daemon is doing.
fn spawn_detached() -> Result<()> {
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
        // SAFETY: `setsid` is async-signal-safe and is the
        // canonical Unix idiom for detaching a forked child from
        // the parent's controlling terminal. `pre_exec` runs after
        // fork() but before exec(), in the child's address space —
        // exactly where this call belongs.
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
        // Windows analogues of Unix `setsid` + closed stdio.
        // DETACHED_PROCESS — child has no console; parent's
        //   console window does not bind it.
        // CREATE_NEW_PROCESS_GROUP — Ctrl-C / Ctrl-Break in the
        //   parent shell does not propagate to the daemon.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let child = cmd
        .spawn()
        .map_err(|e| MnemeError::Mcp(format!("failed to spawn detached daemon: {e}")))?;

    // The PID line is the contract `mneme client`'s spawn protocol
    // (ADR-0012 D12) reads to know what it spawned. Keep it as a
    // single line so callers can `cmd.spawn()...read_line()`
    // without extra parsing.
    println!("{}", child.id());
    Ok(())
}
