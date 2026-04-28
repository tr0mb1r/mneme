//! Test-only WAL driver for crash-recovery proptests.
//!
//! Protocol (line + raw-bytes hybrid):
//!
//! ```text
//! Inbound:
//!   PUT <k_len> <v_len>\n[k_len bytes][v_len bytes]
//!   DEL <k_len>\n[k_len bytes]
//!   DUMP\n
//!   PING\n
//!   EXIT\n
//!
//! Outbound:
//!   READY <max_lsn_observed>\n          (once at startup, after WAL replay)
//!   OK <lsn>\n                          (per PUT/DEL)
//!   ERR <message>\n
//!   DUMP_BEGIN <n_entries>\n
//!   ENT <k_len> <v_len>\n[k_len bytes][v_len bytes]   (× n_entries)
//!   DUMP_END\n
//!   PONG\n
//! ```
//!
//! On `kill -9`, the driver terminates abruptly. Restarting against the same
//! directory replays the WAL and re-emits the oracle state on demand.

use mneme::storage::wal::{self, WalOp, WalWriter};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wal-driver: {e}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err("usage: wal-driver <wal-dir>".into());
    }
    let dir = PathBuf::from(&args[1]);
    std::fs::create_dir_all(&dir)?;

    // Replay to rebuild the oracle and discover the next LSN.
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut max_lsn: u64 = 0;
    for r in wal::replay(&dir)? {
        let rec = r?;
        max_lsn = max_lsn.max(rec.lsn);
        match rec.op {
            WalOp::Put { key, value } => {
                oracle.insert(key, value);
            }
            WalOp::Delete { key } => {
                oracle.remove(&key);
            }
            // The driver only exercises the redb-WAL surface; ignore
            // vector ops if they ever land here so the harness stays
            // forward-compatible with mixed WALs.
            WalOp::VectorInsert { .. } | WalOp::VectorDelete { .. } => {}
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let writer = WalWriter::open(&dir, max_lsn + 1)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stdin = io::stdin();
    let mut input = stdin.lock();

    writeln!(out, "READY {max_lsn}")?;
    out.flush()?;

    while let Some(line) = read_line(&mut input)? {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut parts = line.splitn(3, ' ');
        let cmd = parts.next().unwrap_or("");

        match cmd {
            "PUT" => {
                let k_len: usize = parts.next().unwrap_or("0").parse()?;
                let v_len: usize = parts.next().unwrap_or("0").parse()?;
                let mut key = vec![0u8; k_len];
                input.read_exact(&mut key)?;
                let mut value = vec![0u8; v_len];
                input.read_exact(&mut value)?;
                let lsn = rt.block_on(writer.append(WalOp::Put {
                    key: key.clone(),
                    value: value.clone(),
                }))?;
                oracle.insert(key, value);
                writeln!(out, "OK {lsn}")?;
                out.flush()?;
            }
            "DEL" => {
                let k_len: usize = parts.next().unwrap_or("0").parse()?;
                let mut key = vec![0u8; k_len];
                input.read_exact(&mut key)?;
                let lsn = rt.block_on(writer.append(WalOp::Delete { key: key.clone() }))?;
                oracle.remove(&key);
                writeln!(out, "OK {lsn}")?;
                out.flush()?;
            }
            "DUMP" => {
                writeln!(out, "DUMP_BEGIN {}", oracle.len())?;
                for (k, v) in &oracle {
                    writeln!(out, "ENT {} {}", k.len(), v.len())?;
                    out.write_all(k)?;
                    out.write_all(v)?;
                }
                writeln!(out, "DUMP_END")?;
                out.flush()?;
            }
            "PING" => {
                writeln!(out, "PONG")?;
                out.flush()?;
            }
            "EXIT" => {
                writer.shutdown()?;
                writeln!(out, "BYE")?;
                out.flush()?;
                return Ok(());
            }
            _ if cmd.is_empty() => continue,
            _ => {
                writeln!(out, "ERR unknown command: {cmd}")?;
                out.flush()?;
            }
        }
    }
    Ok(())
}

/// Read one `\n`-terminated line from `r`, returning `Ok(None)` at EOF.
/// Stops at the first `\n` and does NOT over-read into the next frame.
fn read_line<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte)? {
            0 => {
                if buf.is_empty() {
                    return Ok(None);
                } else {
                    return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
            1 => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
            _ => unreachable!(),
        }
    }
}
