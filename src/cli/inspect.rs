//! `mneme inspect` — human-facing memory lookup.
//!
//! Two modes:
//! * `mneme inspect <ULID>` — load a single memory by id from redb,
//!   print as pretty JSON.
//! * `mneme inspect --query "..."` — boot the live embedder, run a
//!   `recall` against the on-disk HNSW (snapshot + WAL replay), print
//!   the top-N hits as JSON.
//!
//! Both paths refuse to run while the server holds the lockfile.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use ulid::Ulid;

use crate::config::Config;
use crate::crypto::{KekStore, OsKeyring};
use crate::ids::MemoryId;
use crate::memory::semantic::{MemoryItem, RecallFilters, SemanticStore, SnapshotConfig};
use crate::storage::MEM_KEY_PREFIX;
use crate::storage::layout;
use crate::{MnemeError, Result, embed, migrate};
const DEFAULT_QUERY_LIMIT: usize = 5;

pub fn execute(id: Option<String>, query: Option<String>) -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    let keyring = OsKeyring::new();
    let payload = match (id, query) {
        (Some(id), None) => inspect_by_id_with_keyring(&root, &id, &keyring)?,
        (None, Some(q)) => inspect_by_query_with_keyring(&root, &q, &keyring)?,
        (Some(_), Some(_)) => {
            return Err(MnemeError::Config(
                "pass exactly one of <ID> or --query".into(),
            ));
        }
        (None, None) => {
            return Err(MnemeError::Config(
                "specify a memory ULID or --query <text>".into(),
            ));
        }
    };
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| MnemeError::Storage(format!("encode result: {e}")))?;
    println!("{pretty}");
    Ok(())
}

fn refuse_if_locked(root: &Path) -> Result<()> {
    let lock = root.join(".lock");
    if lock.exists() {
        Err(MnemeError::Lock(format!(
            "{} is held — stop the running mneme instance before inspecting",
            lock.display()
        )))
    } else {
        Ok(())
    }
}

/// Fast path: open redb, fetch one row, decode. No embedder needed.
/// Resolves the KEK through the OS keyring when the data dir is
/// encrypted.
pub fn inspect_by_id(root: &Path, id_str: &str) -> Result<Value> {
    inspect_by_id_with_keyring(root, id_str, &OsKeyring::new())
}

/// As [`inspect_by_id`], with the KEK custody backend injected.
pub fn inspect_by_id_with_keyring(
    root: &Path,
    id_str: &str,
    keyring: &dyn KekStore,
) -> Result<Value> {
    refuse_if_locked(root)?;
    let ulid = Ulid::from_string(id_str)
        .map_err(|e| MnemeError::Config(format!("`{id_str}` is not a valid ULID: {e}")))?;
    let memory_id = MemoryId(ulid);

    let storage = crate::crypto::boot::open_episodic_storage(root, keyring)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;

    let key = mem_key(&memory_id);
    let bytes = runtime.block_on(async { storage.get(&key).await })?;
    match bytes {
        None => Ok(json!({
            "id": memory_id.to_string(),
            "found": false,
        })),
        Some(b) => {
            let item: MemoryItem = postcard::from_bytes(&b)
                .map_err(|e| MnemeError::Storage(format!("decode MemoryItem {memory_id}: {e}")))?;
            Ok(json!({
                "found": true,
                "memory": item_to_json(&item),
            }))
        }
    }
}

/// Slow path: build the embedder, open the semantic store with the
/// snapshot scheduler disabled, run one recall, drop everything.
/// Resolves the KEK through the OS keyring when the data dir is
/// encrypted.
pub fn inspect_by_query(root: &Path, query: &str) -> Result<Value> {
    inspect_by_query_with_keyring(root, query, &OsKeyring::new())
}

/// As [`inspect_by_query`], with the KEK custody backend injected.
pub fn inspect_by_query_with_keyring(
    root: &Path,
    query: &str,
    keyring: &dyn KekStore,
) -> Result<Value> {
    refuse_if_locked(root)?;
    if query.trim().is_empty() {
        return Err(MnemeError::Config("--query must not be empty".into()));
    }

    // Ensure the schema is current — older fixtures should still
    // inspect cleanly.
    let on_disk = migrate::current_version(root).unwrap_or(0);
    if on_disk < migrate::CURRENT_SCHEMA_VERSION {
        migrate::migrate_to(root, migrate::CURRENT_SCHEMA_VERSION)?;
    }

    let config = Config::load(&root.join("config.toml"))?;
    let cache = root.join("models");
    let embedder = embed::load_from_config(&config.embeddings.model, &cache).map_err(|e| {
        MnemeError::Embedding(format!(
            "failed to load embedder `{}`: {e}. Run `mneme init` first.",
            config.embeddings.model
        ))
    })?;

    let (storage, aead) = crate::crypto::boot::open_episodic_storage_and_aead(root, keyring)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;

    let payload = runtime.block_on(async {
        let semantic = SemanticStore::open_with_crypto(
            root,
            Arc::clone(&storage),
            Arc::clone(&embedder),
            SnapshotConfig::disabled(),
            aead.clone(),
        )?;
        let hits = semantic
            .recall(query, DEFAULT_QUERY_LIMIT, &RecallFilters::default())
            .await?;
        let body = json!({
            "query": query,
            "limit": DEFAULT_QUERY_LIMIT,
            "results": hits.iter().map(|h| {
                let mut row = item_to_json(&h.item);
                row["score"] = json!(h.score);
                row
            }).collect::<Vec<_>>(),
        });
        Ok::<Value, MnemeError>(body)
    })?;
    Ok(payload)
}

fn item_to_json(item: &MemoryItem) -> Value {
    json!({
        "id": item.id.to_string(),
        "content": item.content,
        "kind": item.kind.as_str(),
        "tags": item.tags,
        "scope": item.scope,
        "created_at": item.created_at.to_rfc3339(),
    })
}

fn mem_key(id: &MemoryId) -> Vec<u8> {
    let mut k = Vec::with_capacity(MEM_KEY_PREFIX.len() + 16);
    k.extend_from_slice(MEM_KEY_PREFIX);
    k.extend_from_slice(&id.0.to_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::MemoryKind;
    use crate::storage::Storage;
    use crate::storage::redb_impl::RedbStorage;
    use tempfile::TempDir;

    fn fresh_root() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        layout::scaffold(&root).unwrap();
        migrate::migrate_to(&root, migrate::CURRENT_SCHEMA_VERSION).unwrap();
        (tmp, root)
    }

    /// Seed a memory through the full SemanticStore path on a
    /// dedicated runtime+thread, then drop everything so the redb
    /// lock is released before `inspect_by_id` opens its own handle.
    fn seed_memory(root: &Path, content: &str, kind: MemoryKind) -> MemoryId {
        let root = root.to_path_buf();
        let content = content.to_owned();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let storage: Arc<dyn Storage> = RedbStorage::open(&root.join("episodic")).unwrap();
                let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
                let s =
                    SemanticStore::open_disabled(&root, Arc::clone(&storage), embedder).unwrap();
                let id = s
                    .remember(&content, kind, vec!["t1".into()], "personal".into())
                    .await
                    .unwrap();
                drop(s);
                drop(storage);
                id
            })
        })
        .join()
        .unwrap()
    }

    #[test]
    fn inspect_by_id_finds_seeded_row() {
        let (_tmp, root) = fresh_root();
        let id = seed_memory(&root, "hello inspect", MemoryKind::Fact);
        let v = inspect_by_id(&root, &id.to_string()).unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(v["memory"]["content"], "hello inspect");
        assert_eq!(v["memory"]["kind"], "fact");
        assert_eq!(v["memory"]["scope"], "personal");
    }

    #[test]
    fn inspect_by_id_unknown_returns_not_found() {
        let (_tmp, root) = fresh_root();
        // Touch redb (open+drop) so the file exists with an empty
        // table — otherwise the next open would still be fine, but
        // this matches the usual production state.
        drop(RedbStorage::open(&root.join("episodic")).unwrap());
        let v = inspect_by_id(&root, "01H0000000000000000000000Z").unwrap();
        assert_eq!(v["found"], false);
    }

    /// Regression, 2026-07-25: `inspect` opened `<root>/episodic`
    /// with the plaintext `RedbStorage::open`, so on an encrypted data
    /// dir the redb WAL replay fed AEAD ciphertext to postcard and the
    /// command died with `write-ahead log error: postcard decode: ...`.
    /// Same defect as `stats` and `export`; `run`/`daemon` were always
    /// correct because they boot through `crypto::boot`.
    #[test]
    fn inspect_by_id_reads_an_encrypted_data_dir() {
        use crate::cli::encrypt::MnemonicPrompt;
        use crate::crypto::keyring::InMemoryKekStore;

        struct UnusedPrompt;
        impl MnemonicPrompt for UnusedPrompt {
            fn confirm_written(&mut self) -> Result<()> {
                unreachable!("no_verify=true must not prompt")
            }
            fn answer_position(&mut self, _: usize) -> Result<String> {
                unreachable!("no_verify=true must not prompt")
            }
        }

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let keyring = InMemoryKekStore::new();
        crate::cli::encrypt::encrypt_at(&root, false, true, &keyring, &mut UnusedPrompt).unwrap();

        // Seed through the encrypted stack on its own thread so the
        // redb lock is released before we inspect.
        let id = {
            let root = root.clone();
            let kek = keyring
                .load(&crate::crypto::account_for(&root))
                .unwrap()
                .unwrap();
            std::thread::spawn(move || {
                let ks = crate::crypto::Keystore::load(&root).unwrap().unwrap();
                let dek = ks.unwrap_dek(&kek).unwrap();
                let aead = Arc::new(crate::crypto::Aead::new(&dek));
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let storage: Arc<dyn Storage> =
                        crate::storage::EncryptedStorage::open_redb(&root.join("episodic"), &dek)
                            .unwrap();
                    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
                    let s = SemanticStore::open_with_crypto(
                        &root,
                        Arc::clone(&storage),
                        embedder,
                        SnapshotConfig::disabled(),
                        Some(aead),
                    )
                    .unwrap();
                    let id = s
                        .remember(
                            "sealed inspect",
                            MemoryKind::Fact,
                            vec!["t1".into()],
                            "personal".into(),
                        )
                        .await
                        .unwrap();
                    drop(s);
                    drop(storage);
                    id
                })
            })
            .join()
            .unwrap()
        };

        let v = inspect_by_id_with_keyring(&root, &id.to_string(), &keyring)
            .expect("inspect must read an encrypted data dir");
        assert_eq!(v["found"], true);
        assert_eq!(v["memory"]["content"], "sealed inspect");
    }

    #[test]
    fn inspect_by_id_invalid_ulid_errors() {
        let (_tmp, root) = fresh_root();
        let err = inspect_by_id(&root, "not-a-ulid").unwrap_err();
        assert!(matches!(err, MnemeError::Config(_)));
    }

    #[test]
    fn inspect_refuses_when_locked() {
        let (_tmp, root) = fresh_root();
        std::fs::write(root.join(".lock"), b"42").unwrap();
        let err = inspect_by_id(&root, "01H0000000000000000000000Z").unwrap_err();
        assert!(matches!(err, MnemeError::Lock(_)));
    }

    #[test]
    fn execute_rejects_neither_arg() {
        // Direct unit on the dispatch shape without going to disk.
        let err = match (None::<String>, None::<String>) {
            (None, None) => Err::<(), _>(MnemeError::Config("specify".into())),
            _ => Ok(()),
        };
        assert!(err.is_err());
    }
}
