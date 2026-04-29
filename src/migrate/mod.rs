//! Schema migration framework.
//!
//! `schema_version` is independent of the binary's `Cargo.toml` version.
//! Patches that don't change disk format don't bump the schema; format
//! changes do.
//!
//! Phase 2 ships a single no-op migration v0 → v1. The framework is in
//! place so future schema bumps are additive.

use crate::{MnemeError, Result};
use std::path::Path;

pub mod v1;

/// Current schema version supported by this binary build.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Filename of the schema-version sentinel under `~/.mneme/`.
pub const SCHEMA_VERSION_FILE: &str = "schema_version";

/// Read the on-disk schema version. Returns 0 if the file is missing
/// (a fresh install).
pub fn current_version(root: &Path) -> Result<u32> {
    let path = root.join(SCHEMA_VERSION_FILE);
    if !path.exists() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(&path)?;
    text.trim()
        .parse::<u32>()
        .map_err(|e| MnemeError::Migration(format!("{path:?}: {e}")))
}

/// Migrate the data directory from its current schema version up to `target`.
///
/// * If the disk version equals `target`, this is a no-op.
/// * If the disk version is **higher** than `target`, refuses with an
///   error — downgrades are not supported (spec §8.5).
/// * Otherwise runs each version's migration in ascending order.
pub fn migrate_to(root: &Path, target: u32) -> Result<()> {
    let current = current_version(root)?;
    if current == target {
        return Ok(());
    }
    if current > target {
        return Err(MnemeError::Migration(format!(
            "on-disk schema_version {current} is newer than this binary supports ({target}); refusing to downgrade"
        )));
    }
    for next in (current + 1)..=target {
        match next {
            1 => v1::run(root)?,
            other => {
                return Err(MnemeError::Migration(format!(
                    "no migration registered for v{other}"
                )));
            }
        }
    }
    Ok(())
}

/// Write the schema version sentinel. Used by individual migration steps.
pub(crate) fn write_schema_version(root: &Path, version: u32) -> Result<()> {
    let path = root.join(SCHEMA_VERSION_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{version}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fresh_install_reads_zero() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(current_version(tmp.path()).unwrap(), 0);
    }

    #[test]
    fn migrate_v0_to_v1_writes_file() {
        let tmp = TempDir::new().unwrap();
        migrate_to(tmp.path(), 1).unwrap();
        assert_eq!(current_version(tmp.path()).unwrap(), 1);
    }

    #[test]
    fn idempotent_when_already_at_target() {
        let tmp = TempDir::new().unwrap();
        migrate_to(tmp.path(), 1).unwrap();
        migrate_to(tmp.path(), 1).unwrap();
        assert_eq!(current_version(tmp.path()).unwrap(), 1);
    }

    #[test]
    fn refuses_downgrade() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(SCHEMA_VERSION_FILE), "99\n").unwrap();
        let err = migrate_to(tmp.path(), 1).unwrap_err();
        match err {
            MnemeError::Migration(msg) => assert!(msg.contains("99")),
            other => panic!("expected Migration error, got {other:?}"),
        }
    }

    /// Plan §3 Phase 6 deliverable: "schema migration framework
    /// with one successful round-trip from a 'v0' fixture."
    ///
    /// A "v0" install is one created before the schema_version
    /// sentinel was introduced — `~/.mneme/` exists with config +
    /// data, but no `schema_version` file. After `migrate_to(1)` the
    /// existing data must be untouched and the sentinel must be
    /// written with the right version.
    #[test]
    fn migrates_realistic_v0_install_to_v1() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Hand-build a v0-shaped tree.
        let config_body = b"[storage]\nmax_size_gb = 5\n";
        let pinned_body = b"{\"id\":\"01H0000000000000000000000Z\",\
                            \"content\":\"prefer rust\",\
                            \"tags\":[],\
                            \"scope\":\"personal\",\
                            \"created_at\":\"2026-04-28T16:48:00Z\"}\n";
        let wal_body = b"\xff\xff\xff\xfffake-wal-tail";
        std::fs::write(root.join("config.toml"), config_body).unwrap();
        std::fs::create_dir_all(root.join("procedural")).unwrap();
        std::fs::write(root.join("procedural").join("pinned.jsonl"), pinned_body).unwrap();
        std::fs::create_dir_all(root.join("episodic/wal")).unwrap();
        std::fs::write(root.join("episodic/wal/wal-0000.log"), wal_body).unwrap();
        // Sanity: no schema_version file present.
        assert!(!root.join(SCHEMA_VERSION_FILE).exists());
        assert_eq!(current_version(root).unwrap(), 0);

        // Run the migration.
        migrate_to(root, 1).unwrap();

        // Sentinel is now at v1.
        assert_eq!(current_version(root).unwrap(), 1);
        // Existing data was untouched byte-for-byte.
        assert_eq!(
            std::fs::read(root.join("config.toml")).unwrap(),
            config_body
        );
        assert_eq!(
            std::fs::read(root.join("procedural").join("pinned.jsonl")).unwrap(),
            pinned_body
        );
        assert_eq!(
            std::fs::read(root.join("episodic/wal/wal-0000.log")).unwrap(),
            wal_body
        );

        // Re-running the migration is a no-op (idempotent at the
        // exit gate).
        migrate_to(root, 1).unwrap();
        assert_eq!(current_version(root).unwrap(), 1);
        assert_eq!(
            std::fs::read(root.join("config.toml")).unwrap(),
            config_body
        );
    }
}
