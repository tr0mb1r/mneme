//! `mneme stop` — find the running server via its lockfile and signal it
//! to shut down gracefully. The server's existing `tokio::select!` loop
//! treats SIGTERM/Ctrl-C as a clean exit (see `cli/run.rs`).
//!
//! Robust to two messy real-world cases:
//!   1. The lockfile names a PID that is no longer running (stale file).
//!      We just clean it up and report.
//!   2. SIGTERM kills the server but `Drop` doesn't run (e.g., the server
//!      was shell-suspended and resumed-then-terminated). Our poll loop
//!      detects the dead PID, removes the lockfile ourselves, and reports.

use crate::storage::layout;
use crate::storage::lockfile::pid_is_alive;
use crate::{MnemeError, Result};
use std::path::Path;
use std::time::{Duration, Instant};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub fn execute() -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    let lock_path = root.join(".lock");
    if !lock_path.exists() {
        eprintln!("no running mneme found ({} missing)", lock_path.display());
        return Ok(());
    }

    let pid = read_pid(&lock_path)?;

    // Stale-lock fast path: if the recorded PID is already gone, the
    // lockfile is leftover from a process that didn't run its Drop.
    if !pid_is_alive(pid) {
        std::fs::remove_file(&lock_path)?;
        eprintln!("removed stale lockfile (PID {pid} no longer running)");
        return Ok(());
    }

    eprintln!("signalling mneme PID {pid} to shut down...");
    signal_terminate(pid)?;

    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if !lock_path.exists() {
            eprintln!("mneme PID {pid} stopped cleanly");
            return Ok(());
        }
        if !pid_is_alive(pid) {
            // Process is gone but didn't clean up its own lockfile (can
            // happen if it was shell-suspended at exit time, etc.). Take
            // care of it ourselves.
            let _ = std::fs::remove_file(&lock_path);
            eprintln!("mneme PID {pid} terminated; cleaned up stale lockfile");
            return Ok(());
        }
        if started.elapsed() >= SHUTDOWN_TIMEOUT {
            return Err(MnemeError::Lock(format!(
                "mneme PID {pid} did not exit within {SHUTDOWN_TIMEOUT:?}; \
                 try `mneme stop` again or remove {} manually",
                lock_path.display()
            )));
        }
    }
}

fn read_pid(path: &Path) -> Result<u32> {
    let text = std::fs::read_to_string(path)?;
    text.trim()
        .parse()
        .map_err(|e| MnemeError::Lock(format!("malformed PID in {path:?}: {e}")))
}

#[cfg(unix)]
fn signal_terminate(pid: u32) -> Result<()> {
    // SIGCONT first: if the target was shell-suspended (SIGTTIN/SIGTSTP'd),
    // queued signals — including the SIGTERM we're about to send — are not
    // delivered until the process is continued. Without this, `mneme stop`
    // against a backgrounded `mneme run &` would hang.
    let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) };
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc != 0 {
        return Err(MnemeError::Lock(format!(
            "kill({pid}, SIGTERM) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn signal_terminate(_pid: u32) -> Result<()> {
    Err(MnemeError::NotImplemented(
        "mneme stop on Windows is not yet implemented; use Task Manager or Stop-Process",
    ))
}

#[cfg(not(any(unix, windows)))]
fn signal_terminate(_pid: u32) -> Result<()> {
    Err(MnemeError::NotImplemented("mneme stop on this platform"))
}
