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

/// SEC-003: data-bearing directory mode. Memory content has 180+ day
/// retention and may include credentials despite the
/// privacy-discipline rule, so the data dir and every subdir is
/// pinned to user-only access. Applied on every `scaffold` call so
/// the invariant survives manual `chmod`s that loosened permissions.
#[cfg(unix)]
const DATA_DIR_MODE: u32 = 0o700;

/// Create the `~/.mneme` directory tree under `root`. Idempotent.
///
/// On Unix, tightens the root and every subdirectory to `0o700`
/// (SEC-003 from the 2026-05-23 audit) so a misconfigured umask
/// can't leak the data dir to group/world. On Windows this is a
/// no-op; ACL-based hardening is out of scope for this fix.
pub fn scaffold(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    apply_data_dir_mode(root)?;
    for sub in SUBDIRS {
        let path = root.join(sub);
        std::fs::create_dir_all(&path)?;
        apply_data_dir_mode(&path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_data_dir_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(DATA_DIR_MODE);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_data_dir_mode(_path: &Path) -> Result<()> {
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

    /// SEC-003: the data dir and every subdir end up at 0o700 after
    /// scaffold. Even if the umask is permissive (e.g. 0o022 → dirs
    /// would otherwise inherit 0o755), `apply_data_dir_mode` clamps
    /// them down.
    #[cfg(unix)]
    #[test]
    fn scaffold_locks_data_dirs_to_0o700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mneme");
        scaffold(&root).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "root mode should be 0o700, got {mode:o}");
        for sub in SUBDIRS {
            let mode = std::fs::metadata(root.join(sub))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{sub} mode should be 0o700, got {mode:o}");
        }
    }

    /// SEC-003: scaffold also tightens an existing, looser data dir
    /// on a re-run — so a one-off `chmod 0o755` on an upgrade path
    /// doesn't strand the invariant.
    #[cfg(unix)]
    #[test]
    fn scaffold_retightens_existing_loose_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mneme");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        scaffold(&root).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "scaffold must retighten a loose root");
    }
}
