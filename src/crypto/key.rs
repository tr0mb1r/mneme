//! Symmetric key types for the encryption-at-rest envelope (ADR-0013 D1).
//!
//! Both [`Dek`] and [`Kek`] are 32-byte secrets that scrub themselves on
//! drop via [`zeroize::ZeroizeOnDrop`]. They are deliberately not `Copy`
//! and do not implement `Debug` (only a placeholder that hides the
//! bytes) to make accidental logging or cloning into structured fields
//! fail at compile time.
//!
//! The on-disk envelope never stores a [`Dek`] in plaintext: the
//! keystore holds a wrapped DEK (D5), and the daemon unwraps it once
//! at boot using the [`Kek`] sourced from the OS keyring or the
//! recovery mnemonic (D6/D9).

use rand::{TryRngCore, rngs::OsRng};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{MnemeError, Result};

/// Length of every symmetric key in this module (XChaCha20-Poly1305
/// key size — 256 bits, ADR-0013 D3).
pub const KEY_LEN: usize = 32;

/// Data Encryption Key. Encrypts every record at rest.
///
/// Lives only in daemon RAM during runtime (ADR-0013 D1/D9). Generated
/// once at `mneme encrypt` time via [`Dek::generate`]; reconstructed at
/// every daemon boot by unwrapping the encrypted blob in
/// `~/.mneme/keystore.json`.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Dek([u8; KEY_LEN]);

/// Key Encryption Key. Wraps the DEK.
///
/// Sourced from the OS keyring on the happy path (ADR-0013 D6) or
/// derived from the 12-word recovery mnemonic on the recovery path.
/// Long-lived across daemon restarts; rotated by `mneme rekey` (D10),
/// which generates a fresh KEK and re-wraps the *same* DEK so no data
/// has to be rewritten.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Kek([u8; KEY_LEN]);

macro_rules! impl_key {
    ($ty:ident, $label:literal) => {
        impl $ty {
            /// Generate a fresh key from the OS CSPRNG.
            pub fn generate() -> Result<Self> {
                let mut bytes = [0u8; KEY_LEN];
                OsRng
                    .try_fill_bytes(&mut bytes)
                    .map_err(|e| MnemeError::Crypto(format!("OS RNG: {e}")))?;
                Ok(Self(bytes))
            }

            /// Wrap an existing 32-byte secret. Caller is responsible
            /// for sourcing it from a trusted channel (keyring read,
            /// mnemonic derivation, keystore unwrap).
            pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
                Self(bytes)
            }

            /// Borrow the raw key bytes. Use only when handing the key
            /// to a cipher; never log, never clone into a non-zeroize
            /// container.
            pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
                &self.0
            }
        }

        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                // Never print the bytes; the type name and a redacted
                // marker make accidental `{:?}` formatting safe.
                f.debug_tuple($label).field(&"<redacted>").finish()
            }
        }
    };
}

impl_key!(Dek, "Dek");
impl_key!(Kek, "Kek");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_returns_unique_keys() {
        let a = Dek::generate().unwrap();
        let b = Dek::generate().unwrap();
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn keys_have_full_length() {
        let k = Kek::generate().unwrap();
        assert_eq!(k.as_bytes().len(), KEY_LEN);
    }

    #[test]
    fn debug_does_not_leak_bytes() {
        let k = Dek::from_bytes([0x42; KEY_LEN]);
        let dbg = format!("{k:?}");
        assert!(dbg.contains("redacted"));
        assert!(!dbg.contains("42"));
    }

    #[test]
    fn from_bytes_round_trips() {
        let raw = [0xAB; KEY_LEN];
        let k = Dek::from_bytes(raw);
        assert_eq!(k.as_bytes(), &raw);
    }
}
