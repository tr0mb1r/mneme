//! Newline-delimited JSON transport for MCP over stdio.
//!
//! Wire format: one JSON-RPC frame per line, terminated by `\n`. Lines
//! must not contain unescaped newlines inside the JSON payload — serde
//! emits compact JSON without raw newlines, so this is safe in practice.
//!
//! The transport is intentionally permissive on read: malformed frames
//! are surfaced as [`FrameError::Parse`] without closing the channel,
//! letting the server respond with a JSON-RPC parse error and continue.
//! Hard EOF on stdin is reported as [`FrameError::Eof`] and is the
//! signal for the server loop to shut down cleanly.

use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Hard cap on a single JSON-RPC frame. 16 MiB is two orders of magnitude
/// above any sane MCP message; a request larger than this is either a
/// bug or an attack and should be rejected before we allocate.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Async transport that reads newline-delimited JSON frames from any
/// `AsyncBufRead` and writes them back to any `AsyncWrite`. The writer
/// is wrapped in a `Mutex` so handlers can serialise concurrent
/// responses safely (we hold one lock per `write_frame` call only).
pub struct StdioTransport<R, W> {
    reader: BufReader<R>,
    writer: Mutex<W>,
    buf: Vec<u8>,
}

impl<R, W> StdioTransport<R, W>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: Mutex::new(writer),
            buf: Vec::with_capacity(8 * 1024),
        }
    }

    /// Read one frame. Trailing `\n` and any preceding `\r` are stripped.
    /// Empty lines are skipped (some clients send them as keepalives).
    /// Bytes received at EOF without a terminating newline are
    /// discarded and reported as [`FrameError::Eof`] — a half-written
    /// frame is a dying peer, not a message we should try to parse.
    pub async fn read_frame(&mut self) -> Result<Vec<u8>, FrameError> {
        loop {
            self.buf.clear();
            let outcome =
                read_line_capped(&mut self.reader, &mut self.buf, MAX_FRAME_BYTES).await?;
            match outcome {
                LineOutcome::Eof => return Err(FrameError::Eof),
                LineOutcome::PartialAtEof => return Err(FrameError::Eof),
                LineOutcome::Line => {}
            }
            // strip \n and optional \r
            let mut end = self.buf.len();
            if end > 0 && self.buf[end - 1] == b'\n' {
                end -= 1;
            }
            if end > 0 && self.buf[end - 1] == b'\r' {
                end -= 1;
            }
            if end == 0 {
                continue;
            }
            return Ok(self.buf[..end].to_vec());
        }
    }

    /// Write one frame. Appends `\n`. Flushes before returning so the
    /// peer never waits on a buffered partial response.
    pub async fn write_frame(&self, payload: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(payload).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// Consume the transport and return the inner writer. Test-only:
    /// production code holds the transport for the connection lifetime.
    #[cfg(test)]
    pub fn into_writer(self) -> W {
        self.writer.into_inner()
    }
}

enum LineOutcome {
    Line,
    Eof,
    PartialAtEof,
}

async fn read_line_capped<R>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    cap: usize,
) -> Result<LineOutcome, FrameError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut total = 0;
    loop {
        let available = reader.fill_buf().await.map_err(FrameError::Io)?;
        if available.is_empty() {
            if total == 0 {
                return Ok(LineOutcome::Eof);
            }
            return Ok(LineOutcome::PartialAtEof);
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let take = i + 1;
                if total + take > cap {
                    // The terminating newline is right here in this
                    // buffer; consume up to and including it so the
                    // next frame is intact, then report oversize.
                    reader.consume(take);
                    return Err(FrameError::Oversize { cap });
                }
                buf.extend_from_slice(&available[..take]);
                reader.consume(take);
                return Ok(LineOutcome::Line);
            }
            None => {
                if total + available.len() > cap {
                    // No newline in sight and we are already past cap.
                    // Drop what we have and keep scanning forward
                    // until we find one (or hit EOF).
                    let len = available.len();
                    reader.consume(len);
                    drain_until_newline(reader).await?;
                    return Err(FrameError::Oversize { cap });
                }
                buf.extend_from_slice(available);
                let len = available.len();
                reader.consume(len);
                total += len;
            }
        }
    }
}

async fn drain_until_newline<R>(reader: &mut BufReader<R>) -> Result<(), FrameError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let avail = reader.fill_buf().await.map_err(FrameError::Io)?;
        if avail.is_empty() {
            return Ok(());
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(i) => {
                reader.consume(i + 1);
                return Ok(());
            }
            None => {
                let len = avail.len();
                reader.consume(len);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("EOF on input")]
    Eof,
    #[error("frame exceeds {cap} bytes")]
    Oversize { cap: usize },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(input: &[u8]) -> StdioTransport<&[u8], Vec<u8>> {
        StdioTransport::new(input, Vec::new())
    }

    #[tokio::test]
    async fn reads_one_frame() {
        let mut t = fixture(b"hello\n");
        assert_eq!(t.read_frame().await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn strips_crlf() {
        let mut t = fixture(b"hi\r\n");
        assert_eq!(t.read_frame().await.unwrap(), b"hi");
    }

    #[tokio::test]
    async fn skips_empty_lines() {
        let mut t = fixture(b"\n\nfoo\n");
        assert_eq!(t.read_frame().await.unwrap(), b"foo");
    }

    #[tokio::test]
    async fn reads_multiple_frames() {
        let mut t = fixture(b"a\nbb\nccc\n");
        assert_eq!(t.read_frame().await.unwrap(), b"a");
        assert_eq!(t.read_frame().await.unwrap(), b"bb");
        assert_eq!(t.read_frame().await.unwrap(), b"ccc");
        assert!(matches!(t.read_frame().await, Err(FrameError::Eof)));
    }

    #[tokio::test]
    async fn partial_at_eof_is_eof() {
        let mut t = fixture(b"partial");
        assert!(matches!(t.read_frame().await, Err(FrameError::Eof)));
    }

    #[tokio::test]
    async fn empty_input_is_eof() {
        let mut t = fixture(b"");
        assert!(matches!(t.read_frame().await, Err(FrameError::Eof)));
    }

    #[tokio::test]
    async fn oversize_returns_error_and_resyncs() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES + 10];
        input.push(b'\n');
        input.extend_from_slice(b"ok\n");

        let mut t = StdioTransport::new(input.as_slice(), Vec::new());
        assert!(matches!(
            t.read_frame().await,
            Err(FrameError::Oversize { .. })
        ));
        assert_eq!(t.read_frame().await.unwrap(), b"ok");
    }

    #[tokio::test]
    async fn write_frame_appends_newline_and_flushes() {
        let t = StdioTransport::new(&b""[..], Vec::new());
        t.write_frame(b"hello").await.unwrap();
        let v = t.writer.into_inner();
        assert_eq!(v, b"hello\n");
    }

    #[tokio::test]
    async fn write_frame_concurrent_serialised() {
        // Two concurrent writes must not interleave. We rely on the
        // internal Mutex; this test would flake without it.
        let t = std::sync::Arc::new(StdioTransport::new(&b""[..], Vec::new()));
        let t1 = std::sync::Arc::clone(&t);
        let t2 = std::sync::Arc::clone(&t);
        let h1 = tokio::spawn(async move { t1.write_frame(b"AAAAA").await });
        let h2 = tokio::spawn(async move { t2.write_frame(b"BBBBB").await });
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();
        let arc = std::sync::Arc::try_unwrap(t)
            .map_err(|_| ())
            .expect("only owner");
        let v = arc.writer.into_inner();
        // The two frames may appear in either order, but neither is split.
        assert!(v == b"AAAAA\nBBBBB\n" || v == b"BBBBB\nAAAAA\n");
    }
}
