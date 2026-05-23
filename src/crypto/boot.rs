//! Daemon boot helpers for opting into encrypted storage (ADR-0013 P7).
//!
//! Exactly one decision point: does this data dir have a
//! `keystore.json`? If yes, we must have the KEK (from the OS keyring
//! or the `MNEME_RECOVERY_PHRASE` env fallback) before opening any
//! data file. If no, we boot plaintext as in v1.0/v1.1.
//!
//! This helper deliberately knows nothing about the rest of the boot
//! sequence (embedder, schedulers, MCP server). Its single output is
//! an `Arc<dyn Storage>` that the rest of `src/cli/run.rs` consumes
//! identically whether encryption is on or off — which is how we
//! preserve ADR-0012's multi-agent guarantee that encryption changes
//! no wire byte.

use crate::crypto::{KekStore, Keystore, Mnemonic};
use crate::storage::{EncryptedStorage, Storage, redb_impl::RedbStorage};
use crate::{MnemeError, Result};
use std::path::Path;
use std::sync::Arc;

/// Environment variable read when the OS keyring is unreachable
/// (headless servers, CI, fresh machine before `mneme recover`).
/// Value: the 12-word BIP39 phrase, space-separated.
pub const RECOVERY_PHRASE_ENV: &str = "MNEME_RECOVERY_PHRASE";

/// Open the episodic storage tree at `<root>/episodic`, picking the
/// plaintext or encrypted backend based on whether `<root>/keystore.json`
/// is present.
///
/// Returns `Err(MnemeError::Crypto)` if the keystore exists but the
/// KEK is unavailable from both the keyring and the env fallback. The
/// daemon refuses to bind its socket in that case (P7 gate).
pub fn open_episodic_storage(root: &Path, keyring: &dyn KekStore) -> Result<Arc<dyn Storage>> {
    let episodic = root.join("episodic");
    match Keystore::load(root)? {
        None => {
            // Plaintext mode — legacy v1.0/v1.1 path.
            let s = RedbStorage::open(&episodic)?;
            Ok(s as Arc<dyn Storage>)
        }
        Some(keystore) => {
            let kek = load_kek(&keystore, keyring)?;
            let dek = keystore.unwrap_dek(&kek)?;
            let s = EncryptedStorage::open_redb(&episodic, &dek)?;
            Ok(s as Arc<dyn Storage>)
        }
    }
}

/// Load the KEK from the configured custody backends, in order:
///
/// 1. OS keyring under the account id recorded in the keystore.
/// 2. `MNEME_RECOVERY_PHRASE` env var — for headless boxes where the
///    keyring backend is unreachable. Best practice is to load the
///    env from a secrets manager (systemd LoadCredential, AWS SSM,
///    Vault), not a shell rcfile.
///
/// Returns a structured error if neither source has the key,
/// pointing at `mneme recover --mnemonic ...` as the next step.
fn load_kek(keystore: &Keystore, keyring: &dyn KekStore) -> Result<crate::crypto::Kek> {
    if let Some(k) = keyring.load(&keystore.keyring.account)? {
        return Ok(k);
    }
    if let Ok(phrase) = std::env::var(RECOVERY_PHRASE_ENV)
        && !phrase.is_empty()
    {
        let mnemonic = Mnemonic::parse(phrase.trim())?;
        let derived = mnemonic.derive_kek("");
        // Verify against the keystore before returning — guards
        // against booting under a wrong phrase that "happens" to
        // derive to a KEK we can't unwrap with.
        keystore.unwrap_dek(&derived).map_err(|e| {
            MnemeError::Crypto(format!(
                "{RECOVERY_PHRASE_ENV} does not match this keystore: {e}",
            ))
        })?;
        return Ok(derived);
    }
    Err(MnemeError::Crypto(format!(
        "encrypted store at {} but no KEK available: keyring entry missing AND \
         {RECOVERY_PHRASE_ENV} not set. Run `mneme recover --mnemonic \"<12 words>\"` \
         to restash the KEK, or export {RECOVERY_PHRASE_ENV}=... for headless \
         boots from a secrets manager.",
        crate::crypto::keystore_path(std::path::Path::new(".")).display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keyring::InMemoryKekStore;
    use crate::crypto::{Dek, account_for};
    use tempfile::TempDir;

    fn set_up_encrypted_dir(tmp: &TempDir, keyring: &InMemoryKekStore) -> Mnemonic {
        let mnemonic = Mnemonic::generate().unwrap();
        let kek = mnemonic.derive_kek("");
        let dek = Dek::generate().unwrap();
        let keystore = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        keystore.write(tmp.path()).unwrap();
        keyring.store(&account_for(tmp.path()), &kek).unwrap();
        // also create episodic dir scaffold
        std::fs::create_dir_all(tmp.path().join("episodic")).unwrap();
        mnemonic
    }

    #[test]
    fn plaintext_dir_opens_plaintext_storage() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("episodic")).unwrap();
        let keyring = InMemoryKekStore::new();
        let s = open_episodic_storage(tmp.path(), &keyring).unwrap();
        // Smoke: a put + get round-trips.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            s.put(b"k", b"v").await.unwrap();
            assert_eq!(s.get(b"k").await.unwrap(), Some(b"v".to_vec()));
        });
    }

    #[test]
    fn encrypted_dir_with_keyring_kek_opens_encrypted_storage() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        let _m = set_up_encrypted_dir(&tmp, &keyring);
        let s = open_episodic_storage(tmp.path(), &keyring).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            s.put(b"k", b"plaintext-marker").await.unwrap();
            // The wrapper layer decrypts on read; the on-disk redb file
            // does NOT contain "plaintext-marker" (tested separately
            // in tests/encrypted_storage_e2e.rs).
            assert_eq!(
                s.get(b"k").await.unwrap(),
                Some(b"plaintext-marker".to_vec())
            );
        });
    }

    #[test]
    fn encrypted_dir_with_no_kek_fails_clean() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        let m = set_up_encrypted_dir(&tmp, &keyring);
        // Wipe the keyring entry and don't set the env var.
        keyring.delete(&account_for(tmp.path())).unwrap();
        // SAFETY: tests run single-threaded for env-touching paths;
        // if needed we'd use std::env::vars-snapshot pattern, but
        // here we explicitly unset to be sure.
        // SAFETY: env mutation in tests; we read-only restore below.
        unsafe { std::env::remove_var(RECOVERY_PHRASE_ENV) };

        let err = open_episodic_storage(tmp.path(), &keyring);
        assert!(err.is_err(), "encrypted dir + no KEK must refuse");

        // Sanity check that the test fixture is internally consistent:
        // setting the env should let us recover.
        // SAFETY: env mutation in tests
        unsafe { std::env::set_var(RECOVERY_PHRASE_ENV, m.to_phrase()) };
        let s = open_episodic_storage(tmp.path(), &keyring);
        assert!(s.is_ok(), "env fallback must succeed");
        // SAFETY: env mutation in tests
        unsafe { std::env::remove_var(RECOVERY_PHRASE_ENV) };
    }

    #[test]
    fn env_fallback_with_wrong_mnemonic_fails() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        let _m = set_up_encrypted_dir(&tmp, &keyring);
        keyring.delete(&account_for(tmp.path())).unwrap();
        // SAFETY: env mutation in tests
        unsafe {
            std::env::set_var(
                RECOVERY_PHRASE_ENV,
                Mnemonic::generate().unwrap().to_phrase(),
            )
        };
        let err = open_episodic_storage(tmp.path(), &keyring);
        assert!(err.is_err());
        // SAFETY: env mutation in tests
        unsafe { std::env::remove_var(RECOVERY_PHRASE_ENV) };
    }
}
