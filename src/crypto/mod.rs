//! Encryption-at-rest primitives (ADR-0013 Phase 1).
//!
//! This module is intentionally narrow: it provides the cryptographic
//! building blocks — AEAD envelope, key types, BIP39 recovery mnemonic —
//! and nothing else. Keystore file format and OS-keyring integration
//! live in `crate::crypto::keystore` (Phase 2). The encrypted Storage
//! wrapper lives in `crate::storage` (Phase 3). The daemon boot path
//! lives in `crate::cli::run` (Phase 7).
//!
//! ## Architecture (ADR-0013)
//!
//! ```text
//!     12-word BIP39 mnemonic  ──┐
//!     (128-bit entropy)         │ PBKDF2-HMAC-SHA512, 2048 iter
//!                               ▼
//!                          KEK (32 bytes) ────────┐
//!                          (in OS keyring         │ XChaCha20-Poly1305
//!                          or derived             │ wrap
//!                          from mnemonic)         ▼
//!                                            Wrapped DEK
//!                                            (~/.mneme/keystore.json)
//!                                                 │
//!                                                 │ unwrap at boot
//!                                                 ▼
//!                          DEK (32 bytes, daemon RAM, Zeroize on drop)
//!                                                 │
//!                                                 │ XChaCha20-Poly1305
//!                                                 │ per record, random
//!                                                 │ 24-byte nonce, AAD
//!                                                 │ bound to surface
//!                                                 ▼
//!                          Encrypted records on disk
//! ```
//!
//! ## AAD domains (ADR-0013 D4)
//!
//! Every encrypt/decrypt call binds an Associated Data prefix that
//! identifies its surface. Mixing a ciphertext from one surface into
//! another fails decryption hard rather than producing garbage.
//!
//! See [`AadDomain`] for the canonical prefixes.

mod envelope;
mod key;
pub mod keyring;
pub mod keystore;
mod mnemonic;

pub use envelope::{AadDomain, Aead, ENVELOPE_VERSION, MAGIC};
pub use key::{Dek, Kek};
pub use keyring::{KEYRING_SERVICE, KekStore, OsKeyring, account_for};
pub use keystore::{KEYSTORE_FILENAME, KEYSTORE_VERSION, Keystore, keystore_path};
pub use mnemonic::{MNEMONIC_WORDS, Mnemonic, VerifyChallenge};

use crate::MnemeError;

impl From<chacha20poly1305::Error> for MnemeError {
    fn from(_: chacha20poly1305::Error) -> Self {
        // The AEAD crate intentionally returns an opaque error (don't
        // leak whether the failure was nonce, tag, or AAD mismatch).
        // We propagate the opaque-ness.
        MnemeError::Crypto("AEAD operation failed".to_string())
    }
}

impl From<bip39::Error> for MnemeError {
    fn from(err: bip39::Error) -> Self {
        MnemeError::Crypto(format!("mnemonic: {err}"))
    }
}
