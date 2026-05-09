//! `mneme daemon` (without `--foreground`) self-detaches per
//! ADR-0012 D9 — parent exits after spawning, the child runs the
//! actual daemon loop in a new session/process-group with no
//! controlling terminal.
//!
//! Unix-only for now: the detach idiom uses `setsid(2)` via
//! `Command::pre_exec`. The Windows path uses
//! `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` and would need a
//! parallel test exercising the equivalent process-handle checks;
//! deferred to whenever Windows CI lands per ADR-0012 D2/D9.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn binary_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    p.push("mneme");
    p
}

#[test]
fn daemon_self_detaches_and_returns_shell() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    // Scaffold the data dir so the spawned daemon has somewhere to
    // boot.
    let init = Command::new(binary_path())
        .arg("init")
        .env("MNEME_DATA_DIR", &root)
        .output()
        .expect("init spawn");
    assert!(init.status.success(), "init must succeed: {init:?}");

    // Run `mneme daemon` (no --foreground). The contract: parent
    // returns within ~1 second with the child PID on stdout. The
    // child keeps running and binds the socket.
    let parent_started = Instant::now();
    let output = Command::new(binary_path())
        .arg("daemon")
        .env("MNEME_DATA_DIR", &root)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .output()
        .expect("daemon spawn");
    let parent_elapsed = parent_started.elapsed();

    assert!(
        output.status.success(),
        "parent must exit cleanly; got status {:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        parent_elapsed < Duration::from_secs(5),
        "parent should return promptly, took {parent_elapsed:?}"
    );

    let pid_line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let child_pid: i32 = pid_line
        .parse()
        .unwrap_or_else(|e| panic!("parent stdout should be a PID, got {pid_line:?}: {e}"));

    // The child binds the socket asynchronously. Poll until it
    // shows up; bound by a 5-second budget so a wedged daemon
    // doesn't hang the test indefinitely.
    let socket = root.join("run").join("mneme.sock");
    let bind_deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < bind_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        socket.exists(),
        "child must bind the socket within 5s; PID={child_pid} root={root:?}"
    );

    // Confirm the child is alive by sending signal 0 (kill(pid, 0)
    // is the standard Unix liveness probe — it doesn't deliver a
    // signal, just returns success/EPERM/ESRCH).
    // SAFETY: libc::kill is async-signal-safe and the args are
    // valid (pid is a positive PID, signal is 0).
    let alive = unsafe { libc::kill(child_pid, 0) };
    assert_eq!(
        alive,
        0,
        "child must be alive after parent exits (kill(pid, 0) returned non-zero); errno={}",
        std::io::Error::last_os_error()
    );

    // Clean up: send SIGTERM, wait for the child to drop the
    // socket. Test-cleanup hygiene — without this the daemon would
    // outlive the test and clutter the system.
    // SAFETY: same as above; SIGTERM is a valid signal.
    let term = unsafe { libc::kill(child_pid, libc::SIGTERM) };
    assert_eq!(term, 0, "SIGTERM should reach the daemon");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < exit_deadline {
        // SAFETY: kill(pid, 0) is the canonical liveness probe.
        if unsafe { libc::kill(child_pid, 0) } != 0 {
            return; // child has exited; success
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon did not exit within 10s of SIGTERM (PID={child_pid})");
}

#[test]
fn daemon_foreground_flag_stays_attached() {
    // With `--foreground`, the parent IS the daemon — there's no
    // self-detach. The contract: the process keeps running until
    // killed; stdout is empty (no PID line).
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    let init = Command::new(binary_path())
        .arg("init")
        .env("MNEME_DATA_DIR", &root)
        .output()
        .expect("init spawn");
    assert!(init.status.success(), "init must succeed");

    let mut child = Command::new(binary_path())
        .arg("daemon")
        .arg("--foreground")
        .env("MNEME_DATA_DIR", &root)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon --foreground spawn");

    // Give the daemon up to 5s to bind the socket.
    let socket = root.join("run").join("mneme.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("daemon --foreground exited prematurely with status {status:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(socket.exists(), "foreground daemon must bind the socket");

    // Cleanup: kill, wait. SIGTERM via the std API.
    child.kill().expect("kill");
    let _ = child.wait();
}
