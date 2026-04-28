//! Property tests for the stdio transport and JSON-RPC parser.
//!
//! The invariant: no input — random bytes, malformed JSON, oversized
//! frames, mid-message EOF, embedded NULs — should panic. Every error
//! path either returns a typed error or surfaces as EOF; the server
//! must always be able to keep going (or shut down cleanly).

use mneme::mcp::jsonrpc::{ParseError, parse_inbound};
use mneme::mcp::transport::stdio::{FrameError, MAX_FRAME_BYTES, StdioTransport};
use proptest::prelude::*;

proptest! {
    /// `parse_inbound` never panics on arbitrary bytes. Either it
    /// returns one of the three message variants, or it returns a
    /// typed `ParseError`.
    #[test]
    fn parse_inbound_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = parse_inbound(&bytes);
    }

    /// `parse_inbound` never panics on bytes that are valid UTF-8.
    /// (Separate strategy because the byte strategy almost never
    /// produces well-formed UTF-8 by chance.)
    #[test]
    fn parse_inbound_never_panics_on_text(s in "\\PC{0,256}") {
        let _ = parse_inbound(s.as_bytes());
    }

    /// The transport's read loop never panics on arbitrary input.
    /// Each line is processed independently; bad lines surface as
    /// errors but do not corrupt state for subsequent reads.
    #[test]
    fn transport_read_loop_never_panics(
        chunks in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..128),
            0..16,
        )
    ) {
        let mut input: Vec<u8> = Vec::new();
        for c in &chunks {
            input.extend_from_slice(c);
            input.push(b'\n');
        }
        // Run on a current-thread runtime per case (cheap; proptest is sync).
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut t = StdioTransport::new(input.as_slice(), Vec::<u8>::new());
            // Read up to N+1 frames, expecting EOF eventually.
            for _ in 0..(chunks.len() + 1) {
                match t.read_frame().await {
                    Ok(_frame) => {}
                    Err(FrameError::Eof) => break,
                    Err(FrameError::Oversize { .. }) => {} // never occurs at these sizes
                    Err(FrameError::Io(_)) => break,
                }
            }
        });
    }

    /// Combine: every successful frame either parses to a JSON-RPC
    /// message or yields a typed parse error. No panics.
    #[test]
    fn transport_then_parser_never_panics(
        chunks in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..96),
            0..8,
        )
    ) {
        let mut input: Vec<u8> = Vec::new();
        for c in &chunks {
            input.extend_from_slice(c);
            input.push(b'\n');
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut t = StdioTransport::new(input.as_slice(), Vec::<u8>::new());
            loop {
                match t.read_frame().await {
                    Ok(frame) => {
                        match parse_inbound(&frame) {
                            Ok(_) | Err(ParseError::InvalidJson(_)) => {}
                            Err(_) => {} // any other parse error is fine; not a panic
                        }
                    }
                    Err(FrameError::Eof) => break,
                    Err(_) => break,
                }
            }
        });
    }

    /// Oversize handling: a giant pre-newline blob followed by a
    /// normal frame must yield `Oversize` and *not* lose the next
    /// frame (the regression we fixed earlier).
    #[test]
    fn oversize_then_normal_frame_resyncs(extra in 1usize..1024) {
        let big_size = MAX_FRAME_BYTES + extra;
        let mut input = vec![b'a'; big_size];
        input.push(b'\n');
        input.extend_from_slice(b"ok\n");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut t = StdioTransport::new(input.as_slice(), Vec::<u8>::new());
            let first = t.read_frame().await;
            let is_oversize = matches!(first, Err(FrameError::Oversize { .. }));
            prop_assert!(is_oversize);
            let second = t.read_frame().await.unwrap();
            prop_assert_eq!(second, b"ok".to_vec());
            Ok(())
        }).unwrap();
    }
}
