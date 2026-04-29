//! Exclusive lockfile (`~/.mneme/.lock`) preventing two `mneme run`
//! instances from sharing one data directory.
//!
//! The file body holds the PID of the holder, so a second instance can
//! produce a clear error message (`"lock held by PID N"`). Stale locks
//! (PID file present but the process is dead) are reclaimed automatically
//! on Unix; on Windows we surface a clear error and leave the file alone.

use crate::{MnemeError, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Holder for the active lock. Releases on drop.
#[derive(Debug)]
pub struct LockGuard {
    file: File,
    path: PathBuf,
}

impl LockGuard {
    /// Try to acquire an exclusive lock on `path`. On contention, returns
    /// `MnemeError::Lock` with the holder PID. Reclaims stale locks (Unix).
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open (or create) the file with read+write so we can both inspect
        // the PID and update it.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        // First attempt.
        match file.try_lock_exclusive() {
            Ok(()) => {
                write_pid(&mut file)?;
                Ok(LockGuard {
                    file,
                    path: path.to_path_buf(),
                })
            }
            Err(_) => {
                // Lock held — see who's holding it.
                let holder = read_pid(&mut file).unwrap_or(0);
                if holder != 0 && pid_is_alive(holder) {
                    return Err(MnemeError::Lock(format!("lock held by PID {holder}")));
                }
                // Stale lock — try to reclaim by force-locking. We can't
                // unlock another process's hold, so try_lock_exclusive will
                // still fail until the OS releases the prior holder. The
                // typical scenario is a crashed process that never released
                // the OS-level flock; on Linux/macOS the kernel releases
                // flocks when the file descriptor is closed, which happens
                // on process exit. So if the holder is dead, the OS lock
                // is already gone — try once more.
                match file.try_lock_exclusive() {
                    Ok(()) => {
                        write_pid(&mut file)?;
                        Ok(LockGuard {
                            file,
                            path: path.to_path_buf(),
                        })
                    }
                    Err(_) => Err(MnemeError::Lock(format!(
                        "lock held by stale PID {holder} that the OS still considers alive"
                    ))),
                }
            }
        }
    }

    /// Path of the underlying lockfile.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort: unlock and remove the file. The OS releases flocks
        // on process exit, but we proactively unlink so other processes
        // don't have to chase a stale PID file.
        let _ = FileExt::unlock(&self.file);
        if let Err(e) = std::fs::remove_file(&self.path) {
            // Visible warning — silent failure here was the source of a
            // confusing "lockfile won't go away" bug during Phase 2 dogfood.
            tracing::warn!(
                error = %e,
                path = %self.path.display(),
                "failed to remove lockfile during Drop"
            );
        }
    }
}

fn write_pid(file: &mut File) -> Result<()> {
    let pid = std::process::id();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{pid}")?;
    file.sync_all()?;
    Ok(())
}

fn read_pid(file: &mut File) -> Option<u32> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut s = String::new();
    file.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

/// True iff `pid` names a running process (or one we don't have permission
/// to signal — conservatively treated as alive).
#[cfg(unix)]
pub fn pid_is_alive(pid: u32) -> bool {
    // kill(pid, 0) returns 0 if the process exists (and we have permission
    // to signal it), or -1 with errno=ESRCH if it doesn't.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno != libc::ESRCH
}

#[cfg(windows)]
pub fn pid_is_alive(_pid: u32) -> bool {
    // Conservative: assume any holder is alive on Windows. Stale lockfiles
    // require manual cleanup on this platform.
    true
}

#[cfg(not(any(unix, windows)))]
pub fn pid_is_alive(_pid: u32) -> bool {
    true
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_acquirer_wins() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".lock");
        let guard = LockGuard::acquire(&path).unwrap();
        assert!(path.exists());

        // On Unix, file locks are advisory — the PID written by
        // `acquire` is readable through the path while the lock is
        // still held. On Windows, fs2's exclusive lock prevents
        // even the holding process from opening the file by path
        // (the kernel returns ERROR_LOCK_VIOLATION). The PID is
        // still written; we just can't read it back without
        // releasing the lock first.
        #[cfg(unix)]
        {
            let contents = std::fs::read_to_string(&path).unwrap();
            let pid: u32 = contents.trim().parse().unwrap();
            assert_eq!(pid, std::process::id());
        }

        drop(guard);
        // After drop, file is removed.
        assert!(!path.exists());
    }

    #[test]
    fn second_acquirer_sees_clear_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".lock");
        let _g = LockGuard::acquire(&path).unwrap();
        let err = LockGuard::acquire(&path).unwrap_err();
        match err {
            MnemeError::Lock(msg) => {
                assert!(msg.contains("PID"), "message must mention PID, got: {msg}");
            }
            other => panic!("expected MnemeError::Lock, got {other:?}"),
        }
    }
}
