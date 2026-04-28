//! `mneme restore <path>` — Phase 6 §11.2 deliverable.
//!
//! Read a `mneme backup`-produced `.tar.gz` and unpack into the data
//! directory. Refuses to clobber an already-populated `<root>/`
//! unless `--force` is supplied.
//!
//! Like `backup`, the command refuses to run while a server holds
//! the lockfile.

use std::fs::File;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use crate::storage::layout;
use crate::{MnemeError, Result};

pub fn execute(input: PathBuf, force: bool) -> Result<()> {
    let root = layout::default_root()
        .ok_or_else(|| MnemeError::Config("could not resolve ~/.mneme".into()))?;
    restore_at(&input, &root, force)
}

/// Library entry point. Tests call directly.
pub fn restore_at(input: &Path, root: &Path, force: bool) -> Result<()> {
    if !input.exists() {
        return Err(MnemeError::Config(format!(
            "backup file {} does not exist",
            input.display()
        )));
    }
    refuse_if_locked(root)?;
    if root_has_data(root)? && !force {
        return Err(MnemeError::Config(format!(
            "{} is non-empty — pass --force to overwrite",
            root.display()
        )));
    }

    std::fs::create_dir_all(root)?;
    let f = File::open(input)?;
    let gz = GzDecoder::new(f);
    let mut tar = tar::Archive::new(gz);
    // Don't trust archive paths blindly — `tar` 0.4 already refuses
    // entries that escape the unpack directory, but we belt-and-
    // suspenders by walking the entries one at a time.
    let mut count = 0usize;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.is_absolute() || path.components().any(|c| c.as_os_str() == "..") {
            return Err(MnemeError::Config(format!(
                "archive entry {} has an unsafe path; refusing to restore",
                path.display()
            )));
        }
        let dest = root.join(&path);
        // tar::Entry::unpack handles dir/file/symlink polymorphism.
        entry.unpack(&dest)?;
        count += 1;
    }
    eprintln!("restored {count} entries from {}", input.display());
    Ok(())
}

fn refuse_if_locked(root: &Path) -> Result<()> {
    let lock_path = root.join(".lock");
    if lock_path.exists() {
        return Err(MnemeError::Lock(format!(
            "{} is held — stop the running mneme instance before restoring",
            lock_path.display()
        )));
    }
    Ok(())
}

fn root_has_data(root: &Path) -> Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    // "Has data" ≡ "any non-hidden, non-`.lock`, non-empty entry".
    // `.lock` shouldn't be present (we refuse_if_locked above) but
    // ignoring it here keeps the check robust under stale lockfiles.
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s == ".lock" || s.starts_with('.') {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, body: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn fixture_with_data() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mneme");
        write(&root.join("config.toml"), b"max_size_gb = 10");
        write(&root.join("episodic/wal/wal-001.log"), b"wal-bytes");
        write(&root.join("procedural/pinned.jsonl"), b"{\"a\":1}\n");
        // Excluded subdirs to make sure backup→restore loses them.
        write(&root.join("logs/mneme.log"), b"chatter");
        write(&root.join("models/blob.bin"), b"blob");
        tmp
    }

    /// Spec §11.2 exit gate: backup → wipe → restore → all data
    /// present and queryable.
    #[test]
    fn backup_then_restore_preserves_data() {
        let src_tmp = fixture_with_data();
        let src_root = src_tmp.path().join("mneme");

        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        super::super::backup::backup_at(&src_root, &archive, false).unwrap();

        // Wipe + restore into a fresh dir. The exit-gate phrasing
        // says "wipe" — we model that as a fresh empty root.
        let dst_tmp = TempDir::new().unwrap();
        let dst_root = dst_tmp.path().join("mneme");
        restore_at(&archive, &dst_root, false).unwrap();

        // Files we backed up are present.
        assert!(dst_root.join("config.toml").exists());
        assert!(dst_root.join("episodic/wal/wal-001.log").exists());
        assert!(dst_root.join("procedural/pinned.jsonl").exists());
        // Excluded subdirs are absent.
        assert!(!dst_root.join("logs").exists());
        assert!(!dst_root.join("models").exists());
        // Bytes survived.
        let conf = std::fs::read(dst_root.join("config.toml")).unwrap();
        assert_eq!(conf, b"max_size_gb = 10");
    }

    #[test]
    fn restore_refuses_to_clobber_without_force() {
        let src_tmp = fixture_with_data();
        let src_root = src_tmp.path().join("mneme");
        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        super::super::backup::backup_at(&src_root, &archive, false).unwrap();

        let dst_tmp = TempDir::new().unwrap();
        let dst_root = dst_tmp.path().join("mneme");
        write(&dst_root.join("existing.txt"), b"don't clobber me");

        match restore_at(&archive, &dst_root, false) {
            Err(MnemeError::Config(msg)) => assert!(msg.contains("non-empty")),
            other => panic!("expected Config error, got {other:?}"),
        }
        // existing.txt still there.
        assert!(dst_root.join("existing.txt").exists());
    }

    #[test]
    fn restore_force_overwrites() {
        let src_tmp = fixture_with_data();
        let src_root = src_tmp.path().join("mneme");
        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        super::super::backup::backup_at(&src_root, &archive, false).unwrap();

        let dst_tmp = TempDir::new().unwrap();
        let dst_root = dst_tmp.path().join("mneme");
        write(&dst_root.join("existing.txt"), b"clobber me");

        restore_at(&archive, &dst_root, true).unwrap();
        assert!(dst_root.join("config.toml").exists());
    }

    #[test]
    fn restore_refuses_when_lockfile_present() {
        let src_tmp = fixture_with_data();
        let src_root = src_tmp.path().join("mneme");
        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        super::super::backup::backup_at(&src_root, &archive, false).unwrap();

        let dst_tmp = TempDir::new().unwrap();
        let dst_root = dst_tmp.path().join("mneme");
        write(&dst_root.join(".lock"), b"12345");

        match restore_at(&archive, &dst_root, true) {
            Err(MnemeError::Lock(msg)) => {
                assert!(msg.contains("running mneme"));
            }
            other => panic!("expected Lock error, got {other:?}"),
        }
    }

    #[test]
    fn restore_missing_archive_is_clear_error() {
        let dst_tmp = TempDir::new().unwrap();
        let dst_root = dst_tmp.path().join("mneme");
        let nope = std::path::PathBuf::from("/does/not/exist/backup.tar.gz");
        match restore_at(&nope, &dst_root, false) {
            Err(MnemeError::Config(msg)) => assert!(msg.contains("does not exist")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
