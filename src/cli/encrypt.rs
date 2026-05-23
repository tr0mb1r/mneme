//! `mneme encrypt` / `mneme recover` / `mneme rekey` / `mneme decrypt`
//! — encryption-at-rest CLI (ADR-0013 D10, P6).
//!
//! All four subcommands refuse to run while the daemon's lockfile is
//! held. They operate on the keystore + keyring layer only — full
//! data-dir migration (P5a–P5d wire-up of HNSW, procedural, sessions,
//! and cold archive) is a follow-up. For now `mneme encrypt` is the
//! one-time init on a fresh or already-encrypted data dir; data
//! written *after* the keystore lands goes through the encrypted
//! Storage stack (P3 + P4) and is opaque on disk.

use crate::crypto::{
    Dek, KekStore, Keystore, Mnemonic, OsKeyring, VerifyChallenge, account_for, keystore_path,
};
use crate::{MnemeError, Result};
use std::io::{BufRead, Write};
use std::path::Path;

// ---------- public CLI entry points ----------

/// `mneme encrypt` — generate keys + recovery phrase, store keystore + keyring entry.
pub fn run_encrypt(force_reinit: bool, no_verify: bool) -> Result<()> {
    encrypt_at(
        &data_root()?,
        force_reinit,
        no_verify,
        &OsKeyring::new(),
        &mut StdinPrompt,
    )
}

/// `mneme recover --mnemonic "<12 words>"` — re-derive KEK, verify it
/// unwraps the keystore's DEK, restash in keyring.
pub fn run_recover(mnemonic_phrase: &str) -> Result<()> {
    recover_at(&data_root()?, mnemonic_phrase, &OsKeyring::new())
}

/// `mneme rekey` — new KEK + new mnemonic, re-wraps the unchanged DEK.
/// O(1): no on-disk data has to be rewritten.
pub fn run_rekey(no_verify: bool) -> Result<()> {
    rekey_at(
        &data_root()?,
        no_verify,
        &OsKeyring::new(),
        &mut StdinPrompt,
    )
}

/// `mneme decrypt --yes-i-really-mean-it` — clears the keystore + keyring.
pub fn run_decrypt(force: bool) -> Result<()> {
    decrypt_at(&data_root()?, force, &OsKeyring::new())
}

// ---------- testable cores ----------

/// Trait for the interactive prompt loop. Tests inject scripted answers.
pub trait MnemonicPrompt {
    /// Wait for the user to acknowledge they wrote the phrase down.
    fn confirm_written(&mut self) -> Result<()>;
    /// Ask the user for the word at `position` (1-indexed). Returns
    /// the typed string (whitespace will be trimmed by the caller).
    fn answer_position(&mut self, position: usize) -> Result<String>;
}

pub struct StdinPrompt;

impl MnemonicPrompt for StdinPrompt {
    fn confirm_written(&mut self) -> Result<()> {
        eprintln!("Press ENTER when you have written down all 12 words.");
        let _ = read_line()?;
        Ok(())
    }
    fn answer_position(&mut self, position: usize) -> Result<String> {
        eprint!("  Word #{position:>2}: ");
        std::io::stderr().flush().ok();
        Ok(read_line()?.trim().to_string())
    }
}

pub fn encrypt_at(
    root: &Path,
    force_reinit: bool,
    no_verify: bool,
    keyring: &dyn KekStore,
    prompt: &mut dyn MnemonicPrompt,
) -> Result<()> {
    refuse_if_locked(root)?;
    let ks_path = keystore_path(root);
    if ks_path.exists() && !force_reinit {
        return Err(MnemeError::Config(format!(
            "keystore already exists at {} (pass --force-reinit to replace it; \
             you will lose access to any existing encrypted data unless you keep \
             the current recovery mnemonic)",
            ks_path.display()
        )));
    }

    let mnemonic = Mnemonic::generate()?;
    let kek = mnemonic.derive_kek("");
    let dek = Dek::generate()?;
    display_mnemonic(&mnemonic);

    let mnemonic_verified = if no_verify {
        eprintln!(
            "--no-verify: skipping mnemonic verification. Keystore will record \
             mnemonic_verified=false."
        );
        false
    } else {
        verify_mnemonic_loop(&mnemonic, prompt)?
    };

    let keystore = Keystore::create(root, &dek, &kek, mnemonic_verified)?;
    keystore.write(root)?;
    let account = account_for(root);
    keyring.store(&account, &kek).map_err(|e| {
        MnemeError::Config(format!(
            "could not write KEK to OS keyring: {e}. If you are on a headless box \
             with no keyring backend, you can still recover via \
             `mneme recover --mnemonic ...` later — the keystore is written.",
        ))
    })?;

    eprintln!();
    eprintln!("Encryption enabled. Keystore: {}", ks_path.display());
    eprintln!(
        "Recovery phrase {}. Run `mneme run` or `mneme daemon` to start the server.",
        if mnemonic_verified {
            "verified"
        } else {
            "NOT verified (--no-verify)"
        }
    );

    // `keystore` and `mnemonic` go out of scope here. The mnemonic
    // contains zeroize-on-drop fields; the keystore does not carry
    // plaintext key material.
    Ok(())
}

pub fn recover_at(root: &Path, mnemonic_phrase: &str, keyring: &dyn KekStore) -> Result<()> {
    refuse_if_locked(root)?;
    let keystore = Keystore::load(root)?.ok_or_else(|| {
        MnemeError::Config(format!(
            "no keystore at {} — nothing to recover. Run `mneme encrypt` first.",
            keystore_path(root).display()
        ))
    })?;
    let mnemonic = Mnemonic::parse(mnemonic_phrase.trim())?;
    let kek = mnemonic.derive_kek("");
    let _verified_dek = keystore.unwrap_dek(&kek).map_err(|e| {
        MnemeError::Crypto(format!(
            "recovery phrase does not match this keystore: {e}. Check the words and re-run."
        ))
    })?;
    keyring.store(&keystore.keyring.account, &kek)?;
    eprintln!("Recovery phrase verified and stored in OS keyring.");
    eprintln!("Run `mneme run` or `mneme daemon` to resume.");
    Ok(())
}

pub fn rekey_at(
    root: &Path,
    no_verify: bool,
    keyring: &dyn KekStore,
    prompt: &mut dyn MnemonicPrompt,
) -> Result<()> {
    refuse_if_locked(root)?;
    let mut keystore = Keystore::load(root)?.ok_or_else(|| {
        MnemeError::Config("no keystore to rekey — run `mneme encrypt` first".into())
    })?;
    let current_kek = keyring.load(&keystore.keyring.account)?.ok_or_else(|| {
        MnemeError::Crypto(
            "current KEK not found in keyring; run `mneme recover --mnemonic ...` first to seed it"
                .into(),
        )
    })?;
    let dek = keystore.unwrap_dek(&current_kek)?;

    eprintln!("Generating new recovery phrase. The OLD phrase will stop working.");
    eprintln!();
    let new_mnemonic = Mnemonic::generate()?;
    let new_kek = new_mnemonic.derive_kek("");
    display_mnemonic(&new_mnemonic);
    let verified = if no_verify {
        eprintln!("--no-verify: skipping verification.");
        false
    } else {
        verify_mnemonic_loop(&new_mnemonic, prompt)?
    };
    keystore.rewrap(&dek, &new_kek)?;
    keystore.mnemonic_verified = verified;
    keystore.write(root)?;
    keyring.store(&keystore.keyring.account, &new_kek)?;
    eprintln!();
    eprintln!(
        "Rekey complete. New phrase {}.",
        if verified { "verified" } else { "NOT verified" }
    );
    eprintln!("The previous recovery phrase will no longer unwrap this keystore.");
    Ok(())
}

pub fn decrypt_at(root: &Path, force: bool, keyring: &dyn KekStore) -> Result<()> {
    refuse_if_locked(root)?;
    if !force {
        return Err(MnemeError::Config(
            "refusing without --yes-i-really-mean-it. Removing the keystore makes \
             every record encrypted under it unrecoverable. If you want to roll \
             back encryption you should `mneme backup` first."
                .into(),
        ));
    }
    let keystore = match Keystore::load(root)? {
        Some(k) => k,
        None => {
            eprintln!(
                "No keystore at {}. Nothing to do.",
                keystore_path(root).display()
            );
            return Ok(());
        }
    };
    keyring.delete(&keystore.keyring.account)?;
    let p = keystore_path(root);
    std::fs::remove_file(&p)?;
    eprintln!("Removed {} and cleared keyring entry.", p.display());
    eprintln!(
        "WARNING: any record written while encryption was on remains opaque on \
         disk. Restore from a plaintext backup if you need to read them."
    );
    Ok(())
}

// ---------- helpers ----------

fn data_root() -> Result<std::path::PathBuf> {
    crate::storage::layout::default_root()
        .ok_or_else(|| MnemeError::Config("could not resolve ~/.mneme".into()))
}

fn refuse_if_locked(root: &Path) -> Result<()> {
    let lock_path = root.join(".lock");
    if lock_path.exists() {
        return Err(MnemeError::Lock(format!(
            "{} is held — stop the running mneme instance before encrypt/rekey/recover/decrypt",
            lock_path.display()
        )));
    }
    Ok(())
}

fn display_mnemonic(mnemonic: &Mnemonic) {
    eprintln!("════════════════ RECOVERY PHRASE — WRITE THIS DOWN ════════════════");
    let words = mnemonic.words();
    for (i, word) in words.iter().enumerate() {
        eprint!(" {:>2}. {:<10}", i + 1, word);
        if (i + 1) % 4 == 0 {
            eprintln!();
        }
    }
    eprintln!("══════════════════════════════════════════════════════════════════");
    eprintln!();
}

fn verify_mnemonic_loop(mnemonic: &Mnemonic, prompt: &mut dyn MnemonicPrompt) -> Result<bool> {
    prompt.confirm_written()?;
    for attempt in 1..=3u32 {
        let challenge = VerifyChallenge::random(mnemonic)?;
        eprintln!("Verification (attempt {attempt} of 3):");
        let mut answers: [String; 3] = Default::default();
        for (slot, &pos) in answers.iter_mut().zip(challenge.positions()) {
            *slot = prompt.answer_position(pos)?;
        }
        let answer_refs: [&str; 3] = [
            answers[0].as_str(),
            answers[1].as_str(),
            answers[2].as_str(),
        ];
        if challenge.verify(&answer_refs) {
            eprintln!("Phrase verified.");
            return Ok(true);
        }
        eprintln!("One or more words did not match. Try again.");
    }
    Err(MnemeError::Config(
        "mnemonic verification failed after 3 attempts; re-run `mneme encrypt` when ready".into(),
    ))
}

fn read_line() -> Result<String> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut buf = String::new();
    handle.read_line(&mut buf).map_err(MnemeError::Io)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keyring::InMemoryKekStore;
    use tempfile::TempDir;

    /// Scripted prompt that knows the truth and types the right words.
    struct GoodPrompt {
        words: Vec<String>,
    }
    impl GoodPrompt {
        fn new(m: &Mnemonic) -> Self {
            Self {
                words: m.words().iter().map(|s| (*s).to_string()).collect(),
            }
        }
    }
    impl MnemonicPrompt for GoodPrompt {
        fn confirm_written(&mut self) -> Result<()> {
            Ok(())
        }
        fn answer_position(&mut self, p: usize) -> Result<String> {
            Ok(self.words[p - 1].clone())
        }
    }

    /// Prompt that always types the wrong word — verification must fail.
    struct BadPrompt;
    impl MnemonicPrompt for BadPrompt {
        fn confirm_written(&mut self) -> Result<()> {
            Ok(())
        }
        fn answer_position(&mut self, _: usize) -> Result<String> {
            Ok("wrong".to_string())
        }
    }

    #[test]
    fn encrypt_at_creates_keystore_and_stashes_kek() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        encrypt_at(
            tmp.path(),
            false,
            /* no_verify */ true,
            &keyring,
            &mut BadPrompt,
        )
        .unwrap();
        // keystore.json exists
        assert!(keystore_path(tmp.path()).is_file());
        // KEK present in keyring under the account id
        let account = account_for(tmp.path());
        assert!(keyring.load(&account).unwrap().is_some());
    }

    #[test]
    fn encrypt_at_refuses_if_keystore_exists() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt).unwrap();
        let err = encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt);
        assert!(err.is_err());
    }

    #[test]
    fn encrypt_at_force_reinit_replaces_existing_keystore() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt).unwrap();
        // Note the previous keystore's wrapped_dek to confirm it changes.
        let before = Keystore::load(tmp.path()).unwrap().unwrap().wrapped_dek;
        encrypt_at(tmp.path(), true, true, &keyring, &mut BadPrompt).unwrap();
        let after = Keystore::load(tmp.path()).unwrap().unwrap().wrapped_dek;
        assert_ne!(before, after);
    }

    #[test]
    fn encrypt_refuses_when_lockfile_present() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".lock"), b"42").unwrap();
        let keyring = InMemoryKekStore::new();
        let err = encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt);
        assert!(err.is_err());
    }

    #[test]
    fn recover_at_seeds_keyring_from_mnemonic() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        // Set up an encrypted dir whose keyring entry has been wiped.
        let mnemonic = Mnemonic::generate().unwrap();
        let kek = mnemonic.derive_kek("");
        let dek = Dek::generate().unwrap();
        let ks = Keystore::create(tmp.path(), &dek, &kek, true).unwrap();
        ks.write(tmp.path()).unwrap();
        // Don't store KEK in keyring — simulate fresh machine.

        recover_at(tmp.path(), &mnemonic.to_phrase(), &keyring).unwrap();
        let account = account_for(tmp.path());
        assert!(keyring.load(&account).unwrap().is_some());
    }

    #[test]
    fn recover_rejects_wrong_mnemonic() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        let mnemonic = Mnemonic::generate().unwrap();
        let kek = mnemonic.derive_kek("");
        let dek = Dek::generate().unwrap();
        Keystore::create(tmp.path(), &dek, &kek, true)
            .unwrap()
            .write(tmp.path())
            .unwrap();

        let wrong = Mnemonic::generate().unwrap();
        let err = recover_at(tmp.path(), &wrong.to_phrase(), &keyring);
        assert!(err.is_err(), "wrong mnemonic must be rejected");
    }

    #[test]
    fn rekey_changes_keystore_wrap_but_preserves_dek_bytes() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt).unwrap();
        let ks_before = Keystore::load(tmp.path()).unwrap().unwrap();
        // Reconstruct the DEK via the keyring KEK so we can compare.
        let account = account_for(tmp.path());
        let original_kek = keyring.load(&account).unwrap().unwrap();
        let dek_before = ks_before.unwrap_dek(&original_kek).unwrap();
        let dek_before_bytes = *dek_before.as_bytes();

        // Use no_verify=true so we don't need to know the new mnemonic
        // in advance to script a verifying prompt.
        rekey_at(tmp.path(), true, &keyring, &mut BadPrompt).unwrap();
        let ks_after = Keystore::load(tmp.path()).unwrap().unwrap();
        assert_ne!(ks_before.wrapped_dek, ks_after.wrapped_dek);
        assert!(ks_after.rotated_at.is_some());

        let new_kek = keyring.load(&account).unwrap().unwrap();
        let dek_after = ks_after.unwrap_dek(&new_kek).unwrap();
        assert_eq!(dek_after.as_bytes(), &dek_before_bytes);

        // Old KEK no longer unwraps.
        assert!(ks_after.unwrap_dek(&original_kek).is_err());
    }

    #[test]
    fn decrypt_requires_force_flag() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt).unwrap();
        let err = decrypt_at(tmp.path(), false, &keyring);
        assert!(err.is_err());
        assert!(
            keystore_path(tmp.path()).exists(),
            "keystore must survive a refused decrypt"
        );
    }

    #[test]
    fn decrypt_with_force_removes_keystore_and_keyring() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        encrypt_at(tmp.path(), false, true, &keyring, &mut BadPrompt).unwrap();
        let account = account_for(tmp.path());
        assert!(keyring.load(&account).unwrap().is_some());

        decrypt_at(tmp.path(), true, &keyring).unwrap();
        assert!(!keystore_path(tmp.path()).exists());
        assert!(keyring.load(&account).unwrap().is_none());
    }

    #[test]
    fn decrypt_on_absent_keystore_is_noop() {
        let tmp = TempDir::new().unwrap();
        let keyring = InMemoryKekStore::new();
        decrypt_at(tmp.path(), true, &keyring).unwrap();
    }

    #[test]
    fn happy_path_verifies_mnemonic_when_prompt_answers_correctly() {
        // Drive the verify loop end-to-end with a prompt that mirrors
        // the generated mnemonic. We can't trivially script this for
        // `encrypt_at` because the mnemonic is generated inside; but
        // the helper itself is reachable through verify_mnemonic_loop.
        let m = Mnemonic::generate().unwrap();
        let mut prompt = GoodPrompt::new(&m);
        let ok = verify_mnemonic_loop(&m, &mut prompt).unwrap();
        assert!(ok);
    }
}
