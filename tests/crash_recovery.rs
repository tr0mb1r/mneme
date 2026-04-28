//! Phase 2 spike exit gate: WAL durability under `kill -9`.
//!
//! For each iteration, the test:
//!   1. Spawns the `wal-driver` binary against a fresh temp directory.
//!   2. Sends a random workload of PUT/DEL commands, recording in an
//!      in-process oracle the operations that returned `OK <lsn>`.
//!   3. At a randomly chosen point, hard-kills the child (SIGKILL).
//!   4. Restarts the driver against the same directory.
//!   5. Issues `DUMP` and asserts the dump matches the oracle.
//!
//! By default the test runs **64 iterations**, which is fast enough for
//! routine CI. Set `MNEME_CRASH_TEST_ITERATIONS=1000` for the formal
//! Phase 2 exit-gate run from `proj_docs/mneme-implementation-plan.md` §3.
//!
//! The driver binary is gated behind `--features test-driver`. The test
//! locates it via `CARGO_BIN_EXE_wal-driver` (cargo sets this for any
//! `[[bin]]` enabled in the active feature set when the binary is built
//! via `--features test-driver`). The test must be invoked as:
//!
//! ```bash
//! cargo test --test crash_recovery --features test-driver
//! ```

#![cfg(feature = "test-driver")]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const DRIVER_BIN: &str = env!("CARGO_BIN_EXE_wal-driver");

fn iterations() -> usize {
    std::env::var("MNEME_CRASH_TEST_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64)
}

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

struct Driver {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Driver {
    fn spawn(dir: &PathBuf) -> Self {
        let mut child = Command::new(DRIVER_BIN)
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wal-driver");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut d = Driver {
            child: Some(child),
            stdin: Some(stdin),
            stdout,
        };
        // Wait for READY <max_lsn>
        let line = d.read_line();
        assert!(line.starts_with("READY "), "expected READY, got {line:?}");
        d
    }

    fn read_line(&mut self) -> String {
        let mut s = String::new();
        // bounded wait — driver should respond fast
        let start = Instant::now();
        loop {
            s.clear();
            match self.stdout.read_line(&mut s) {
                Ok(0) => panic!("EOF from driver"),
                Ok(_) => return s,
                Err(e) => {
                    if start.elapsed() > Duration::from_secs(10) {
                        panic!("read_line timeout: {e}");
                    }
                }
            }
        }
    }

    fn read_exact(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.stdout.read_exact(&mut buf).expect("read_exact");
        buf
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> std::io::Result<u64> {
        let stdin = self.stdin.as_mut().unwrap();
        let header = format!("PUT {} {}\n", key.len(), value.len());
        stdin.write_all(header.as_bytes())?;
        stdin.write_all(key)?;
        stdin.write_all(value)?;
        stdin.flush()?;
        let line = self.read_line();
        let line = line.trim_end_matches(['\r', '\n']);
        let mut parts = line.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        if tag != "OK" {
            return Err(std::io::Error::other(format!("driver replied: {line}")));
        }
        let lsn: u64 = parts
            .next()
            .unwrap_or("0")
            .parse()
            .map_err(|e: std::num::ParseIntError| std::io::Error::other(e.to_string()))?;
        Ok(lsn)
    }

    fn delete(&mut self, key: &[u8]) -> std::io::Result<u64> {
        let stdin = self.stdin.as_mut().unwrap();
        let header = format!("DEL {}\n", key.len());
        stdin.write_all(header.as_bytes())?;
        stdin.write_all(key)?;
        stdin.flush()?;
        let line = self.read_line();
        let line = line.trim_end_matches(['\r', '\n']);
        let mut parts = line.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        if tag != "OK" {
            return Err(std::io::Error::other(format!("driver replied: {line}")));
        }
        let lsn: u64 = parts
            .next()
            .unwrap_or("0")
            .parse()
            .map_err(|e: std::num::ParseIntError| std::io::Error::other(e.to_string()))?;
        Ok(lsn)
    }

    fn dump(&mut self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.stdin.as_mut().unwrap().write_all(b"DUMP\n").unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
        let header = self.read_line();
        let header = header.trim_end_matches(['\r', '\n']);
        let n: usize = header
            .strip_prefix("DUMP_BEGIN ")
            .expect("DUMP_BEGIN")
            .parse()
            .expect("dump count");
        let mut out = BTreeMap::new();
        for _ in 0..n {
            let line = self.read_line();
            let line = line.trim_end_matches(['\r', '\n']);
            let mut parts = line.split_whitespace();
            assert_eq!(parts.next(), Some("ENT"));
            let k_len: usize = parts.next().unwrap().parse().unwrap();
            let v_len: usize = parts.next().unwrap().parse().unwrap();
            let k = self.read_exact(k_len);
            let v = self.read_exact(v_len);
            out.insert(k, v);
        }
        let end = self.read_line();
        assert_eq!(end.trim_end_matches(['\r', '\n']), "DUMP_END");
        out
    }

    fn kill_minus_9(&mut self) {
        // Drop stdin first so the child sees EOF or EPIPE on its next write.
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            // std::process::Child::kill sends SIGKILL on Unix, TerminateProcess on Windows.
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child.take();
    }

    fn graceful_exit(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(b"EXIT\n");
            let _ = stdin.flush();
        }
        let _ = self.read_line();
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        self.stdin.take();
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Generate a random workload using a passed RNG.
fn gen_workload(rng: &mut SimpleRng, n: usize, key_space: usize) -> Vec<Op> {
    (0..n)
        .map(|_| {
            let key_idx = (rng.next_u32() as usize) % key_space;
            let key = format!("k{key_idx:04}").into_bytes();
            // 70% PUT, 30% DEL
            if rng.next_u32() % 100 < 70 {
                let val_len = (rng.next_u32() as usize % 32) + 1;
                let mut val = vec![0u8; val_len];
                rng.fill_bytes(&mut val);
                Op::Put(key, val)
            } else {
                Op::Del(key)
            }
        })
        .collect()
}

#[test]
fn kill_minus_9_preserves_acked_writes() {
    let total_iters = iterations();
    let mut prng = SimpleRng::seeded(0xC0FFEE_DEADBEEF);

    for iter in 0..total_iters {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let workload_size = (prng.next_u32() as usize) % 100 + 5; // 5–104 ops
        let key_space = 16;
        let workload = gen_workload(&mut prng, workload_size, key_space);
        let kill_at = (prng.next_u32() as usize) % (workload.len() + 1);

        // ---- Phase 1: spawn driver, send ops up to kill_at, then SIGKILL.
        let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        {
            let mut drv = Driver::spawn(&dir);
            for (i, op) in workload.iter().enumerate() {
                if i == kill_at {
                    break;
                }
                match op {
                    Op::Put(k, v) => match drv.put(k, v) {
                        Ok(_lsn) => {
                            oracle.insert(k.clone(), v.clone());
                        }
                        Err(_) => break, // child died mid-flight; accept whatever's on disk
                    },
                    Op::Del(k) => match drv.delete(k) {
                        Ok(_lsn) => {
                            oracle.remove(k);
                        }
                        Err(_) => break,
                    },
                }
            }
            drv.kill_minus_9();
        }

        // ---- Phase 2: restart, dump, compare.
        let mut drv = Driver::spawn(&dir);
        let recovered = drv.dump();
        drv.graceful_exit();

        assert_eq!(
            recovered, oracle,
            "iteration {iter}: recovered state does not match oracle\n  workload_size={workload_size}\n  kill_at={kill_at}"
        );
    }
}

// ---------- minimal deterministic RNG (xorshift64) ----------

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn seeded(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0xDEADBEEF } else { seed },
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut i = 0;
        while i < dest.len() {
            let val = self.next_u64().to_le_bytes();
            let take = (dest.len() - i).min(8);
            dest[i..i + take].copy_from_slice(&val[..take]);
            i += take;
        }
    }
}
