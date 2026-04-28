//! Property tests for the WAL frame format.
//!
//! Three invariants are exercised:
//!
//! 1. **Round-trip:** any sequence of `WalOp`s written through `WalWriter`
//!    must come back byte-identical via `replay`, in LSN order.
//! 2. **Bit-flip detection:** flipping a single byte anywhere inside an
//!    encoded WAL frame must cause replay to either drop the affected
//!    record (torn-tail behaviour) OR return strictly fewer records than
//!    were written. CRC32C must never silently accept corruption.
//! 3. **Truncation safety:** truncating a WAL file at any offset must yield
//!    a clean replay (no panics, no errors) returning a prefix of the
//!    written records.

use mneme::storage::wal::{self, WalOp, WalWriter};
use proptest::collection::vec as proptest_vec;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use tempfile::TempDir;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn arb_walop() -> impl Strategy<Value = WalOp> {
    prop_oneof![
        (
            proptest_vec(any::<u8>(), 0..256),
            proptest_vec(any::<u8>(), 0..1024)
        )
            .prop_map(|(k, v)| WalOp::Put { key: k, value: v }),
        proptest_vec(any::<u8>(), 0..256).prop_map(|k| WalOp::Delete { key: k }),
    ]
}

/// Apply ops to an oracle map (last write wins; delete removes).
fn apply_to_oracle(ops: &[WalOp], oracle: &mut BTreeMap<Vec<u8>, Vec<u8>>) {
    for op in ops {
        match op {
            WalOp::Put { key, value } => {
                oracle.insert(key.clone(), value.clone());
            }
            WalOp::Delete { key } => {
                oracle.remove(key);
            }
            // arb_walop only generates Put/Delete; vector variants are
            // exercised in src/index/delta.rs tests, not here.
            WalOp::VectorInsert { .. } | WalOp::VectorDelete { .. } => {}
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn round_trip_arbitrary_workload(ops in proptest_vec(arb_walop(), 0..50)) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();
        let written: Vec<WalOp> = ops.clone();

        rt.block_on(async {
            let writer = WalWriter::open(dir, 1).unwrap();
            for op in &ops {
                writer.append(op.clone()).await.unwrap();
            }
            writer.shutdown().unwrap();
        });

        let recs: Vec<_> = wal::replay(dir).unwrap().collect::<mneme::Result<_>>().unwrap();
        prop_assert_eq!(recs.len(), written.len());
        for (i, rec) in recs.iter().enumerate() {
            prop_assert_eq!(rec.lsn, (i as u64) + 1);
            prop_assert_eq!(&rec.op, &written[i]);
        }
    }

    #[test]
    fn truncation_yields_prefix(
        ops in proptest_vec(arb_walop(), 1..30),
        truncate_to in 0u64..2048,
    ) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let writer = WalWriter::open(dir, 1).unwrap();
            for op in &ops {
                writer.append(op.clone()).await.unwrap();
            }
            writer.shutdown().unwrap();
        });

        // Find the active segment and truncate it.
        let segments: Vec<_> = std::fs::read_dir(dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.starts_with("wal-") && s.ends_with(".log")
            })
            .map(|e| e.path())
            .collect();
        prop_assert_eq!(segments.len(), 1);
        let active = &segments[0];

        let actual_size = std::fs::metadata(active).unwrap().len();
        let target = truncate_to.min(actual_size);
        let f = OpenOptions::new().write(true).open(active).unwrap();
        f.set_len(target).unwrap();
        f.sync_data().unwrap();
        drop(f);

        // Replay must succeed cleanly and return some prefix (possibly empty).
        let recs: Vec<_> = wal::replay(dir).unwrap()
            .collect::<mneme::Result<_>>()
            .expect("replay must not error on truncation");
        prop_assert!(recs.len() <= ops.len(),
                     "expected ≤ {} records after truncation, got {}", ops.len(), recs.len());
        for (i, rec) in recs.iter().enumerate() {
            prop_assert_eq!(rec.lsn, (i as u64) + 1);
            prop_assert_eq!(&rec.op, &ops[i]);
        }
    }

    #[test]
    fn bit_flip_is_caught(
        op in arb_walop(),
        flip_offset in 0usize..256,
        flip_bit in 0u8..8,
    ) {
        // Single-record WAL, flip one bit anywhere in the frame, assert
        // replay does NOT silently return the original record.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let rt = rt();

        rt.block_on(async {
            let writer = WalWriter::open(dir, 1).unwrap();
            writer.append(op.clone()).await.unwrap();
            writer.shutdown().unwrap();
        });

        let segs: Vec<_> = std::fs::read_dir(dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("wal-"))
            .collect();
        let active = &segs[0];
        let size = std::fs::metadata(active).unwrap().len();
        let off = (flip_offset as u64) % size;

        // Flip one bit at offset `off`.
        let mut f = OpenOptions::new().read(true).write(true).open(active).unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut byte = [0u8; 1];
        std::io::Read::read_exact(&mut f, &mut byte).unwrap();
        byte[0] ^= 1u8 << flip_bit;
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(&byte).unwrap();
        f.sync_data().unwrap();
        drop(f);

        // Replay must either return 0 records (torn tail) or, in the rare
        // case the flip lands inside payload bytes that postcard still
        // decodes as a *different* WalOp with a happenstance-matching CRC,
        // produce something that is NOT the original op. CRC32C must not
        // silently confirm the original.
        let result: Result<Vec<_>, _> = wal::replay(dir).unwrap().collect();
        match result {
            Ok(recs) => {
                if recs.len() == 1 {
                    prop_assert_ne!(&recs[0].op, &op,
                                    "CRC32C must catch a single-bit flip");
                }
                // 0 records is the expected case (CRC mismatch terminates replay).
            }
            Err(_) => {
                // Decode error after CRC pass would be unusual; not a failure.
            }
        }
    }
}

/// Sanity-check that the oracle helper reflects what replay produces.
#[test]
fn oracle_matches_replay() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let rt = rt();

    let ops = vec![
        WalOp::Put {
            key: b"a".to_vec(),
            value: b"1".to_vec(),
        },
        WalOp::Put {
            key: b"b".to_vec(),
            value: b"2".to_vec(),
        },
        WalOp::Put {
            key: b"a".to_vec(),
            value: b"3".to_vec(),
        },
        WalOp::Delete { key: b"b".to_vec() },
    ];
    let mut oracle = BTreeMap::new();
    apply_to_oracle(&ops, &mut oracle);

    rt.block_on(async {
        let writer = WalWriter::open(dir, 1).unwrap();
        for op in &ops {
            writer.append(op.clone()).await.unwrap();
        }
        writer.shutdown().unwrap();
    });

    let mut replayed = BTreeMap::new();
    for r in wal::replay(dir).unwrap() {
        let rec = r.unwrap();
        match rec.op {
            WalOp::Put { key, value } => {
                replayed.insert(key, value);
            }
            WalOp::Delete { key } => {
                replayed.remove(&key);
            }
            // arb_walop never produces these; safe to ignore.
            WalOp::VectorInsert { .. } | WalOp::VectorDelete { .. } => {}
        }
    }
    assert_eq!(replayed, oracle);
}
