//! Directory scaffold for the on-disk layout described in spec §7.3.
//!
//! Files (`config.toml`, `schema_version`, `.lock`) are written by the
//! modules that own them; this module only creates the directory tree.

use crate::Result;
use std::path::{Path, PathBuf};

/// Subdirectories created under `root` per spec §7.3.
pub const SUBDIRS: &[&str] = &[
    "procedural",
    "episodic",
    "episodic/data",
    "episodic/wal",
    "semantic",
    "semantic/wal",
    "cold",
    "sessions",
    "scopes",
    "logs",
    "models",
];

/// Create the `~/.mneme` directory tree under `root`. Idempotent.
pub fn scaffold(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    for sub in SUBDIRS {
        std::fs::create_dir_all(root.join(sub))?;
    }
    Ok(())
}

/// Default data directory.
///
/// Resolved as: `$MNEME_DATA_DIR` if set, otherwise `~/.mneme`. The env
/// var is the escape hatch tests and power users use to point mneme at
/// a non-default location. Returns `None` only if both lookups fail.
pub fn default_root() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("MNEME_DATA_DIR")
        && !s.is_empty()
    {
        return Some(PathBuf::from(s));
    }
    dirs::home_dir().map(|h| h.join(".mneme"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scaffold_creates_all_subdirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mneme");
        scaffold(&root).unwrap();
        for sub in SUBDIRS {
            assert!(root.join(sub).is_dir(), "expected {sub} to be a directory");
        }
    }

    #[test]
    fn scaffold_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mneme");
        scaffold(&root).unwrap();
        scaffold(&root).unwrap();
    }
}
