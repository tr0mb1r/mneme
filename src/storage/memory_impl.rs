//! In-memory [`Storage`] implementation. Backed by a `BTreeMap` behind a
//! `tokio::sync::RwLock`. Useful for tests and for ephemeral configurations
//! where no on-disk persistence is desired.

use crate::Result;
use crate::storage::Storage;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct MemoryStorage {
    inner: RwLock<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemoryStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(BTreeMap::new()),
        })
    }
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.inner
            .write()
            .await
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.read().await.get(key).cloned())
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        self.inner.write().await.remove(key);
        Ok(())
    }

    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let map = self.inner.read().await;
        Ok(map
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_delete() {
        let s = MemoryStorage::new();
        s.put(b"hello", b"world").await.unwrap();
        assert_eq!(s.get(b"hello").await.unwrap(), Some(b"world".to_vec()));
        s.delete(b"hello").await.unwrap();
        assert_eq!(s.get(b"hello").await.unwrap(), None);
    }

    #[tokio::test]
    async fn scan_prefix_filters_correctly() {
        let s = MemoryStorage::new();
        s.put(b"x:1", b"a").await.unwrap();
        s.put(b"x:2", b"b").await.unwrap();
        s.put(b"y:1", b"c").await.unwrap();
        let xs = s.scan_prefix(b"x:").await.unwrap();
        assert_eq!(xs.len(), 2);
        let ys = s.scan_prefix(b"y:").await.unwrap();
        assert_eq!(ys.len(), 1);
    }
}
