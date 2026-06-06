//! Property tests for the encryption-at-rest envelope (ADR-0013 D3/D4/D5).
//!
//! Three invariants are exercised:
//!
//! 1. **Round-trip:** for any plaintext, AAD domain, and position, the
//!    sealed envelope decrypts back to the original plaintext under the
//!    same key + domain + position.
//! 2. **Tamper detection:** flipping a single bit anywhere past the
//!    magic+version prefix must cause `open` to return an error. The
//!    AEAD must never silently accept a corrupted envelope.
//! 3. **Domain / position binding:** an envelope sealed under one
//!    (domain, position) must fail to open under any *different*
//!    (domain, position) pair, even with the correct key.

use mneme::crypto::{AadDomain, Aead, Dek};
use proptest::collection::vec as proptest_vec;
use proptest::prelude::*;

fn arb_domain() -> impl Strategy<Value = AadDomain> {
    prop_oneof![
        Just(AadDomain::Redb),
        Just(AadDomain::Wal),
        Just(AadDomain::Hnsw),
        Just(AadDomain::Pinned),
        Just(AadDomain::Session),
        Just(AadDomain::Cold),
        Just(AadDomain::Keystore),
    ]
}

fn arb_plaintext() -> impl Strategy<Value = Vec<u8>> {
    proptest_vec(any::<u8>(), 0..2048)
}

fn arb_position() -> impl Strategy<Value = Vec<u8>> {
    proptest_vec(any::<u8>(), 0..64)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn round_trip_preserves_plaintext(
        domain in arb_domain(),
        position in arb_position(),
        plaintext in arb_plaintext(),
    ) {
        let aead = Aead::new(&Dek::generate().unwrap());
        let env = aead.seal(domain, &position, &plaintext).unwrap();
        let out = aead.open(domain, &position, &env).unwrap();
        prop_assert_eq!(out, plaintext);
    }

    #[test]
    fn bit_flip_anywhere_past_prefix_fails(
        domain in arb_domain(),
        position in arb_position(),
        plaintext in arb_plaintext(),
        flip_offset in 0usize..4096,
        flip_bit in 0u8..8,
    ) {
        let aead = Aead::new(&Dek::generate().unwrap());
        let mut env = aead.seal(domain, &position, &plaintext).unwrap();
        // The fixed prefix is magic(4) + version(1) = 5 bytes. Skip it.
        let safe_offset = 5 + (flip_offset % (env.len() - 5));
        env[safe_offset] ^= 1 << flip_bit;
        prop_assert!(aead.open(domain, &position, &env).is_err());
    }

    #[test]
    fn domain_mismatch_fails(
        original in arb_domain(),
        other in arb_domain(),
        position in arb_position(),
        plaintext in arb_plaintext(),
    ) {
        prop_assume!(original != other);
        let aead = Aead::new(&Dek::generate().unwrap());
        let env = aead.seal(original, &position, &plaintext).unwrap();
        prop_assert!(aead.open(other, &position, &env).is_err());
    }

    #[test]
    fn position_mismatch_fails(
        domain in arb_domain(),
        pos_a in arb_position(),
        pos_b in arb_position(),
        plaintext in arb_plaintext(),
    ) {
        prop_assume!(pos_a != pos_b);
        let aead = Aead::new(&Dek::generate().unwrap());
        let env = aead.seal(domain, &pos_a, &plaintext).unwrap();
        prop_assert!(aead.open(domain, &pos_b, &env).is_err());
    }

    #[test]
    fn different_keys_never_interoperate(
        domain in arb_domain(),
        position in arb_position(),
        plaintext in arb_plaintext(),
    ) {
        let alice = Aead::new(&Dek::generate().unwrap());
        let bob = Aead::new(&Dek::generate().unwrap());
        let env = alice.seal(domain, &position, &plaintext).unwrap();
        prop_assert!(bob.open(domain, &position, &env).is_err());
    }
}
