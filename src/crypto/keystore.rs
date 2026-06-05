//! `~/.mneme/keystore.json` — wrapped DEK + KEK provenance (ADR-0013 D5/D6).
//!
//! The keystore file is the *only* on-disk artifact that ties the
//! encrypted data dir to its key material. It contains:
//!
//! - The DEK, wrapped under the KEK via XChaCha20-Poly1305 with
//!   [`AadDomain::Keystore`] AAD bound to the keystore schema version.
//! - The KDF and AEAD identifiers (forward-compat with future
//!   primitive swaps).
//! - The keyring service/account so the daemon knows which entry to
//!   read at boot.
//! - Creation + rotation timestamps for audit.
//! - A flag indicating whether the user successfully completed the
//!   mnemonic-verification challenge at generation time.
//!
//! Absence of `keystore.json` in the data dir means the dir is
//! plaintext (legacy v1.0/v1.1 mode) — never an error.
//!
//! Permissions: `0o600` on unix. The KEK itself never lands on disk
//! in plaintext; only the *wrapped* DEK does, and the wrap is opaque
//! to anyone without the KEK.

use crate::{
    MnemeError, Result,
    crypto::{
        envelope::{AadDomain, Aead},
        key::{Dek, KEY_LEN, Kek},
        keyring::{KEYRING_SERVICE, account_for},
    },
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The filename inside the data directory.
pub const KEYSTORE_FILENAME: &str = "keystore.json";

/// Current keystore schema version (ADR-0013 D5). The version byte is
/// also bound into the AAD when wrapping the DEK so a future v2
/// keystore cannot be mis-opened by v1 code.
pub const KEYSTORE_VERSION: u32 = 1;

/// File permissions for the keystore on unix.
#[cfg(unix)]
const KEYSTORE_MODE: u32 = 0o600;

/// The on-disk JSON shape. Field names are stable: this is the
/// long-term portable contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keystore {
    /// Keystore schema version. Required.
    pub version: u32,
    /// Key derivation function descriptor.
    pub kdf: KdfDescriptor,
    /// AEAD identifier — `"xchacha20poly1305"` at this revision.
    pub aead: String,
    /// Base64-encoded wrapped DEK envelope (magic + ver + nonce + ct||tag).
    pub wrapped_dek: String,
    /// Where the KEK lives on this machine.
    pub keyring: KeyringDescriptor,
    /// First-generated timestamp (set by `mneme encrypt`).
    pub created_at: DateTime<Utc>,
    /// Most-recent rotation timestamp (set by `mneme rekey`). `None`
    /// until the first rotation.
    pub rotated_at: Option<DateTime<Utc>>,
    /// `true` when the user completed the [`VerifyChallenge`] at
    /// generation. Auditable signal that the mnemonic was actually
    /// written down rather than skipped past.
    ///
    /// [`VerifyChallenge`]: crate::crypto::VerifyChallenge
    pub mnemonic_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KdfDescriptor {
    /// `"bip39-pbkdf2-hmac-sha512"` at this revision.
    pub algo: String,
    /// PBKDF2 iterations. BIP39 fixes this at 2048.
    pub iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyringDescriptor {
    pub service: String,
    pub account: String,
}

impl Keystore {
    /// Wrap a freshly-generated DEK under the supplied KEK and build
    /// a new keystore record. Does NOT touch disk — call [`Keystore::write`].
    pub fn create(data_dir: &Path, dek: &Dek, kek: &Kek, mnemonic_verified: bool) -> Result<Self> {
        let aead = Aead::new(kek);
        let wrapped = aead.seal(
            AadDomain::Keystore,
            &KEYSTORE_VERSION.to_be_bytes(),
            dek.as_bytes(),
        )?;
        Ok(Self {
            version: KEYSTORE_VERSION,
            kdf: KdfDescriptor {
                algo: "bip39-pbkdf2-hmac-sha512".to_string(),
                iterations: 2048,
            },
            aead: "xchacha20poly1305".to_string(),
            wrapped_dek: B64.encode(&wrapped),
            keyring: KeyringDescriptor {
                service: KEYRING_SERVICE.to_string(),
                account: account_for(data_dir),
            },
            created_at: Utc::now(),
            rotated_at: None,
            mnemonic_verified,
        })
    }

    /// Unwrap the DEK using the supplied KEK. Returns
    /// [`MnemeError::Crypto`] if the KEK is wrong, the wrap is
    /// tampered, or the schema version is unknown.
    pub fn unwrap_dek(&self, kek: &Kek) -> Result<Dek> {
        self.verify_known_version()?;
        let wrapped = B64
            .decode(&self.wrapped_dek)
            .map_err(|e| MnemeError::Crypto(format!("keystore base64 decode: {e}")))?;
        let aead = Aead::new(kek);
        let plaintext = aead.open(AadDomain::Keystore, &self.version.to_be_bytes(), &wrapped)?;
        if plaintext.len() != KEY_LEN {
            return Err(MnemeError::Crypto(format!(
                "unwrapped DEK has wrong length: {} != {KEY_LEN}",
                plaintext.len()
            )));
        }
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&plaintext);
        Ok(Dek::from_bytes(bytes))
    }

    /// Re-wrap the same DEK under a fresh KEK. Used by
    /// `mneme rekey` — the DEK is unchanged so no record on disk
    /// needs to be rewritten; only this keystore file is updated.
    /// Sets `rotated_at` to the current UTC time.
    pub fn rewrap(&mut self, dek: &Dek, new_kek: &Kek) -> Result<()> {
        self.verify_known_version()?;
        let aead = Aead::new(new_kek);
        let wrapped = aead.seal(
            AadDomain::Keystore,
            &self.version.to_be_bytes(),
            dek.as_bytes(),
        )?;
        self.wrapped_dek = B64.encode(&wrapped);
        self.rotated_at = Some(Utc::now());
        Ok(())
    }

    /// Load the keystore from `data_dir`. Returns `Ok(None)` if the
    /// file is absent (plaintext data dir). Returns `Err` on parse
    /// failure or unsupported schema version.
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = keystore_path(data_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MnemeError::Io(e)),
        };
        let parsed: Self = serde_json::from_str(&text)
            .map_err(|e| MnemeError::Crypto(format!("keystore parse: {e}")))?;
        parsed.verify_known_version()?;
        Ok(Some(parsed))
    }

    /// Atomically persist the keystore at `data_dir/keystore.json`.
    /// Uses temp + rename + parent-dir fsync; sets mode 0o600 on
    /// unix before the rename so the file is never world-readable.
    pub fn write(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = keystore_path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| MnemeError::Crypto(format!("keystore serialise: {e}")))?;
        std::fs::write(&tmp, &body)?;
        set_keystore_perms(&tmp)?;
        std::fs::rename(&tmp, &path)?;
        // fsync the parent dir so the rename is crash-durable; matches
        // the daemon auth-token write path.
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
            && let Err(e) = dir.sync_all()
        {
            tracing::warn!(
                error = %e,
                parent = %parent.display(),
                "keystore parent dir fsync failed",
            );
        }
        Ok(())
    }

    fn verify_known_version(&self) -> Result<()> {
        if self.version != KEYSTORE_VERSION {
            return Err(MnemeError::Crypto(format!(
                "unsupported keystore version: {} (expected {})",
                self.version, KEYSTORE_VERSION
            )));
        }
        Ok(())
    }
}

/// Path the keystore lives at under `data_dir`.
pub fn keystore_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEYSTORE_FILENAME)
}

#[cfg(unix)]
fn set_keystore_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(KEYSTORE_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_keystore_perms(_path: &Path) -> Result<()> {
    // Windows: keystore.json inherits the user-profile ACL of the
    // data directory (already user-owned + non-world-readable on a
    // default install). Matches the auth.token handling.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_keys() -> (Dek, Kek) {
        (Dek::generate().unwrap(), Kek::generate().unwrap())
    }

    #[test]
    fn create_then_unwrap_round_trips() {
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let original_bytes = *dek.as_bytes();

        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        let unwrapped = keystore.unwrap_dek(&kek).unwrap();
        assert_eq!(unwrapped.as_bytes(), &original_bytes);
    }

    #[test]
    fn unwrap_with_wrong_kek_fails() {
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let other_kek = Kek::generate().unwrap();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        assert!(keystore.unwrap_dek(&other_kek).is_err());
    }

    #[test]
    fn write_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        keystore.write(tmp.path()).unwrap();

        let loaded = Keystore::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.version, KEYSTORE_VERSION);
        assert_eq!(loaded.aead, "xchacha20poly1305");
        assert_eq!(loaded.kdf.algo, "bip39-pbkdf2-hmac-sha512");
        assert_eq!(loaded.kdf.iterations, 2048);
        assert_eq!(loaded.keyring.service, "mneme");
        assert_eq!(loaded.keyring.account.len(), 32);
        assert!(loaded.mnemonic_verified);
        assert!(loaded.rotated_at.is_none());

        // Still round-trips the DEK after disk round-trip.
        let unwrapped = loaded.unwrap_dek(&kek).unwrap();
        assert_eq!(unwrapped.as_bytes(), dek.as_bytes());
    }

    #[test]
    fn load_returns_none_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        assert!(Keystore::load(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn load_rejects_unknown_version() {
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let mut keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        keystore.version = 99;
        keystore.write(tmp.path()).unwrap();
        let err = Keystore::load(tmp.path());
        assert!(err.is_err());
    }

    #[test]
    fn rewrap_preserves_dek_but_swaps_outer_key() {
        let tmp = TempDir::new().unwrap();
        let (dek, kek_a) = fixture_keys();
        let kek_b = Kek::generate().unwrap();
        let dek_bytes = *dek.as_bytes();

        let mut keystore = Keystore::create(tmp.path(), &dek, &kek_a, true).unwrap();
        let wrapped_before = keystore.wrapped_dek.clone();

        keystore.rewrap(&dek, &kek_b).unwrap();
        let wrapped_after = keystore.wrapped_dek.clone();

        // Wrapped blob must differ (new KEK, new nonce).
        assert_ne!(wrapped_before, wrapped_after);
        // rotated_at must be set now.
        assert!(keystore.rotated_at.is_some());
        // Old KEK must no longer open the wrap.
        assert!(keystore.unwrap_dek(&kek_a).is_err());
        // New KEK opens to the same DEK bytes.
        let unwrapped = keystore.unwrap_dek(&kek_b).unwrap();
        assert_eq!(unwrapped.as_bytes(), &dek_bytes);
    }

    #[test]
    fn write_creates_data_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested").join("dir");
        let (dek, kek) = fixture_keys();
        let keystore = Keystore::create(&nested, &dek, &kek, true).unwrap();
        keystore.write(&nested).unwrap();
        assert!(nested.join(KEYSTORE_FILENAME).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn write_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        keystore.write(tmp.path()).unwrap();
        let meta = std::fs::metadata(tmp.path().join(KEYSTORE_FILENAME)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, KEYSTORE_MODE);
    }

    #[test]
    fn write_is_atomic_via_temp_rename() {
        // After a successful write, no `.tmp` artifact should be left
        // behind in the data dir.
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        keystore.write(tmp.path()).unwrap();
        let tmp_path = tmp.path().join("keystore.json.tmp");
        assert!(!tmp_path.exists(), "leftover .tmp file");
    }

    #[test]
    fn json_field_names_are_stable() {
        // Lock the wire shape: changing any of these names is a
        // breaking change for existing data dirs.
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        let json = serde_json::to_string(&keystore).unwrap();
        for field in [
            "\"version\"",
            "\"kdf\"",
            "\"algo\"",
            "\"iterations\"",
            "\"aead\"",
            "\"wrapped_dek\"",
            "\"keyring\"",
            "\"service\"",
            "\"account\"",
            "\"created_at\"",
            "\"rotated_at\"",
            "\"mnemonic_verified\"",
        ] {
            assert!(json.contains(field), "missing field: {field}");
        }
    }

    #[test]
    fn keystore_path_lives_under_data_dir() {
        let tmp = TempDir::new().unwrap();
        let p = keystore_path(tmp.path());
        assert_eq!(p, tmp.path().join(KEYSTORE_FILENAME));
    }

    #[test]
    fn debug_does_not_leak_wrapped_dek_into_keys() {
        // Sanity check that the Keystore struct's Debug impl includes
        // wrapped_dek (which is fine — it's already wrapped) but does
        // not somehow expose plaintext.
        let tmp = TempDir::new().unwrap();
        let (dek, kek) = fixture_keys();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        let dbg = format!("{keystore:?}");
        // The plaintext DEK should not appear anywhere in the
        // serialised debug output.
        let dek_first = format!("{:02x}", dek.as_bytes()[0]);
        // Single byte is too short to be diagnostic on its own; use
        // the first 8 bytes hex-encoded.
        let dek_prefix: String = dek
            .as_bytes()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let _ = dek_first;
        assert!(
            !dbg.contains(&dek_prefix),
            "Debug leaked plaintext DEK bytes"
        );
    }
}
