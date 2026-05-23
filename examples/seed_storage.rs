//! Tiny demo helper: populate or verify a mneme data directory.
//!
//! Usage:
//!
//! ```bash
//! MNEME_DATA_DIR=/tmp/foo cargo run --example seed_storage -- write 10
//! MNEME_DATA_DIR=/tmp/foo cargo run --example seed_storage -- read
//! ```
//!
//! - `write N` populates the dir with N plaintext rows under
//!   `RedbStorage::open` (legacy plaintext path).
//! - `read` opens via `crypto::boot::open_episodic_storage`, which
//!   picks the encrypted or plaintext backend based on whether
//!   `keystore.json` is present. Useful to verify that an encrypted
//!   data dir still serves the same values after `mneme encrypt`.
//!
//! Not a public library API; kept under `examples/` so it doesn't
//! show up in `cargo install mneme-mcp` artifacts.

use mneme::crypto::{KekStore, OsKeyring};
use mneme::crypto::boot::open_episodic_storage;
use mneme::memory::procedural::ProceduralStore;
use mneme::storage::Storage;
use mneme::storage::redb_impl::RedbStorage;
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).cloned().unwrap_or_default();
    let root: PathBuf = std::env::var("MNEME_DATA_DIR")
        .map_err(|_| "MNEME_DATA_DIR must be set")?
        .into();

    match cmd.as_str() {
        "write" => {
            let n: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            mneme::cli::init::init_at(&root)?;
            let s = RedbStorage::open(&root.join("episodic"))?;
            for i in 0..n {
                let k = format!("mem:01HKKMIGRATE{i:04}").into_bytes();
                let v = format!("plaintext-marker-row-{i}").into_bytes();
                s.put(&k, &v).await?;
            }
            s.flush().await?;

            // Also drop a procedural pinned item so the migration walk
            // has something visible to encrypt on that surface.
            let proc = ProceduralStore::open(&root)?;
            proc.pin(
                "plaintext-marker-pinned".into(),
                vec!["demo".into()],
                "personal".into(),
            )
            .await?;
            drop(proc);

            println!(
                "wrote {n} plaintext rows + 1 pinned item under {}",
                root.display()
            );
        }
        "read" => {
            let keyring = OsKeyring::new();
            let s = open_episodic_storage(&root, &keyring)?;
            let all = s.scan_prefix(b"mem:01HKKMIGRATE").await?;
            println!("read {} rows:", all.len());
            for (k, v) in all {
                println!(
                    "  {}\t-> {}",
                    String::from_utf8_lossy(&k),
                    String::from_utf8_lossy(&v),
                );
            }
            // Read procedural through whichever mode the dir is in. We
            // could go through boot helpers, but ProceduralStore's
            // legacy open() panics on encrypted dirs (no DEK). So we
            // detect the keystore's presence and pass an Aead.
            let aead = if mneme::crypto::Keystore::load(&root)?.is_some() {
                let keystore = mneme::crypto::Keystore::load(&root)?.unwrap();
                let kek = keyring
                    .load(&keystore.keyring.account)
                    .map_err(|e| format!("keyring: {e}"))?
                    .ok_or("no KEK in keyring")?;
                let dek = keystore.unwrap_dek(&kek)?;
                Some(std::sync::Arc::new(mneme::crypto::Aead::new(&dek)))
            } else {
                None
            };
            let proc = ProceduralStore::open_with_crypto(&root, aead)?;
            let pinned = proc.list(None)?;
            println!("pinned items: {}", pinned.len());
            for p in pinned {
                println!("  {} {} {:?}", p.id, p.content, p.tags);
            }
        }
        other => return Err(format!("unknown command: {other:?}; use `write N` or `read`").into()),
    }
    Ok(())
}
