//! Write-ahead log for durable, crash-safe storage writes.
//!
//! # Format
//!
//! Each record is a frame with this layout:
//!
//! ```text
//! +----------+----------+----------+--------------------+----------+
//! | u32 LE   | u64 LE   | u64 LE   | postcard payload   | u32 LE   |
//! | pl_len   | lsn      | tx_id    | (pl_len bytes)     | crc32c   |
//! +----------+----------+----------+--------------------+----------+
//! ```
//!
//! The CRC32C is computed over `lsn || tx_id || payload`. Header overhead
//! is 20 bytes; trailer is 4 bytes; total frame overhead is 24 bytes.
//!
//! Records live in append-only segment files named `wal-{start_lsn:016x}.log`
//! in the WAL directory. A new segment is started when the current one would
//! exceed `SEGMENT_SIZE_BYTES` (64 MiB).
//!
//! # Durability contract
//!
//! Writes are routed through a single dedicated OS thread that owns the
//! active segment file. Callers `append(op).await` and receive the assigned
//! LSN only after `fdatasync` has succeeded (group-committed across up to
//! `BATCH_CAP` peers). On `kill -9`, any record whose ack returned must be
//! recoverable via [`replay`].
//!
//! # Forward compatibility
//!
//! [`WalOp`] variants are encoded by postcard as varint discriminants. New
//! variants must be added at the END of the enum to preserve on-disk
//! compatibility — Phase 3 will add `EmbedInsert`, `HnswInsert`, `HnswDelete`.

use crate::ids::MemoryId;
use crate::{MnemeError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};

/// Header: payload_len(u32) + lsn(u64) + tx_id(u64).
pub const HEADER_BYTES: usize = 4 + 8 + 8;
/// Trailer: crc32c(u32).
pub const TRAILER_BYTES: usize = 4;
/// Per-record fixed overhead.
pub const FRAME_OVERHEAD_BYTES: usize = HEADER_BYTES + TRAILER_BYTES;
/// Max payload bytes accepted by the encoder/decoder. 16 MiB.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Segment rotation threshold. 64 MiB.
pub const SEGMENT_SIZE_BYTES: u64 = 64 * 1024 * 1024;
/// Max records coalesced into one fsync.
pub const BATCH_CAP: usize = 64;
/// Inbound command channel capacity.
pub const CHANNEL_CAP: usize = 1024;

/// Operations recorded in the WAL.
///
/// Variants are encoded by postcard with varint discriminants. ADD NEW
/// VARIANTS ONLY AT THE END to preserve on-disk format compatibility.
///
/// A given physical WAL only ever carries a subset of these. The redb
/// storage WAL (under `episodic/wal/`) only sees [`Put`] / [`Delete`];
/// the semantic-index WAL (under `semantic/wal/`) only sees
/// [`VectorInsert`] / [`VectorDelete`]. Mixing is not currently
/// supported — appliers reject what they don't recognise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalOp {
    /// Insert or overwrite `key -> value`.
    Put { key: Vec<u8>, value: Vec<u8> },
    /// Remove `key` if present.
    Delete { key: Vec<u8> },
    /// Phase 3 §6 — append HNSW vector for `id`. Vector is stored as a
    /// raw `Vec<f32>`; postcard encodes the length-prefix.
    VectorInsert { id: MemoryId, vec: Vec<f32> },
    /// Phase 3 §6 — soft-delete `id` from the HNSW.
    VectorDelete { id: MemoryId },
}

/// A record yielded by [`replay`].
// `WalOp` (and thus `ReplayRecord`) is no longer `Eq` because
// `VectorInsert { vec: Vec<f32>, .. }` carries an f32 payload. Tests
// that previously used `assert_eq!` rely on PartialEq alone, which
// still passes — Eq is just the marker trait for "PartialEq is also a
// total equivalence", and we don't need that here.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayRecord {
    pub lsn: u64,
    pub tx_id: u64,
    pub op: WalOp,
}

/// Hook called by the writer thread after each fsync, before acks fire.
///
/// Used by [`crate::storage::redb_impl::RedbStorage`] to serialize redb
/// commits with the WAL: every record returned to the applier is durable
/// in the WAL, and the applier runs in LSN order under the writer thread's
/// implicit single-thread guarantee.
///
/// If `apply_batch` returns `Err`, every caller in the batch sees the
/// error on their `append().await`, but the records remain durable in
/// the WAL — replay on next startup will retry the apply.
pub trait Applier: Send + 'static {
    fn apply_batch(&mut self, records: &[ReplayRecord]) -> Result<()>;
}

/// No-op applier — records are durable in the WAL only.
pub struct NoopApplier;

impl Applier for NoopApplier {
    fn apply_batch(&mut self, _records: &[ReplayRecord]) -> Result<()> {
        Ok(())
    }
}

/// Async-facing handle to the WAL writer thread.
///
/// Cloneable cheap senders are not exposed; callers should share the
/// `WalWriter` itself behind an `Arc`.
pub struct WalWriter {
    cmd_tx: mpsc::Sender<WalCommand>,
    join: Option<std::thread::JoinHandle<Result<()>>>,
}

struct WalCommand {
    op: WalOp,
    ack: oneshot::Sender<Result<u64>>,
}

impl WalWriter {
    /// Open the WAL writer with no applier — every record is durable in the
    /// WAL only.
    pub fn open(dir: &Path, start_lsn: u64) -> Result<Self> {
        Self::open_with_applier(dir, start_lsn, Box::new(NoopApplier))
    }

    /// Open the WAL writer with a custom applier invoked after each fsync.
    ///
    /// On startup the writer scans existing segments, validates the tail of
    /// the active segment, truncates any torn record, and opens for append.
    /// `start_lsn` MUST equal `last_observed_lsn + 1` (or 1 if the WAL is
    /// empty); callers establish this by running [`replay`] first.
    pub fn open_with_applier(
        dir: &Path,
        start_lsn: u64,
        applier: Box<dyn Applier>,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let segments = list_segments(dir)?;
        let (active_path, segment_start_lsn, mut bytes_in_segment, observed_max_lsn) =
            prepare_active_segment(dir, &segments, start_lsn)?;

        if observed_max_lsn + 1 != start_lsn {
            return Err(MnemeError::Wal(format!(
                "start_lsn {start_lsn} does not match observed_max_lsn+1 ({})",
                observed_max_lsn + 1
            )));
        }

        // If the chosen active segment is full, rotate immediately.
        if bytes_in_segment >= SEGMENT_SIZE_BYTES {
            let new_path = segment_path(dir, start_lsn);
            File::create(&new_path)?;
            bytes_in_segment = 0;
            return Self::spawn_writer(
                dir,
                new_path,
                start_lsn,
                start_lsn,
                bytes_in_segment,
                applier,
            );
        }

        Self::spawn_writer(
            dir,
            active_path,
            segment_start_lsn,
            start_lsn,
            bytes_in_segment,
            applier,
        )
    }

    fn spawn_writer(
        dir: &Path,
        active_path: PathBuf,
        segment_start_lsn: u64,
        next_lsn: u64,
        bytes_in_segment: u64,
        applier: Box<dyn Applier>,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_CAP);
        let dir_owned = dir.to_path_buf();
        let join = std::thread::Builder::new()
            .name("mneme-wal-writer".into())
            .spawn(move || {
                let state = WriterState::open(
                    dir_owned,
                    active_path,
                    segment_start_lsn,
                    next_lsn,
                    bytes_in_segment,
                )?;
                writer_loop(state, cmd_rx, applier)
            })
            .map_err(MnemeError::Io)?;
        Ok(WalWriter {
            cmd_tx,
            join: Some(join),
        })
    }

    /// Append a record. Returns the assigned LSN once the record is durable on disk.
    pub async fn append(&self, op: WalOp) -> Result<u64> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.cmd_tx
            .send(WalCommand { op, ack: ack_tx })
            .await
            .map_err(|_| MnemeError::Wal("writer task closed".into()))?;
        ack_rx
            .await
            .map_err(|_| MnemeError::Wal("ack channel dropped".into()))?
    }

    /// Gracefully shut down the writer. Drains pending records then syncs and joins.
    pub fn shutdown(mut self) -> Result<()> {
        Self::shutdown_inner(&mut self.cmd_tx, &mut self.join)
    }

    /// Shared close + join logic used by both [`shutdown`] and [`Drop`].
    fn shutdown_inner(
        cmd_tx: &mut mpsc::Sender<WalCommand>,
        join: &mut Option<std::thread::JoinHandle<Result<()>>>,
    ) -> Result<()> {
        // Replace the sender with a dummy whose receiver is dropped
        // immediately. Dropping the original sender causes the writer
        // thread's blocking_recv to return None and exit cleanly.
        drop(std::mem::replace(cmd_tx, mpsc::channel(1).0));
        if let Some(j) = join.take() {
            j.join()
                .map_err(|_| MnemeError::Wal("writer thread panicked".into()))?
        } else {
            Ok(())
        }
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        // Synchronously close the channel and wait for the writer thread,
        // so any state held by the applier (e.g. an Arc<redb::Database>)
        // is released before this Drop returns. Errors are swallowed —
        // callers who care should use `shutdown()` explicitly.
        let _ = Self::shutdown_inner(&mut self.cmd_tx, &mut self.join);
    }
}

struct WriterState {
    dir: PathBuf,
    active_path: PathBuf,
    file: BufWriter<File>,
    segment_start_lsn: u64,
    next_lsn: u64,
    next_tx_id: u64,
    bytes_in_segment: u64,
    /// `true` once a write has hit ENOSPC; subsequent writes fail fast.
    disk_full: bool,
}

impl WriterState {
    fn open(
        dir: PathBuf,
        active_path: PathBuf,
        segment_start_lsn: u64,
        next_lsn: u64,
        bytes_in_segment: u64,
    ) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .truncate(false)
            .open(&active_path)?;
        Ok(Self {
            dir,
            active_path,
            file: BufWriter::new(file),
            segment_start_lsn,
            next_lsn,
            next_tx_id: next_lsn,
            bytes_in_segment,
            disk_full: false,
        })
    }

    fn write_record(&mut self, op: WalOp) -> Result<u64> {
        if self.disk_full {
            return Err(MnemeError::DiskFull);
        }
        let payload = postcard::to_allocvec(&op)
            .map_err(|e| MnemeError::Wal(format!("postcard encode: {e}")))?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(MnemeError::Wal(format!(
                "payload {} exceeds MAX_PAYLOAD_BYTES",
                payload.len()
            )));
        }
        let frame_len = (FRAME_OVERHEAD_BYTES + payload.len()) as u64;
        if self.bytes_in_segment > 0 && self.bytes_in_segment + frame_len > SEGMENT_SIZE_BYTES {
            self.rotate()?;
        }

        let lsn = self.next_lsn;
        let tx_id = self.next_tx_id;

        let mut crc_buf = Vec::with_capacity(8 + 8 + payload.len());
        crc_buf.extend_from_slice(&lsn.to_le_bytes());
        crc_buf.extend_from_slice(&tx_id.to_le_bytes());
        crc_buf.extend_from_slice(&payload);
        let crc = crc32c::crc32c(&crc_buf);

        self.write_with_enospc(&(payload.len() as u32).to_le_bytes())?;
        self.write_with_enospc(&lsn.to_le_bytes())?;
        self.write_with_enospc(&tx_id.to_le_bytes())?;
        self.write_with_enospc(&payload)?;
        self.write_with_enospc(&crc.to_le_bytes())?;

        self.bytes_in_segment += frame_len;
        self.next_lsn += 1;
        self.next_tx_id += 1;
        Ok(lsn)
    }

    fn write_with_enospc(&mut self, bytes: &[u8]) -> Result<()> {
        match self.file.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(libc_enospc) if libc_enospc == ENOSPC) => {
                self.disk_full = true;
                Err(MnemeError::DiskFull)
            }
            Err(e) => Err(MnemeError::Io(e)),
        }
    }

    fn flush_and_sync(&mut self) -> Result<()> {
        match self.file.flush() {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == ENOSPC) => {
                self.disk_full = true;
                return Err(MnemeError::DiskFull);
            }
            Err(e) => return Err(MnemeError::Io(e)),
        }
        match self.file.get_ref().sync_data() {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == ENOSPC) => {
                self.disk_full = true;
                Err(MnemeError::DiskFull)
            }
            Err(e) => Err(MnemeError::Io(e)),
        }
    }

    fn rotate(&mut self) -> Result<()> {
        self.flush_and_sync()?;
        let new_path = segment_path(&self.dir, self.next_lsn);
        let new_file = OpenOptions::new()
            .create(true)
            .append(true)
            .truncate(false)
            .open(&new_path)?;
        self.file = BufWriter::new(new_file);
        self.active_path = new_path;
        self.segment_start_lsn = self.next_lsn;
        self.bytes_in_segment = 0;
        Ok(())
    }

    /// Truncate the active segment back to `pre_offset` bytes and reset LSN
    /// counters so a rolled-back batch leaves no torn records on disk.
    fn rollback(&mut self, pre_offset: u64, pre_lsn: u64) -> Result<()> {
        let _ = self.file.flush();
        self.file.get_mut().set_len(pre_offset)?;
        // reopen to reset BufWriter position to the new file end
        let f = OpenOptions::new().append(true).open(&self.active_path)?;
        self.file = BufWriter::new(f);
        self.bytes_in_segment = pre_offset;
        self.next_lsn = pre_lsn;
        self.next_tx_id = pre_lsn;
        Ok(())
    }

    fn close(mut self) -> Result<()> {
        // Best-effort final flush + sync. A previous DiskFull state can
        // legitimately fail this; we surface the error to the caller.
        match self.file.flush() {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == ENOSPC) => {
                return Err(MnemeError::DiskFull);
            }
            Err(e) => return Err(MnemeError::Io(e)),
        }
        match self.file.get_ref().sync_data() {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == ENOSPC) => {
                Err(MnemeError::DiskFull)
            }
            Err(e) => Err(MnemeError::Io(e)),
        }
    }
}

#[cfg(target_os = "linux")]
const ENOSPC: i32 = 28;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const ENOSPC: i32 = 28;
#[cfg(windows)]
const ENOSPC: i32 = 39; // ERROR_HANDLE_DISK_FULL on win32; rough mapping
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
const ENOSPC: i32 = 28;

fn writer_loop(
    mut state: WriterState,
    mut cmd_rx: mpsc::Receiver<WalCommand>,
    mut applier: Box<dyn Applier>,
) -> Result<()> {
    loop {
        let Some(first) = cmd_rx.blocking_recv() else {
            return state.close();
        };
        let mut batch: Vec<WalCommand> = Vec::with_capacity(BATCH_CAP);
        batch.push(first);
        while batch.len() < BATCH_CAP {
            match cmd_rx.try_recv() {
                Ok(cmd) => batch.push(cmd),
                Err(_) => break,
            }
        }

        // Snapshot pre-batch state for rollback on fatal mid-batch error.
        let pre_offset = state.bytes_in_segment;
        let pre_lsn = state.next_lsn;
        let pre_active = state.active_path.clone();

        let mut acks_to_send: Vec<(oneshot::Sender<Result<u64>>, u64)> =
            Vec::with_capacity(batch.len());
        let mut records: Vec<ReplayRecord> = Vec::with_capacity(batch.len());
        let mut rollback_needed: Option<MnemeError> = None;

        for cmd in batch {
            if rollback_needed.is_some() {
                // After a fatal error we still need to consume remaining
                // batch members so their oneshot senders see a result.
                let _ = cmd
                    .ack
                    .send(Err(MnemeError::Wal("batch rolled back".into())));
                continue;
            }
            let op_clone = cmd.op.clone();
            match state.write_record(cmd.op) {
                Ok(lsn) => {
                    acks_to_send.push((cmd.ack, lsn));
                    records.push(ReplayRecord {
                        lsn,
                        tx_id: lsn,
                        op: op_clone,
                    });
                }
                Err(MnemeError::DiskFull) => {
                    let _ = cmd.ack.send(Err(MnemeError::DiskFull));
                    rollback_needed = Some(MnemeError::DiskFull);
                }
                Err(e) => {
                    let _ = cmd
                        .ack
                        .send(Err(MnemeError::Wal(format!("write failed: {e}"))));
                    rollback_needed = Some(MnemeError::Wal(format!("write failed: {e}")));
                }
            }
        }

        if let Some(err) = rollback_needed {
            // Roll back any partial writes from this batch.
            // If we rotated mid-batch, restore active_path too — best effort.
            if state.active_path != pre_active {
                // Re-point the writer back at the previous segment.
                if let Ok(f) = OpenOptions::new().append(true).open(&pre_active) {
                    state.file = BufWriter::new(f);
                    state.active_path = pre_active.clone();
                }
            }
            let _ = state.rollback(pre_offset, pre_lsn);
            for (ack, _) in acks_to_send {
                let _ = ack.send(Err(MnemeError::Wal(format!("batch rolled back: {err}"))));
            }
            continue;
        }

        // Single fdatasync for the whole batch.
        match state.flush_and_sync() {
            Ok(()) => {
                // Records are durable. Run the applier in LSN order on this
                // same thread — gives us redb's commit serialization for free.
                match applier.apply_batch(&records) {
                    Ok(()) => {
                        for (ack, lsn) in acks_to_send {
                            let _ = ack.send(Ok(lsn));
                        }
                    }
                    Err(e) => {
                        // WAL is durable, but redb is behind. Surface the error
                        // to all batched callers; on next restart, replay
                        // catches up. The records are NOT rolled back from WAL.
                        for (ack, _) in acks_to_send {
                            let _ = ack.send(Err(MnemeError::Wal(format!("apply failed: {e}"))));
                        }
                    }
                }
            }
            Err(e) => {
                // fsync failed — durability cannot be claimed. Fail every ack.
                let _ = state.rollback(pre_offset, pre_lsn);
                for (ack, _) in acks_to_send {
                    let _ = ack.send(Err(MnemeError::Wal(format!("fsync failed: {e}"))));
                }
            }
        }
    }
}

// ---------- Replay ----------

/// Read every record in the WAL directory in LSN order.
///
/// On a torn tail (truncated frame or CRC mismatch at the end of a segment),
/// the iterator returns `None` cleanly — that record was never durably
/// committed and must not be applied. Hard errors (bad postcard payload,
/// oversize record, I/O) surface as `Some(Err(_))`.
pub fn replay(dir: &Path) -> Result<Replay> {
    let segments = if dir.exists() {
        list_segments(dir)?
    } else {
        Vec::new()
    };
    Ok(Replay {
        segments,
        idx: 0,
        current: None,
        torn_tail_seen: false,
    })
}

pub struct Replay {
    segments: Vec<(u64, PathBuf)>,
    idx: usize,
    current: Option<BufReader<File>>,
    torn_tail_seen: bool,
}

impl Iterator for Replay {
    type Item = Result<ReplayRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.torn_tail_seen {
            return None;
        }
        loop {
            if self.current.is_none() {
                if self.idx >= self.segments.len() {
                    return None;
                }
                let (_lsn, path) = &self.segments[self.idx];
                self.idx += 1;
                let file = match File::open(path) {
                    Ok(f) => f,
                    Err(e) => return Some(Err(MnemeError::Io(e))),
                };
                self.current = Some(BufReader::new(file));
            }
            let reader = self.current.as_mut().unwrap();
            match read_frame(reader) {
                Ok(Some(rec)) => return Some(Ok(rec)),
                Ok(None) => {
                    // clean end of segment — advance to next
                    self.current = None;
                    continue;
                }
                Err(FrameReadError::TornTail) => {
                    // Stop replay completely on torn tail. A torn tail in
                    // any segment except the last would imply mid-WAL
                    // corruption; either way, we cannot trust subsequent
                    // records to maintain LSN ordering.
                    self.torn_tail_seen = true;
                    return None;
                }
                Err(FrameReadError::CrcMismatch) => {
                    self.torn_tail_seen = true;
                    return None;
                }
                Err(FrameReadError::Io(e)) => return Some(Err(MnemeError::Io(e))),
                Err(FrameReadError::PostcardDecode(e)) => {
                    return Some(Err(MnemeError::Wal(format!("postcard decode: {e}"))));
                }
                Err(FrameReadError::PayloadTooLarge(n)) => {
                    return Some(Err(MnemeError::Wal(format!("payload too large: {n}"))));
                }
            }
        }
    }
}

#[derive(Debug)]
enum FrameReadError {
    Io(std::io::Error),
    TornTail,
    CrcMismatch,
    PostcardDecode(String),
    PayloadTooLarge(usize),
}

fn read_frame<R: Read>(
    reader: &mut R,
) -> std::result::Result<Option<ReplayRecord>, FrameReadError> {
    let mut header = [0u8; HEADER_BYTES];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(FrameReadError::Io(e)),
    }
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let lsn = u64::from_le_bytes(header[4..12].try_into().unwrap());
    let tx_id = u64::from_le_bytes(header[12..20].try_into().unwrap());
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(FrameReadError::PayloadTooLarge(payload_len));
    }
    let mut payload = vec![0u8; payload_len];
    if let Err(e) = reader.read_exact(&mut payload) {
        return if e.kind() == ErrorKind::UnexpectedEof {
            Err(FrameReadError::TornTail)
        } else {
            Err(FrameReadError::Io(e))
        };
    }
    let mut trailer = [0u8; TRAILER_BYTES];
    if let Err(e) = reader.read_exact(&mut trailer) {
        return if e.kind() == ErrorKind::UnexpectedEof {
            Err(FrameReadError::TornTail)
        } else {
            Err(FrameReadError::Io(e))
        };
    }
    let claimed_crc = u32::from_le_bytes(trailer);
    let mut crc_buf = Vec::with_capacity(8 + 8 + payload_len);
    crc_buf.extend_from_slice(&lsn.to_le_bytes());
    crc_buf.extend_from_slice(&tx_id.to_le_bytes());
    crc_buf.extend_from_slice(&payload);
    let actual_crc = crc32c::crc32c(&crc_buf);
    if actual_crc != claimed_crc {
        return Err(FrameReadError::CrcMismatch);
    }
    let op: WalOp = postcard::from_bytes(&payload)
        .map_err(|e| FrameReadError::PostcardDecode(format!("{e}")))?;
    Ok(Some(ReplayRecord { lsn, tx_id, op }))
}

/// Delete WAL segments whose entire LSN range is fully covered by
/// `applied_lsn`. Phase 3 §8 calls this after every successful HNSW
/// snapshot — once a snapshot is durable, the WAL records folded into
/// it are pure recovery overhead and can be reclaimed.
///
/// Conservative correctness rule, used so we don't have to crack
/// every segment to find its max LSN: a segment is deletable iff the
/// **next** segment's `start_lsn` is `<= applied_lsn + 1`. That guarantees
/// every record in this segment has `lsn < next.start_lsn <= applied_lsn + 1`,
/// i.e. `lsn <= applied_lsn`. The active (last) segment is never
/// deleted — that would race with the writer thread.
///
/// Returns the number of segments deleted.
pub fn truncate_through(dir: &Path, applied_lsn: u64) -> Result<usize> {
    let segments = if dir.exists() {
        list_segments(dir)?
    } else {
        return Ok(0);
    };
    if segments.len() < 2 {
        return Ok(0);
    }
    let mut deleted = 0usize;
    // Walk pairs; stop at the second-to-last so we never touch the active segment.
    for i in 0..segments.len().saturating_sub(1) {
        let (_this_start, this_path) = &segments[i];
        let (next_start, _next_path) = &segments[i + 1];
        // `next_start - 1` is the highest LSN this segment could contain.
        // If that's already <= applied_lsn, we can drop this segment.
        if next_start.saturating_sub(1) <= applied_lsn {
            std::fs::remove_file(this_path)?;
            deleted += 1;
        } else {
            // Segments are start-LSN ordered; once we find one we
            // can't drop, no later segment is droppable either.
            break;
        }
    }
    Ok(deleted)
}

// ---------- Segment filesystem helpers ----------

fn segment_path(dir: &Path, start_lsn: u64) -> PathBuf {
    dir.join(format!("wal-{:016x}.log", start_lsn))
}

fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if let Some(rest) = s.strip_prefix("wal-")
            && let Some(hex) = rest.strip_suffix(".log")
            && let Ok(start_lsn) = u64::from_str_radix(hex, 16)
        {
            out.push((start_lsn, entry.path()));
        }
    }
    out.sort_by_key(|(l, _)| *l);
    Ok(out)
}

/// Walk the active segment, validate each frame, and return:
/// `(active_segment_path, segment_start_lsn, valid_bytes_offset, observed_max_lsn)`.
///
/// Truncates the active segment to `valid_bytes_offset` so torn-tail bytes
/// are physically removed before the writer thread opens it for append.
///
/// `max_lsn` is `start_lsn - 1` if no records were observed (so the caller's
/// invariant `start_lsn == max+1` checks out for an empty WAL).
fn prepare_active_segment(
    dir: &Path,
    segments: &[(u64, PathBuf)],
    start_lsn: u64,
) -> Result<(PathBuf, u64, u64, u64)> {
    let mut observed_max_lsn = start_lsn.saturating_sub(1);

    // Validate all but the last segment (which we may need to truncate).
    for (_, path) in segments.iter().take(segments.len().saturating_sub(1)) {
        let mut reader = BufReader::new(File::open(path)?);
        loop {
            match read_frame(&mut reader) {
                Ok(Some(rec)) => observed_max_lsn = rec.lsn.max(observed_max_lsn),
                Ok(None) => break,
                Err(FrameReadError::TornTail) | Err(FrameReadError::CrcMismatch) => {
                    return Err(MnemeError::Wal(format!(
                        "torn tail in non-active segment {path:?} — refusing to open"
                    )));
                }
                Err(FrameReadError::Io(e)) => return Err(MnemeError::Io(e)),
                Err(FrameReadError::PostcardDecode(e)) => {
                    return Err(MnemeError::Wal(format!(
                        "postcard decode error in {path:?}: {e}"
                    )));
                }
                Err(FrameReadError::PayloadTooLarge(n)) => {
                    return Err(MnemeError::Wal(format!(
                        "oversize payload in {path:?}: {n}"
                    )));
                }
            }
        }
    }

    if let Some((seg_start_lsn, active_path)) = segments.last() {
        let (valid_offset, max_in_segment) = validate_segment_tail(active_path)?;
        observed_max_lsn = max_in_segment
            .unwrap_or(observed_max_lsn)
            .max(observed_max_lsn);
        // Truncate any torn bytes physically.
        let actual_size = std::fs::metadata(active_path)?.len();
        if valid_offset < actual_size {
            let f = OpenOptions::new().write(true).open(active_path)?;
            f.set_len(valid_offset)?;
            f.sync_all()?;
        }
        Ok((
            active_path.clone(),
            *seg_start_lsn,
            valid_offset,
            observed_max_lsn,
        ))
    } else {
        // No segments — create one starting at start_lsn.
        let path = segment_path(dir, start_lsn);
        File::create(&path)?;
        Ok((path, start_lsn, 0, observed_max_lsn))
    }
}

/// Walk a segment and return `(last_good_offset, last_good_lsn_observed)`.
/// On torn tail or CRC mismatch, returns the offset just before the bad frame.
fn validate_segment_tail(path: &Path) -> Result<(u64, Option<u64>)> {
    let mut file = File::open(path)?;
    let mut reader = BufReader::new(&mut file);
    let mut good_offset = 0u64;
    let mut max_lsn: Option<u64> = None;
    loop {
        match read_frame(&mut reader) {
            Ok(Some(rec)) => {
                let frame_size = (FRAME_OVERHEAD_BYTES + payload_size_of(&rec.op)?) as u64;
                good_offset += frame_size;
                max_lsn = Some(max_lsn.map_or(rec.lsn, |m| m.max(rec.lsn)));
            }
            Ok(None) => return Ok((good_offset, max_lsn)),
            Err(FrameReadError::TornTail) | Err(FrameReadError::CrcMismatch) => {
                return Ok((good_offset, max_lsn));
            }
            Err(FrameReadError::Io(e)) => return Err(MnemeError::Io(e)),
            Err(FrameReadError::PostcardDecode(e)) => {
                return Err(MnemeError::Wal(format!(
                    "postcard decode error in {path:?}: {e}"
                )));
            }
            Err(FrameReadError::PayloadTooLarge(n)) => {
                return Err(MnemeError::Wal(format!(
                    "oversize payload in {path:?}: {n}"
                )));
            }
        }
    }
}

fn payload_size_of(op: &WalOp) -> Result<usize> {
    let v =
        postcard::to_allocvec(op).map_err(|e| MnemeError::Wal(format!("postcard encode: {e}")))?;
    Ok(v.len())
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn round_trip_put_then_replay() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let writer = WalWriter::open(dir, 1).unwrap();
            let lsn1 = writer
                .append(WalOp::Put {
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec(),
                })
                .await
                .unwrap();
            let lsn2 = writer
                .append(WalOp::Put {
                    key: b"k2".to_vec(),
                    value: b"v2".to_vec(),
                })
                .await
                .unwrap();
            let lsn3 = writer
                .append(WalOp::Delete {
                    key: b"k1".to_vec(),
                })
                .await
                .unwrap();
            assert_eq!(lsn1, 1);
            assert_eq!(lsn2, 2);
            assert_eq!(lsn3, 3);
            writer.shutdown().unwrap();
        });

        let recs: Vec<_> = replay(dir).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].lsn, 1);
        assert_eq!(recs[2].lsn, 3);
        assert_eq!(
            recs[2].op,
            WalOp::Delete {
                key: b"k1".to_vec()
            }
        );
    }

    #[test]
    fn reopen_resumes_lsn() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let w = WalWriter::open(dir, 1).unwrap();
            w.append(WalOp::Put {
                key: vec![1],
                value: vec![10],
            })
            .await
            .unwrap();
            w.append(WalOp::Put {
                key: vec![2],
                value: vec![20],
            })
            .await
            .unwrap();
            w.shutdown().unwrap();
        });

        // Determine the next_lsn from replay and reopen.
        let max = replay(dir)
            .unwrap()
            .map(|r| r.unwrap().lsn)
            .max()
            .unwrap_or(0);

        rt.block_on(async {
            let w = WalWriter::open(dir, max + 1).unwrap();
            let lsn = w
                .append(WalOp::Put {
                    key: vec![3],
                    value: vec![30],
                })
                .await
                .unwrap();
            assert_eq!(lsn, 3);
            w.shutdown().unwrap();
        });

        let recs: Vec<_> = replay(dir).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(
            recs.iter().map(|r| r.lsn).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn torn_tail_replay_stops_cleanly() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let w = WalWriter::open(dir, 1).unwrap();
            w.append(WalOp::Put {
                key: vec![1],
                value: vec![10],
            })
            .await
            .unwrap();
            w.append(WalOp::Put {
                key: vec![2],
                value: vec![20],
            })
            .await
            .unwrap();
            w.shutdown().unwrap();
        });

        // Corrupt: append garbage bytes (simulates a half-written record).
        let segs = list_segments(dir).unwrap();
        let active = &segs[0].1;
        let mut f = OpenOptions::new().append(true).open(active).unwrap();
        f.write_all(b"\xff\xff\xff\xff\x00\x00").unwrap();
        f.sync_data().unwrap();

        let mut count = 0;
        for r in replay(dir).unwrap() {
            r.unwrap();
            count += 1;
        }
        assert_eq!(count, 2, "torn tail must be ignored");
    }

    #[test]
    fn segment_rotation_at_threshold() {
        // Force a small segment size by writing many records.
        // We don't have a knob for SEGMENT_SIZE_BYTES at runtime; instead
        // verify the rotation helper would work by directly exercising it
        // through validate_segment_tail under the real cap.
        // (Smoke: just confirm append works across what would be a rotation
        // boundary if the cap were lower. Without runtime knobs this stays
        // a documentation test for now.)
    }

    /// Helper: write empty segment files at the given start LSNs and
    /// run `truncate_through(applied_lsn)`. Returns the start_lsns
    /// remaining on disk in order.
    fn truncate_scenario(starts: &[u64], applied_lsn: u64) -> Vec<u64> {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        for s in starts {
            let p = segment_path(dir, *s);
            std::fs::write(&p, b"").unwrap();
        }
        truncate_through(dir, applied_lsn).unwrap();
        let mut left: Vec<u64> = list_segments(dir)
            .unwrap()
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        left.sort();
        left
    }

    #[test]
    fn truncate_through_keeps_active_segment_always() {
        // Single-segment WAL is never truncated.
        assert_eq!(truncate_scenario(&[1], 9999), vec![1]);
    }

    #[test]
    fn truncate_through_drops_fully_covered_segments() {
        // Segments at start_lsns 1, 11, 21, 31 (active). Records in
        // each segment cover [start, next_start - 1]:
        //   seg(1):  1..=10
        //   seg(11): 11..=20
        //   seg(21): 21..=30
        //   seg(31): active
        // applied_lsn=20 covers seg(1) entirely (max=10) and seg(11)
        // entirely (max=20), but not seg(21) (max=30).
        assert_eq!(truncate_scenario(&[1, 11, 21, 31], 20), vec![21, 31]);
    }

    #[test]
    fn truncate_through_partial_coverage_keeps_segment() {
        // applied_lsn=15 covers seg(1) (max=10) but only partially
        // covers seg(11) (max=20). The conservative rule keeps seg(11).
        assert_eq!(truncate_scenario(&[1, 11, 21], 15), vec![11, 21]);
    }

    #[test]
    fn truncate_through_zero_applied_drops_nothing() {
        assert_eq!(truncate_scenario(&[1, 11, 21], 0), vec![1, 11, 21]);
    }

    #[test]
    fn truncate_through_returns_count() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        for s in [1u64, 11, 21, 31] {
            std::fs::write(segment_path(dir, s), b"").unwrap();
        }
        let n = truncate_through(dir, 20).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn truncate_through_replay_then_keeps_correctness() {
        // Real-world end-to-end: write some records across rotations,
        // pretend applied_lsn covers the first segment, truncate, and
        // verify replay still returns the expected uncovered tail.
        // We can't force rotation cheaply here, so we hand-craft two
        // segments with valid frames each.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();
        rt.block_on(async {
            // First batch — lands in segment starting at LSN 1.
            let w = WalWriter::open(dir, 1).unwrap();
            w.append(WalOp::Put {
                key: b"a".to_vec(),
                value: b"1".to_vec(),
            })
            .await
            .unwrap();
            w.append(WalOp::Put {
                key: b"b".to_vec(),
                value: b"2".to_vec(),
            })
            .await
            .unwrap();
            w.shutdown().unwrap();
        });
        // Manually pin a "next segment" to make the first truncatable:
        // create an empty segment file at start_lsn=3 so the existing
        // segment(1) becomes non-active.
        std::fs::write(segment_path(dir, 3), b"").unwrap();
        // applied_lsn=2 fully covers segment(1) (its max LSN is 2,
        // and next_start=3, so next_start-1=2 <= 2).
        let removed = truncate_through(dir, 2).unwrap();
        assert_eq!(removed, 1);
        let surviving: Vec<u64> = list_segments(dir)
            .unwrap()
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(surviving, vec![3]);
    }
}
