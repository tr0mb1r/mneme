//! On-disk envelope for encrypted records (ADR-0013 D3/D4/D5).
//!
//! Wire format:
//!
//! ```text
//!  +--------+--------+----------+--------------------+
//!  | u32 BE | u8     | 24 bytes | variable           |
//!  | magic  | ver    | nonce    | ciphertext || tag  |
//!  +--------+--------+----------+--------------------+
//!     "MNE1"  0x01
//! ```
//!
//! Each call to [`Aead::seal`] / [`Aead::open`] binds the surface that
//! the blob belongs to (a redb table value, a WAL frame, a session
//! snapshot, etc.) via [`AadDomain`]. Mixing a ciphertext between
//! surfaces fails authentication hard — see ADR-0013 D4.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead as _, KeyInit, Payload},
};
use rand::{TryRngCore, rngs::OsRng};

use crate::{
    MnemeError, Result,
    crypto::key::{Dek, KEY_LEN, Kek},
};

/// Magic prefix identifying a mneme encrypted envelope. The four bytes
/// `MNE1` (0x4D, 0x4E, 0x45, 0x31) discriminate encrypted blobs from
/// plaintext legacy data dirs without ambiguity.
pub const MAGIC: [u8; 4] = *b"MNE1";

/// Current envelope version. Bumped only when the on-disk layout
/// itself changes (e.g. swap of AEAD primitive). v1.2 ships `0x01`.
pub const ENVELOPE_VERSION: u8 = 0x01;

/// XChaCha20-Poly1305 nonce size (24 bytes — ADR-0013 D3).
pub const NONCE_LEN: usize = 24;

/// XChaCha20-Poly1305 authentication tag size (16 bytes).
pub const TAG_LEN: usize = 16;

/// Fixed-size prefix that precedes every ciphertext on disk.
pub const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN;

/// AAD domain prefixes binding each ciphertext to its surface.
///
/// Per ADR-0013 D4, every encrypt/decrypt call concatenates the
/// domain's [`AadDomain::tag`] with a position-specific suffix
/// (e.g. row key, LSN, session id) to form the AAD. An attacker who
/// swaps a ciphertext between surfaces — say, moving a session
/// snapshot blob into the redb data table — will see decryption fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AadDomain {
    /// `b"redb"` — redb table values (L3 events, L4 memories).
    Redb,
    /// `b"wal "` — WAL frame payloads.
    Wal,
    /// `b"hnsw"` — HNSW snapshot file.
    Hnsw,
    /// `b"pin "` — procedural pinned.jsonl lines.
    Pinned,
    /// `b"sess"` — session checkpoint snapshots.
    Session,
    /// `b"cold"` — cold-tier zstd archives.
    Cold,
    /// `b"kek "` — wrapped DEK inside keystore.json.
    Keystore,
}

impl AadDomain {
    pub fn tag(self) -> &'static [u8; 4] {
        match self {
            AadDomain::Redb => b"redb",
            AadDomain::Wal => b"wal ",
            AadDomain::Hnsw => b"hnsw",
            AadDomain::Pinned => b"pin ",
            AadDomain::Session => b"sess",
            AadDomain::Cold => b"cold",
            AadDomain::Keystore => b"kek ",
        }
    }
}

/// AEAD wrapper holding an initialised cipher for one DEK.
///
/// The cipher itself is cheap to clone (a couple of register-sized
/// fields), but we hold one instance per [`Aead`] to avoid re-running
/// the key schedule on every call.
pub struct Aead {
    cipher: XChaCha20Poly1305,
}

/// Sealed trait for the two symmetric key types in this module.
///
/// Both [`Dek`] and [`Kek`] are 32-byte XChaCha20-Poly1305 keys. The
/// distinction exists at the type level to keep wrap (KEK) and
/// record (DEK) paths from accidentally mixing keys — but the AEAD
/// itself is agnostic and accepts either through this trait.
pub trait AeadKey: private::Sealed {
    fn key_bytes(&self) -> &[u8; KEY_LEN];
}

impl AeadKey for Dek {
    fn key_bytes(&self) -> &[u8; KEY_LEN] {
        self.as_bytes()
    }
}
impl AeadKey for Kek {
    fn key_bytes(&self) -> &[u8; KEY_LEN] {
        self.as_bytes()
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::Dek {}
    impl Sealed for super::Kek {}
}

impl Aead {
    /// Construct an [`Aead`] from any [`AeadKey`] (i.e. a [`Dek`] for
    /// per-record encryption, or a [`Kek`] when wrapping the DEK in
    /// the keystore).
    pub fn new<K: AeadKey>(key: &K) -> Self {
        let cipher = XChaCha20Poly1305::new(key.key_bytes().into());
        Self { cipher }
    }

    /// Encrypt `plaintext` against the given AAD domain + position,
    /// returning the fully-framed envelope (magic + version + nonce +
    /// ciphertext+tag).
    ///
    /// A fresh 24-byte nonce is drawn from the OS CSPRNG per call.
    /// XChaCha20's 192-bit nonce space makes random nonces safe — the
    /// birthday collision threshold is ~2^96 records (ADR-0013 D3).
    pub fn seal(&self, domain: AadDomain, position: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|e| MnemeError::Crypto(format!("OS RNG: {e}")))?;
        let nonce = XNonce::from_slice(&nonce_bytes);

        let aad = build_aad(domain, position);
        let payload = Payload {
            msg: plaintext,
            aad: &aad,
        };
        let ciphertext = self.cipher.encrypt(nonce, payload)?;

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(&MAGIC);
        out.push(ENVELOPE_VERSION);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt an envelope produced by [`Aead::seal`].
    ///
    /// Returns [`MnemeError::Crypto`] if:
    /// - the magic prefix doesn't match,
    /// - the version byte is unknown,
    /// - the input is shorter than the fixed header,
    /// - the AEAD tag doesn't verify (wrong key, wrong AAD domain,
    ///   wrong position, or tampering).
    pub fn open(&self, domain: AadDomain, position: &[u8], envelope: &[u8]) -> Result<Vec<u8>> {
        if envelope.len() < HEADER_LEN + TAG_LEN {
            return Err(MnemeError::Crypto("envelope too short".to_string()));
        }
        if envelope[..MAGIC.len()] != MAGIC {
            return Err(MnemeError::Crypto("missing MNE1 magic".to_string()));
        }
        let version = envelope[MAGIC.len()];
        if version != ENVELOPE_VERSION {
            return Err(MnemeError::Crypto(format!(
                "unsupported envelope version: {version:#x}"
            )));
        }

        let nonce_start = MAGIC.len() + 1;
        let nonce = XNonce::from_slice(&envelope[nonce_start..nonce_start + NONCE_LEN]);
        let ciphertext = &envelope[HEADER_LEN..];

        let aad = build_aad(domain, position);
        let payload = Payload {
            msg: ciphertext,
            aad: &aad,
        };
        let plaintext = self.cipher.decrypt(nonce, payload)?;
        Ok(plaintext)
    }
}

fn build_aad(domain: AadDomain, position: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(4 + position.len());
    aad.extend_from_slice(domain.tag());
    aad.extend_from_slice(position);
    aad
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::key::Dek;

    fn aead() -> Aead {
        Aead::new(&Dek::generate().unwrap())
    }

    #[test]
    fn round_trip_preserves_plaintext() {
        let a = aead();
        let pt = b"hello, mneme";
        let env = a.seal(AadDomain::Redb, b"row-1", pt).unwrap();
        let out = a.open(AadDomain::Redb, b"row-1", &env).unwrap();
        assert_eq!(out, pt);
    }

    #[test]
    fn envelope_starts_with_magic_and_version() {
        let env = aead().seal(AadDomain::Wal, b"lsn-1", b"x").unwrap();
        assert_eq!(&env[..4], &MAGIC);
        assert_eq!(env[4], ENVELOPE_VERSION);
    }

    #[test]
    fn nonces_are_unique_across_calls() {
        let a = aead();
        let e1 = a.seal(AadDomain::Redb, b"k", b"v").unwrap();
        let e2 = a.seal(AadDomain::Redb, b"k", b"v").unwrap();
        // Same plaintext + same key + same AAD must still yield distinct
        // ciphertext because the nonce is fresh per call.
        assert_ne!(e1[5..5 + NONCE_LEN], e2[5..5 + NONCE_LEN]);
        assert_ne!(e1, e2);
    }

    #[test]
    fn wrong_domain_fails_to_open() {
        let a = aead();
        let env = a.seal(AadDomain::Redb, b"k", b"secret").unwrap();
        let err = a.open(AadDomain::Wal, b"k", &env);
        assert!(err.is_err(), "domain mismatch must fail");
    }

    #[test]
    fn wrong_position_fails_to_open() {
        let a = aead();
        let env = a.seal(AadDomain::Redb, b"row-1", b"secret").unwrap();
        let err = a.open(AadDomain::Redb, b"row-2", &env);
        assert!(err.is_err(), "position mismatch must fail");
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let a = aead();
        let b = aead();
        let env = a.seal(AadDomain::Redb, b"k", b"secret").unwrap();
        let err = b.open(AadDomain::Redb, b"k", &env);
        assert!(err.is_err(), "wrong key must fail");
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let a = aead();
        let mut env = a.seal(AadDomain::Redb, b"k", b"secret").unwrap();
        // Flip a bit in the ciphertext body, past the header.
        let last = env.len() - 1;
        env[last] ^= 0x01;
        let err = a.open(AadDomain::Redb, b"k", &env);
        assert!(err.is_err(), "tampered ciphertext must fail");
    }

    #[test]
    fn truncated_envelope_fails_to_open() {
        let a = aead();
        let env = a.seal(AadDomain::Redb, b"k", b"secret").unwrap();
        let err = a.open(AadDomain::Redb, b"k", &env[..HEADER_LEN]);
        assert!(err.is_err(), "header-only envelope must fail");
    }

    #[test]
    fn missing_magic_fails_to_open() {
        let a = aead();
        let mut env = a.seal(AadDomain::Redb, b"k", b"x").unwrap();
        env[0] = 0xFF;
        let err = a.open(AadDomain::Redb, b"k", &env);
        assert!(matches!(
            err,
            Err(crate::MnemeError::Crypto(ref m)) if m.contains("magic")
        ));
    }

    #[test]
    fn unknown_version_fails_to_open() {
        let a = aead();
        let mut env = a.seal(AadDomain::Redb, b"k", b"x").unwrap();
        env[4] = 0x02;
        let err = a.open(AadDomain::Redb, b"k", &env);
        assert!(matches!(
            err,
            Err(crate::MnemeError::Crypto(ref m)) if m.contains("version")
        ));
    }

    #[test]
    fn all_domain_tags_are_distinct_and_four_bytes() {
        use AadDomain::*;
        let all = [Redb, Wal, Hnsw, Pinned, Session, Cold, Keystore];
        let mut seen = std::collections::HashSet::new();
        for d in all {
            assert_eq!(d.tag().len(), 4);
            assert!(seen.insert(*d.tag()), "duplicate domain tag for {d:?}");
        }
    }

    #[test]
    fn position_is_part_of_aad_across_long_inputs() {
        // Regression guard: ensure position bytes participate in AAD
        // even when ciphertext is longer than one cipher block.
        let a = aead();
        let pt = vec![0xCD; 4096];
        let env = a.seal(AadDomain::Redb, b"row-A", &pt).unwrap();
        let bad = a.open(AadDomain::Redb, b"row-B", &env);
        assert!(bad.is_err());
        let good = a.open(AadDomain::Redb, b"row-A", &env).unwrap();
        assert_eq!(good, pt);
    }
}
