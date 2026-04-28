//! Schema migration framework.
//!
//! `schema_version` is independent of the binary's `Cargo.toml` version
//! (per locked decision in `proj_docs/mneme-implementation-plan.md` §0).
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
}
