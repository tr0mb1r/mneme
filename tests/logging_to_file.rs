//! Integration test pinning the file-logging contract.
//!
//! `mneme run` (and any other long-running mode) writes INFO+ logs
//! to `<root>/logs/mneme.log`, honoring `[logging]` config from
//! `<root>/config.toml`. Pre-v1.1.x, `init_logging` only wrote to
//! stderr — this test guards against regression.
//!
//! Test approach: spawn the release-built binary with
//! `MNEME_DATA_DIR` pointing at a tempdir, give it a moment to boot
//! and log a few INFO lines, kill, then assert the log file exists
//! and contains the boot lines.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

fn binary_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    // CARGO_TARGET_DIR override would land here; fall back to the
    // standard layout. The test runner already builds the binary
    // before invoking integration tests, so no `cargo build` here.
    p.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    p.push("mneme");
    p
}

#[test]
fn run_writes_info_logs_to_file_at_default_path() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    // Scaffold the data dir so `mneme run` has somewhere to boot.
    let init = Command::new(binary_path())
        .arg("init")
        .env("MNEME_DATA_DIR", &root)
        .output()
        .expect("mneme init should succeed");
    assert!(init.status.success(), "init failed: {init:?}");

    // Spawn `mneme run` with stub embedder + stdin closed → boots,
    // emits the "starting on stdio" / "shutting down" INFO pair,
    // exits cleanly. The non-blocking writer flushes when its
    // WorkerGuard is dropped at process exit.
    let mut child = Command::new(binary_path())
        .arg("run")
        .env("MNEME_DATA_DIR", &root)
        .env("MNEME_EMBEDDER", "stub")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mneme run");

    // Give the run loop time to log boot + see EOF + shut down. The
    // stub-embedder boot path takes a few hundred ms; the EOF
    // shutdown is immediate after.
    let _ = child.wait_timeout(Duration::from_secs(5));
    let _ = child.kill();
    let _ = child.wait();

    let log_path = root.join("logs").join("mneme.log");
    assert!(
        log_path.exists(),
        "expected log file at {log_path:?} (logs dir: {:?})",
        root.join("logs").read_dir().ok().map(|d| d.count())
    );
    let content = std::fs::read_to_string(&log_path).expect("read log file");
    assert!(
        content.contains("INFO"),
        "log file should contain INFO lines; got:\n{content}"
    );
    assert!(
        content.contains("mneme MCP server"),
        "log file should contain a server boot/stop line; got:\n{content}"
    );
}

// Tiny shim so we can express `wait_timeout` without pulling another
// dep — std doesn't ship a timeout-aware wait, but we can poll.
trait WaitTimeout {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let start = std::time::Instant::now();
        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None => {
                    if start.elapsed() >= timeout {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}
