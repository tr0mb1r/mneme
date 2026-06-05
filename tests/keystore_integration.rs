//! End-to-end test of the encryption-at-rest key lifecycle (P2).
//!
//! Walks the same boot path the daemon will execute (P7), but against
//! the in-memory `KekStore` so CI doesn't need a real OS keyring.
//!
//! Scenarios:
//!
//! 1. **`mneme encrypt` happy path** — generate mnemonic, derive KEK,
//!    generate DEK, create + write keystore, store KEK in keyring;
//!    then a "later daemon boot": read keystore from disk, load KEK
//!    from keyring, unwrap DEK, confirm it matches the original.
//! 2. **Recovery from cleared keyring** — wipe the keyring entry,
//!    re-derive the KEK from the same mnemonic, unwrap the DEK,
//!    confirm it matches.
//! 3. **`mneme rekey`** — generate a new mnemonic, re-wrap the DEK,
//!    confirm the old mnemonic no longer opens it and the new one does;
//!    the unwrapped DEK is byte-identical to the original.
//! 4. **Cross-machine restore** — simulate copying the data dir to a
//!    fresh machine (empty keyring) and recovering via the mnemonic.

use mneme::crypto::keyring::InMemoryKekStore;
use mneme::crypto::{Dek, KekStore, Keystore, Mnemonic, account_for};
use tempfile::TempDir;

fn mneme_encrypt_flow(data_dir: &std::path::Path, keyring: &dyn KekStore) -> (Mnemonic, [u8; 32]) {
    // 1. Generate fresh key material.
    let mnemonic = Mnemonic::generate().unwrap();
    let kek = mnemonic.derive_kek("");
    let dek = Dek::generate().unwrap();
    let dek_bytes = *dek.as_bytes();

    // 2. Persist the wrapped DEK to disk and stash the KEK in the keyring.
    let keystore = Keystore::create(data_dir, &dek, &kek, /* mnemonic_verified */ true).unwrap();
    keystore.write(data_dir).unwrap();
    keyring.store(&account_for(data_dir), &kek).unwrap();

    (mnemonic, dek_bytes)
}

fn daemon_boot(data_dir: &std::path::Path, keyring: &dyn KekStore) -> [u8; 32] {
    let keystore = Keystore::load(data_dir).unwrap().expect("keystore present");
    let kek = keyring
        .load(&keystore.keyring.account)
        .unwrap()
        .expect("KEK in keyring");
    let dek = keystore.unwrap_dek(&kek).unwrap();
    *dek.as_bytes()
}

#[test]
fn happy_path_encrypt_then_boot() {
    let tmp = TempDir::new().unwrap();
    let keyring = InMemoryKekStore::new();

    let (_mnemonic, original_dek) = mneme_encrypt_flow(tmp.path(), &keyring);
    let booted_dek = daemon_boot(tmp.path(), &keyring);

    assert_eq!(booted_dek, original_dek);
}

#[test]
fn recovery_from_cleared_keyring() {
    let tmp = TempDir::new().unwrap();
    let keyring = InMemoryKekStore::new();

    let (mnemonic, original_dek) = mneme_encrypt_flow(tmp.path(), &keyring);

    // Simulate keyring wipe (re-installed OS, cleared Keychain, etc.).
    let account = account_for(tmp.path());
    keyring.delete(&account).unwrap();
    assert!(keyring.load(&account).unwrap().is_none());

    // Daemon boot would now fail at the keyring lookup. The recovery
    // path: user types the mnemonic into `mneme recover`, we re-derive
    // the KEK, restash it, and the boot resumes.
    let kek = mnemonic.derive_kek("");
    keyring.store(&account, &kek).unwrap();

    let booted_dek = daemon_boot(tmp.path(), &keyring);
    assert_eq!(booted_dek, original_dek);
}

#[test]
fn rekey_keeps_dek_but_invalidates_old_mnemonic() {
    let tmp = TempDir::new().unwrap();
    let keyring = InMemoryKekStore::new();

    let (old_mnemonic, original_dek) = mneme_encrypt_flow(tmp.path(), &keyring);

    // Reload to mutate.
    let mut keystore = Keystore::load(tmp.path()).unwrap().unwrap();
    let dek = keystore.unwrap_dek(&old_mnemonic.derive_kek("")).unwrap();

    // Generate fresh mnemonic + KEK, rewrap.
    let new_mnemonic = Mnemonic::generate().unwrap();
    let new_kek = new_mnemonic.derive_kek("");
    keystore.rewrap(&dek, &new_kek).unwrap();
    keystore.write(tmp.path()).unwrap();
    keyring.store(&account_for(tmp.path()), &new_kek).unwrap();

    // Old mnemonic must not open.
    let reloaded = Keystore::load(tmp.path()).unwrap().unwrap();
    let old_kek = old_mnemonic.derive_kek("");
    assert!(reloaded.unwrap_dek(&old_kek).is_err());

    // New mnemonic opens to the same DEK bytes (no data needed rewriting).
    let reopened = reloaded.unwrap_dek(&new_kek).unwrap();
    assert_eq!(reopened.as_bytes(), &original_dek);

    // rotated_at must now be set.
    assert!(reloaded.rotated_at.is_some());
}

#[test]
fn cross_machine_restore() {
    // Source machine: encrypt + populate keyring.
    let src = TempDir::new().unwrap();
    let src_keyring = InMemoryKekStore::new();
    let (mnemonic, original_dek) = mneme_encrypt_flow(src.path(), &src_keyring);

    // "Copy" the data dir to a destination machine. We mimic this by
    // reading keystore.json verbatim and writing it under a new
    // root. The mnemonic *does not travel with the tarball* (per
    // ADR-0013 §"Consequences") — the user types it on the new box.
    let dst = TempDir::new().unwrap();
    let keystore_bytes = std::fs::read(src.path().join("keystore.json")).unwrap();
    std::fs::write(dst.path().join("keystore.json"), &keystore_bytes).unwrap();

    // The destination keyring starts empty.
    let dst_keyring = InMemoryKekStore::new();
    let account = account_for(dst.path());
    assert!(dst_keyring.load(&account).unwrap().is_none());

    // User runs `mneme recover --mnemonic "..."` — we derive the KEK
    // and stash it locally.
    let kek = mnemonic.derive_kek("");
    dst_keyring.store(&account, &kek).unwrap();

    // NOTE: the keystore.json carries the source machine's account
    // hash, not the destination's. Daemon boot uses the account
    // recorded in keystore.json, not the freshly-computed one — so the
    // KEK must be stored under the keystore's account id.
    let keystore = Keystore::load(dst.path()).unwrap().unwrap();
    dst_keyring.store(&keystore.keyring.account, &kek).unwrap();

    let booted_dek = daemon_boot(dst.path(), &dst_keyring);
    assert_eq!(booted_dek, original_dek);
}

#[test]
fn missing_keystore_means_plaintext_mode() {
    let tmp = TempDir::new().unwrap();
    let loaded = Keystore::load(tmp.path()).unwrap();
    assert!(loaded.is_none(), "absent keystore must be Ok(None)");
}

#[test]
fn parsing_corrupted_keystore_fails_cleanly() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("keystore.json"), b"{this is not json").unwrap();
    let err = Keystore::load(tmp.path());
    assert!(err.is_err(), "garbled keystore.json must error, not panic");
}
