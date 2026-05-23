//! [`EncryptedStorage`] — at-rest envelope on top of any [`Storage`] (ADR-0013 P3).
//!
//! Wraps a backend (`RedbStorage` in production, `MemoryStorage` in
//! some tests) and seals/opens every value with the DEK. Keys flow
//! through unmodified — they're identifiers (ULID-shaped) rather than
//! content, and leaving them plaintext preserves redb's O(log n)
//! range scans (rationale in ADR-0013 D7).
//!
//! **AAD discipline (ADR-0013 D4):** every value is bound to its key
//! at the storage layer. A ciphertext blob stored at `mem:01HKK…` can
//! never be moved to `epi:01HKK…` and successfully decrypted —
//! authentication will fail.
//!
//! **WAL pass-through.** This wrapper sits *above* the WAL: the bytes
//! that reach `RedbStorage::put` are already ciphertext, so the WAL
//! frame payload is ciphertext as a side effect. P4 will additionally
//! encrypt the WAL frame structure itself (the postcard envelope
//! around the value) for defence in depth.

use crate::{
    Result,
    crypto::{AadDomain, Aead, Dek},
    storage::Storage,
};
use async_trait::async_trait;
use std::sync::Arc;

/// Wrap any [`Storage`] implementation with per-value AEAD using the
/// supplied DEK.
pub struct EncryptedStorage<S: Storage> {
    inner: Arc<S>,
    aead: Aead,
}

impl<S: Storage> EncryptedStorage<S> {
    /// Construct a new wrapper around `inner` using `dek` as the
    /// per-record key.
    pub fn new(inner: Arc<S>, dek: &Dek) -> Arc<Self> {
        Arc::new(Self {
            inner,
            aead: Aead::new(dek),
        })
    }

    /// Borrow the underlying backend. Tests use this to assert that
    /// what the wrapper writes through is in fact opaque ciphertext.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

#[async_trait]
impl<S: Storage> Storage for EncryptedStorage<S> {
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let envelope = self.aead.seal(AadDomain::Redb, key, value)?;
        self.inner.put(key, &envelope).await
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.inner.get(key).await? {
            Some(envelope) => {
                let plaintext = self.aead.open(AadDomain::Redb, key, &envelope)?;
                Ok(Some(plaintext))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        self.inner.delete(key).await
    }

    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let raw = self.inner.scan_prefix(prefix).await?;
        let mut out = Vec::with_capacity(raw.len());
        for (key, envelope) in raw {
            let plaintext = self.aead.open(AadDomain::Redb, &key, &envelope)?;
            out.push((key, plaintext));
        }
        Ok(out)
    }

    async fn flush(&self) -> Result<()> {
        self.inner.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Dek;
    use crate::storage::memory_impl::MemoryStorage;

    fn fixture() -> (
        Arc<EncryptedStorage<MemoryStorage>>,
        Arc<MemoryStorage>,
        Dek,
    ) {
        let inner = MemoryStorage::new();
        let dek = Dek::generate().unwrap();
        // We need a second handle to the inner backend to inspect raw
        // ciphertext bytes — clone the Arc before handing it to the
        // wrapper.
        let inner_observer = inner.clone();
        let wrapped = EncryptedStorage::new(inner, &dek);
        (wrapped, inner_observer, dek)
    }

    #[tokio::test]
    async fn put_get_round_trip() {
        let (s, _, _dek) = fixture();
        s.put(b"mem:foo", b"hello").await.unwrap();
        assert_eq!(s.get(b"mem:foo").await.unwrap(), Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn raw_backend_holds_only_ciphertext() {
        let (s, raw, _dek) = fixture();
        s.put(b"mem:foo", b"the quick brown fox jumps over the lazy dog")
            .await
            .unwrap();
        let on_disk = raw.get(b"mem:foo").await.unwrap().unwrap();
        // Plaintext must not appear verbatim in the inner backend.
        let plain = b"the quick brown fox";
        let found = on_disk.windows(plain.len()).any(|w| w == plain);
        assert!(!found, "plaintext leaked into the inner backend ciphertext");
        // Envelope must start with the MNE1 magic.
        assert_eq!(&on_disk[..4], b"MNE1");
    }

    #[tokio::test]
    async fn delete_passes_through() {
        let (s, _, _dek) = fixture();
        s.put(b"mem:x", b"v").await.unwrap();
        s.delete(b"mem:x").await.unwrap();
        assert_eq!(s.get(b"mem:x").await.unwrap(), None);
    }

    #[tokio::test]
    async fn scan_prefix_decrypts_each_value() {
        let (s, _, _dek) = fixture();
        s.put(b"epi:a", b"alpha").await.unwrap();
        s.put(b"epi:b", b"bravo").await.unwrap();
        s.put(b"mem:c", b"charlie").await.unwrap();

        let mut epi = s.scan_prefix(b"epi:").await.unwrap();
        epi.sort();
        assert_eq!(
            epi,
            vec![
                (b"epi:a".to_vec(), b"alpha".to_vec()),
                (b"epi:b".to_vec(), b"bravo".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn get_with_wrong_dek_fails() {
        let inner = MemoryStorage::new();
        let dek_a = Dek::generate().unwrap();
        let dek_b = Dek::generate().unwrap();
        let writer = EncryptedStorage::new(inner.clone(), &dek_a);
        writer.put(b"mem:secret", b"plaintext").await.unwrap();

        let reader = EncryptedStorage::new(inner, &dek_b);
        assert!(reader.get(b"mem:secret").await.is_err());
    }

    #[tokio::test]
    async fn moving_ciphertext_to_a_different_key_fails_to_decrypt() {
        // AAD-binding regression: rewriting the same ciphertext under
        // a different key must fail authentication.
        let (s, raw, _) = fixture();
        s.put(b"mem:original", b"secret value").await.unwrap();
        let ciphertext = raw.get(b"mem:original").await.unwrap().unwrap();
        raw.put(b"mem:relocated", &ciphertext).await.unwrap();
        assert!(s.get(b"mem:relocated").await.is_err());
    }

    #[tokio::test]
    async fn moving_ciphertext_to_a_different_prefix_fails_too() {
        // Even within the same logical id, swapping the table prefix
        // (mem: → epi:) must fail authentication.
        let (s, raw, _) = fixture();
        s.put(b"mem:01XYZ", b"semantic memory body").await.unwrap();
        let ciphertext = raw.get(b"mem:01XYZ").await.unwrap().unwrap();
        raw.put(b"epi:01XYZ", &ciphertext).await.unwrap();
        assert!(s.get(b"epi:01XYZ").await.is_err());
    }

    #[tokio::test]
    async fn tampered_byte_in_ciphertext_fails() {
        let (s, raw, _) = fixture();
        s.put(b"mem:t", b"important data").await.unwrap();
        let mut ciphertext = raw.get(b"mem:t").await.unwrap().unwrap();
        // Flip a bit deep in the ciphertext body, past the header.
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0x01;
        raw.put(b"mem:t", &ciphertext).await.unwrap();
        assert!(s.get(b"mem:t").await.is_err());
    }

    #[tokio::test]
    async fn get_missing_key_returns_none() {
        let (s, _, _) = fixture();
        assert_eq!(s.get(b"mem:absent").await.unwrap(), None);
    }

    #[tokio::test]
    async fn flush_passes_through() {
        let (s, _, _) = fixture();
        s.flush().await.unwrap();
    }

    #[tokio::test]
    async fn each_put_produces_distinct_ciphertext_for_same_value() {
        // Nonce-freshness regression: repeated puts of the same value
        // at the same key must overwrite with a fresh nonce so an
        // observer of the raw backend can't tell that the plaintext is
        // unchanged.
        let (s, raw, _) = fixture();
        s.put(b"mem:repeat", b"same value").await.unwrap();
        let first = raw.get(b"mem:repeat").await.unwrap().unwrap();
        s.put(b"mem:repeat", b"same value").await.unwrap();
        let second = raw.get(b"mem:repeat").await.unwrap().unwrap();
        assert_ne!(
            first, second,
            "nonce reuse — same ciphertext for same value"
        );
        // But the wrapped get still returns the plaintext correctly.
        assert_eq!(
            s.get(b"mem:repeat").await.unwrap(),
            Some(b"same value".to_vec())
        );
    }
}
