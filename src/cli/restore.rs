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
    // Canonical root for the containment checks below. `root` was just
    // created, so this resolves.
    let canon_root = std::fs::canonicalize(root)?;
    let f = File::open(input)?;
    let gz = GzDecoder::new(f);
    let mut tar = tar::Archive::new(gz);
    // Don't trust archive paths. We reject entry *names* that are
    // absolute or contain `..`, and — because `Entry::unpack` follows a
    // pre-existing symlink at the destination — we also refuse to unpack
    // *through* any symlink an earlier entry may have planted that
    // resolves outside the data dir (the classic "symlink then
    // write-through" tar escape, CWE-22). `Entry::unpack` on its own does
    // NOT enforce containment; only the checks in this loop do.
    let mut count = 0usize;
    for entry in tar.entries()? {
        let mut entry = entry?;
        // Own the path so the immutable borrow of `entry` ends here,
        // leaving `entry` free for the `unpack` mutable borrow below.
        let rel = entry.path()?.into_owned();
        if rel.is_absolute() || rel.components().any(|c| c.as_os_str() == "..") {
            return Err(MnemeError::Config(format!(
                "archive entry {} has an unsafe path; refusing to restore",
                rel.display()
            )));
        }
        let dest = root.join(&rel);

        // The nearest already-existing ancestor of `dest` must resolve
        // (symlinks included) to a location inside the data dir. If an
        // earlier entry planted a symlink pointing outside `root`, a
        // child written through it canonicalizes outside `canon_root` —
        // refuse rather than escape.
        let mut ancestor = dest.parent();
        while let Some(a) = ancestor {
            if a.exists() {
                let canon = std::fs::canonicalize(a)?;
                if !canon.starts_with(&canon_root) {
                    return Err(MnemeError::Config(format!(
                        "archive entry {} resolves outside the data directory \
                         (via a symlink); refusing to restore",
                        rel.display()
                    )));
                }
                break;
            }
            ancestor = a.parent();
        }

        // Never write *onto* an existing symlink/hardlink: unlink it so
        // `unpack` writes a fresh inode instead of following the link to
        // a target outside `root`. A well-formed backup never lists the
        // same path twice, so this only fires on `--force` re-restores
        // and crafted archives.
        if let Ok(meta) = std::fs::symlink_metadata(&dest)
            && !meta.is_dir()
        {
            let _ = std::fs::remove_file(&dest);
        }

        // `Entry::unpack` handles dir/file/symlink polymorphism.
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
    // "Has data" ≡ "any non-hidden, non-`.lock`, non-runtime entry".
    // We skip transient artifacts the binary creates on its own rather
    // than user data:
    //   - `.lock` — refuse_if_locked already handled it; tolerating a
    //     stale one here keeps the check robust.
    //   - `logs` / `run` — the file-logger creates `logs/` at process
    //     startup (in `main::init_tracing`, before this check runs) and
    //     the daemon creates `run/`; neither is part of a backup. Without
    //     skipping them, a `mneme restore` into an otherwise-fresh root
    //     would always trip the non-empty guard just because the running
    //     binary logged a line.
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s == ".lock" || s == "logs" || s == "run" || s.starts_with('.') {
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
    fn restore_into_root_with_only_logs_succeeds() {
        // Regression: the file-logger creates `<root>/logs/` at process
        // startup, before restore's empty-check. A fresh restore must not
        // be blocked by the binary's own logging (or daemon `run/`) dir.
        let src_tmp = fixture_with_data();
        let src_root = src_tmp.path().join("mneme");
        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        super::super::backup::backup_at(&src_root, &archive, false).unwrap();

        let dst_tmp = TempDir::new().unwrap();
        let dst_root = dst_tmp.path().join("mneme");
        write(&dst_root.join("logs/mneme.log"), b"startup chatter");
        std::fs::create_dir_all(dst_root.join("run")).unwrap();

        // No --force, yet it must proceed and land the data.
        restore_at(&archive, &dst_root, false).unwrap();
        assert!(dst_root.join("procedural/pinned.jsonl").exists());
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

    /// Security regression: a crafted archive that plants a symlink
    /// pointing outside the data dir, then writes a file *through* it,
    /// must be refused — and must not write anything outside `root`.
    /// Guards the CWE-22 symlink-escape fix in `restore_at`.
    #[cfg(unix)]
    #[test]
    fn restore_refuses_symlink_escape() {
        // The "victim" location an attacker wants to write into, well
        // outside the restore root.
        let victim = TempDir::new().unwrap();
        let victim_dir = victim.path().to_path_buf();

        // Build the malicious .tar.gz in memory:
        //   entry 1: symlink  `pwn`      -> <victim_dir>  (absolute target)
        //   entry 2: regular  `pwn/loot` -> attacker bytes
        let mut buf: Vec<u8> = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);

            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            link.set_mode(0o777);
            builder.append_link(&mut link, "pwn", &victim_dir).unwrap();

            let loot = b"pwned";
            let mut fh = tar::Header::new_gnu();
            fh.set_entry_type(tar::EntryType::Regular);
            fh.set_size(loot.len() as u64);
            fh.set_mode(0o644);
            builder.append_data(&mut fh, "pwn/loot", &loot[..]).unwrap();

            let enc = builder.into_inner().unwrap();
            enc.finish().unwrap();
        }

        let archive_dir = TempDir::new().unwrap();
        let archive = archive_dir.path().join("evil.tar.gz");
        std::fs::write(&archive, &buf).unwrap();

        let dst = TempDir::new().unwrap();
        let root = dst.path().join("mneme");

        let result = restore_at(&archive, &root, false);
        assert!(result.is_err(), "symlink-escape archive must be refused");
        // The crucial invariant: nothing was written into the victim dir.
        assert!(
            !victim_dir.join("loot").exists(),
            "restore wrote through a symlink into {}",
            victim_dir.display()
        );
    }
}
