//! End-to-end backup/restore cycle for encrypted data directories (P8).
//!
//! Walks the path a user takes after losing a laptop: encrypted dir on
//! machine A, `mneme backup` produces a tarball, the tarball is copied
//! to machine B (a fresh data dir with no keyring entry), `mneme
//! recover --mnemonic ...` restashes the KEK, and the daemon boots
//! and serves the same data.

use mneme::cli::backup::backup_at;
use mneme::cli::encrypt::{decrypt_at, encrypt_at, recover_at};
use mneme::cli::restore::restore_at;
use mneme::crypto::boot::open_episodic_storage;
use mneme::crypto::keyring::InMemoryKekStore;
use mneme::crypto::{KekStore, Keystore, Mnemonic, account_for, keystore_path};
use tempfile::TempDir;

/// Scripted prompt that always answers correctly — never used in this
/// test (we go through `--no-verify`), but the trait impl is required
/// for `encrypt_at`'s signature.
struct AnyPrompt;
impl mneme::cli::encrypt::MnemonicPrompt for AnyPrompt {
    fn confirm_written(&mut self) -> mneme::Result<()> {
        Ok(())
    }
    fn answer_position(&mut self, _: usize) -> mneme::Result<String> {
        Ok(String::new())
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn encrypted_backup_then_restore_then_recover_round_trips_data() {
    // ----- Machine A: encrypted store with some data -----
    let src = TempDir::new().unwrap();
    let src_keyring = InMemoryKekStore::new();
    encrypt_at(
        src.path(),
        false,
        /* no_verify */ true,
        &src_keyring,
        &mut AnyPrompt,
    )
    .unwrap();

    // Sanity: keystore + keyring entry present.
    assert!(keystore_path(src.path()).is_file());
    assert!(
        src_keyring
            .load(&account_for(src.path()))
            .unwrap()
            .is_some()
    );

    // Capture the mnemonic. We derive it from the just-created keystore
    // + keyring by reading the wrapped DEK back out — but we generated
    // the mnemonic inside `encrypt_at`, which doesn't surface it.
    // Workaround for the test: re-implement the encrypt flow manually
    // so we own the mnemonic ourselves.
    //
    // Reset and redo with explicit mnemonic.
    decrypt_at(src.path(), true, &src_keyring).unwrap();

    let mnemonic = Mnemonic::generate().unwrap();
    let kek = mnemonic.derive_kek("");
    let dek = mneme::crypto::Dek::generate().unwrap();
    let dek_marker = *dek.as_bytes();
    let ks = Keystore::create(src.path(), &dek, &kek, true).unwrap();
    ks.write(src.path()).unwrap();
    src_keyring.store(&account_for(src.path()), &kek).unwrap();

    // Write some data through the encrypted storage stack.
    rt().block_on(async {
        let s = open_episodic_storage(src.path(), &src_keyring).unwrap();
        s.put(b"mem:01HKKA0000", b"alpha-marker").await.unwrap();
        s.put(b"mem:01HKKB0000", b"bravo-marker").await.unwrap();
        s.put(b"epi:01HKKC0000", b"charlie-marker").await.unwrap();
        s.flush().await.unwrap();
    });

    // ----- Tarball -----
    let archive = TempDir::new().unwrap();
    let archive_path = archive.path().join("mneme-backup.tar.gz");
    backup_at(src.path(), &archive_path, /* include_models */ false).unwrap();
    assert!(archive_path.is_file());

    // ----- Machine B: fresh root + empty keyring -----
    let dst = TempDir::new().unwrap();
    let dst_root = dst.path().join("home").join(".mneme");
    std::fs::create_dir_all(&dst_root).unwrap();
    restore_at(&archive_path, &dst_root, /* force */ false).unwrap();
    assert!(keystore_path(&dst_root).is_file(), "keystore copied");

    // Fresh keyring on machine B — no KEK entry yet.
    let dst_keyring = InMemoryKekStore::new();

    // Booting without a KEK must refuse cleanly.
    let err = open_episodic_storage(&dst_root, &dst_keyring);
    assert!(err.is_err(), "encrypted dir + empty keyring must refuse");

    // ----- Recovery via the mnemonic -----
    recover_at(&dst_root, &mnemonic.to_phrase(), &dst_keyring).unwrap();
    assert!(
        dst_keyring.load(&account_for(&dst_root)).unwrap().is_none(),
        "recover_at stores under the keystore's account, not the destination's",
    );
    // The recovery path stashes under keystore.keyring.account, which
    // is the SOURCE machine's hash (the tarball carries that field
    // verbatim). The destination has to load via the same account, so
    // this is the right behaviour. Confirm by reading the keystore.
    let keystore = Keystore::load(&dst_root).unwrap().unwrap();
    assert!(
        dst_keyring
            .load(&keystore.keyring.account)
            .unwrap()
            .is_some(),
        "KEK was restashed under the keystore's account",
    );

    // Daemon boot succeeds and reads back the same data.
    rt().block_on(async {
        let s = open_episodic_storage(&dst_root, &dst_keyring).unwrap();
        assert_eq!(
            s.get(b"mem:01HKKA0000").await.unwrap().as_deref(),
            Some(b"alpha-marker".as_ref())
        );
        assert_eq!(
            s.get(b"mem:01HKKB0000").await.unwrap().as_deref(),
            Some(b"bravo-marker".as_ref())
        );
        assert_eq!(
            s.get(b"epi:01HKKC0000").await.unwrap().as_deref(),
            Some(b"charlie-marker".as_ref())
        );
    });

    // Sanity: the DEK byte pattern survived the round-trip end-to-end.
    let dek = keystore.unwrap_dek(&kek).unwrap();
    assert_eq!(dek.as_bytes(), &dek_marker);
}

#[test]
fn restore_into_keystore_dir_requires_force_for_existing_data() {
    let src = TempDir::new().unwrap();
    let src_keyring = InMemoryKekStore::new();
    encrypt_at(src.path(), false, true, &src_keyring, &mut AnyPrompt).unwrap();
    let archive = TempDir::new().unwrap();
    let path = archive.path().join("b.tar.gz");
    backup_at(src.path(), &path, false).unwrap();

    let dst = TempDir::new().unwrap();
    // Pre-populate the destination with a stray file so it's non-empty.
    std::fs::write(dst.path().join("stray.txt"), b"hi").unwrap();
    let err = restore_at(&path, dst.path(), /* force */ false);
    assert!(err.is_err(), "non-empty dst without --force must refuse");

    // With --force the restore proceeds.
    restore_at(&path, dst.path(), true).unwrap();
}
