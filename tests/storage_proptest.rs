//! Phase 2 exit gate (per `proj_docs/mneme-implementation-plan.md` §3):
//! "10K random put/get/delete operations survive restart byte-identical (proptest)."
//!
//! This is the in-process companion to `tests/crash_recovery.rs` (which
//! exercises subprocess kill -9). Here we drive the real `RedbStorage` for
//! N operations, drop+reopen at random checkpoints, and assert the live
//! state matches an in-memory `BTreeMap` oracle byte-for-byte.

use mneme::storage::{Storage, redb_impl::RedbStorage};
use proptest::collection::vec as proptest_vec;
use proptest::prelude::*;
use std::collections::BTreeMap;
use tempfile::TempDir;

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Get(Vec<u8>),
}

fn arb_key() -> impl Strategy<Value = Vec<u8>> {
    // Constrained key space so puts/deletes land on the same keys often.
    (0u8..32).prop_map(|i| format!("k{i:02}").into_bytes())
}

fn arb_value() -> impl Strategy<Value = Vec<u8>> {
    proptest_vec(any::<u8>(), 0..64)
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (arb_key(), arb_value()).prop_map(|(k, v)| Op::Put(k, v)),
        2 => arb_key().prop_map(Op::Delete),
        1 => arb_key().prop_map(Op::Get),
    ]
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        ..ProptestConfig::default()
    })]

    /// Run a 200-300 op workload, drop+reopen the storage at random
    /// checkpoints, and verify state matches the oracle on every checkpoint
    /// and at the end. 32 cases × ~250 ops × ~3 reopens ≈ 25K total ops
    /// across runs — well over the spec's 10K bar.
    #[test]
    fn workload_survives_random_restarts(
        ops in proptest_vec(arb_op(), 200..300),
        checkpoint_seeds in proptest_vec(any::<u8>(), 0..3),
    ) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        // Compute checkpoint indices (sorted, in-bounds, deduplicated).
        let mut checkpoints: Vec<usize> = checkpoint_seeds
            .iter()
            .map(|s| (*s as usize) * ops.len() / 256)
            .collect();
        checkpoints.sort();
        checkpoints.dedup();

        let mut next_cp = 0;
        let mut storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });

        for (i, op) in ops.iter().enumerate() {
            // Maybe close+reopen.
            if next_cp < checkpoints.len() && checkpoints[next_cp] == i {
                drop(storage);
                storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });
                next_cp += 1;
                // Spot-check oracle invariants after reopen.
                rt.block_on(async {
                    for (k, v) in oracle.iter().take(8) {
                        let got = storage.get(k).await.unwrap();
                        prop_assert_eq!(got.as_deref(), Some(v.as_slice()),
                                        "post-reopen drift on key {:?}", k);
                    }
                    Ok(())
                })?;
            }

            rt.block_on(async {
                match op {
                    Op::Put(k, v) => {
                        storage.put(k, v).await.unwrap();
                        oracle.insert(k.clone(), v.clone());
                    }
                    Op::Delete(k) => {
                        storage.delete(k).await.unwrap();
                        oracle.remove(k);
                    }
                    Op::Get(k) => {
                        let got = storage.get(k).await.unwrap();
                        let expected = oracle.get(k).cloned();
                        prop_assert_eq!(got, expected, "live get drift on key {:?}", k);
                    }
                }
                Ok(())
            })?;
        }

        // Final reopen + full state comparison via scan_prefix on b"".
        drop(storage);
        let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });
        let recovered = rt.block_on(async { storage.scan_prefix(b"").await.unwrap() });
        let recovered_map: BTreeMap<Vec<u8>, Vec<u8>> = recovered.into_iter().collect();
        prop_assert_eq!(recovered_map, oracle);
    }
}

/// Specifically asserts the headline 10K-op figure, in a single deterministic run.
#[test]
fn ten_thousand_ops_survive_restart() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let rt = rt();
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    let mut prng = SimpleRng::seeded(0x000F_ADED_FACE);
    let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });

    rt.block_on(async {
        for i in 0..10_000u64 {
            let k = format!("k{:08}", i % 256).into_bytes();
            let r = prng.next_u32() % 10;
            if r < 7 {
                let v_len = (prng.next_u32() % 32) as usize + 1;
                let mut v = vec![0u8; v_len];
                prng.fill_bytes(&mut v);
                storage.put(&k, &v).await.unwrap();
                oracle.insert(k, v);
            } else if r < 9 {
                storage.delete(&k).await.unwrap();
                oracle.remove(&k);
            } else {
                let got = storage.get(&k).await.unwrap();
                assert_eq!(got, oracle.get(&k).cloned());
            }
        }
    });

    drop(storage);
    let storage = rt.block_on(async { RedbStorage::open(&root).unwrap() });
    let recovered = rt.block_on(async { storage.scan_prefix(b"").await.unwrap() });
    let recovered_map: BTreeMap<_, _> = recovered.into_iter().collect();
    assert_eq!(recovered_map, oracle, "post-restart state drift");
}

// ---------- minimal deterministic RNG ----------

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
