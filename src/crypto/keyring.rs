//! OS-keyring backend for the KEK (ADR-0013 D6).
//!
//! The daemon's happy-path key custody: read the wrapping key from
//! the user's OS keyring (Keychain on macOS, Secret Service on
//! Linux, DPAPI on Windows). On headless boxes where no keyring
//! backend is reachable, the caller falls through to the recovery
//! mnemonic in `MNEME_RECOVERY_PHRASE`.
//!
//! Account-name discipline (ADR-0013 D6): every account is derived
//! from the canonical absolute path of the data directory via
//! `sha256(path)[:16]` hex, so two independent mneme installs on the
//! same machine — e.g. one under `$HOME/.mneme` and a test fixture
//! under `$MNEME_DATA_DIR=/tmp/foo` — never share an entry.
//!
//! ## Test backend
//!
//! Production code uses [`OsKeyring`], which wraps the `keyring`
//! crate. Tests use the [`KekStore`] trait directly with an in-memory
//! impl ([`InMemoryKekStore`]) so they don't pollute the host's real
//! keyring nor depend on a D-Bus daemon in CI.

use crate::{
    MnemeError, Result,
    crypto::key::{KEY_LEN, Kek},
};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The fixed `service` identifier we register every entry under.
/// Stable across releases so older daemons can read newer keystores
/// and vice-versa.
pub const KEYRING_SERVICE: &str = "mneme";

/// Length of the keyring account string (16 bytes of SHA-256 hex).
pub const ACCOUNT_LEN: usize = 32;

/// Abstract over the platform keyring so tests can run without
/// touching the host's real backend.
pub trait KekStore: Send + Sync {
    /// Persist the KEK under `account`. Idempotent — overwrites any
    /// existing entry.
    fn store(&self, account: &str, kek: &Kek) -> Result<()>;
    /// Read the KEK under `account`. Returns `Ok(None)` when the
    /// entry is missing (the caller should then try the recovery
    /// mnemonic). Returns `Err` only when the backend itself failed.
    fn load(&self, account: &str) -> Result<Option<Kek>>;
    /// Remove the KEK under `account`. No-op if missing.
    fn delete(&self, account: &str) -> Result<()>;
}

/// Compute the keyring account identifier for a data directory.
///
/// `data_dir` does not need to exist on disk — the function canonicalises
/// when it can (resolves symlinks) and falls back to the absolute path
/// when the directory hasn't been scaffolded yet (the `mneme encrypt`
/// case before keystore.json is written).
pub fn account_for(data_dir: &Path) -> String {
    let absolute = std::fs::canonicalize(data_dir)
        .ok()
        .unwrap_or_else(|| absolutise(data_dir));

    let mut hasher = Sha256::new();
    hasher.update(absolute.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    // 16 bytes → 32 hex chars
    hex_lower(&digest[..16])
}

fn absolutise(p: &Path) -> std::path::PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|| p.to_path_buf())
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

/// Production implementation backed by the `keyring` crate.
pub struct OsKeyring;

impl OsKeyring {
    pub fn new() -> Self {
        Self
    }

    fn entry(account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, account)
            .map_err(|e| MnemeError::Crypto(format!("keyring open: {e}")))
    }
}

impl Default for OsKeyring {
    fn default() -> Self {
        Self::new()
    }
}

impl KekStore for OsKeyring {
    fn store(&self, account: &str, kek: &Kek) -> Result<()> {
        Self::entry(account)?
            .set_secret(kek.as_bytes())
            .map_err(|e| MnemeError::Crypto(format!("keyring store: {e}")))
    }

    fn load(&self, account: &str) -> Result<Option<Kek>> {
        match Self::entry(account)?.get_secret() {
            Ok(bytes) => {
                if bytes.len() != KEY_LEN {
                    return Err(MnemeError::Crypto(format!(
                        "keyring entry has wrong length: expected {KEY_LEN}, got {}",
                        bytes.len()
                    )));
                }
                let mut buf = [0u8; KEY_LEN];
                buf.copy_from_slice(&bytes);
                Ok(Some(Kek::from_bytes(buf)))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(MnemeError::Crypto(format!("keyring load: {e}"))),
        }
    }

    fn delete(&self, account: &str) -> Result<()> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(MnemeError::Crypto(format!("keyring delete: {e}"))),
        }
    }
}

/// In-memory implementation for tests and `--no-keyring` headless
/// fallbacks.
///
/// **Security caveat:** the KEK lives in process RAM only. The
/// caller is responsible for not using this in production paths —
/// the daemon's boot path must wire [`OsKeyring`] or fail closed
/// (ADR-0013 D9). Matches the [`MemoryStorage`] pattern in
/// `crate::storage` (in-tree test fixture, plain `pub`).
///
/// [`MemoryStorage`]: crate::storage::memory_impl::MemoryStorage
#[derive(Default)]
pub struct InMemoryKekStore {
    inner: std::sync::Mutex<std::collections::HashMap<String, [u8; KEY_LEN]>>,
}

impl InMemoryKekStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KekStore for InMemoryKekStore {
    fn store(&self, account: &str, kek: &Kek) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(account.to_string(), *kek.as_bytes());
        Ok(())
    }
    fn load(&self, account: &str) -> Result<Option<Kek>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(account)
            .copied()
            .map(Kek::from_bytes))
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.inner.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn account_id_is_32_hex_chars() {
        let tmp = TempDir::new().unwrap();
        let id = account_for(tmp.path());
        assert_eq!(id.len(), ACCOUNT_LEN);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    #[test]
    fn account_id_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        let a = account_for(tmp.path());
        let b = account_for(tmp.path());
        assert_eq!(a, b);
    }

    #[test]
    fn account_id_differs_for_distinct_paths() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let a = account_for(tmp1.path());
        let b = account_for(tmp2.path());
        assert_ne!(a, b);
    }

    #[test]
    fn account_id_handles_nonexistent_path() {
        // Pre-scaffold data dirs go through the non-canonicalize path.
        let p = std::path::Path::new("/definitely/not/a/real/path-for-account-test");
        let id = account_for(p);
        assert_eq!(id.len(), ACCOUNT_LEN);
    }

    #[test]
    fn account_id_resolves_symlinks_when_target_exists() {
        // canonicalize() resolves /var → /private/var on macOS,
        // symlinks generally elsewhere. The contract is that
        // *equivalent* paths produce the same account id.
        let tmp = TempDir::new().unwrap();
        let direct = account_for(tmp.path());
        // Build a `./<basename>` view by relativising — both should
        // canonicalize to the same absolute path.
        let absolute = std::fs::canonicalize(tmp.path()).unwrap();
        let via_abs = account_for(&absolute);
        assert_eq!(direct, via_abs);
    }

    #[test]
    fn in_memory_store_round_trips() {
        let store = InMemoryKekStore::new();
        let account = "test-account";
        let kek = Kek::from_bytes([0x55; KEY_LEN]);
        store.store(account, &kek).unwrap();
        let loaded = store.load(account).unwrap().unwrap();
        assert_eq!(loaded.as_bytes(), kek.as_bytes());
    }

    #[test]
    fn in_memory_store_returns_none_for_missing_account() {
        let store = InMemoryKekStore::new();
        assert!(store.load("nope").unwrap().is_none());
    }

    #[test]
    fn in_memory_store_overwrites_on_repeat_store() {
        let store = InMemoryKekStore::new();
        let account = "rekey-test";
        store
            .store(account, &Kek::from_bytes([1; KEY_LEN]))
            .unwrap();
        store
            .store(account, &Kek::from_bytes([2; KEY_LEN]))
            .unwrap();
        let loaded = store.load(account).unwrap().unwrap();
        assert_eq!(loaded.as_bytes(), &[2; KEY_LEN]);
    }

    #[test]
    fn in_memory_store_delete_is_idempotent() {
        let store = InMemoryKekStore::new();
        store.delete("absent").unwrap();
        store
            .store("present", &Kek::from_bytes([0; KEY_LEN]))
            .unwrap();
        store.delete("present").unwrap();
        store.delete("present").unwrap();
        assert!(store.load("present").unwrap().is_none());
    }
}
