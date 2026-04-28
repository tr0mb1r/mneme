//! `mneme backup <path>` — Phase 6 §11.2 deliverable.
//!
//! Tar+gzip the entire `<root>/` tree to `<path>`. Excludes the
//! embedding-model cache (`<root>/models/`, can be re-downloaded)
//! and rotating log files (`<root>/logs/`) so the backup stays
//! small and focused on irreplaceable data.
//!
//! Refuses to run while a `mneme run` instance holds the lockfile —
//! taking a snapshot of an in-flight WAL/HNSW state could capture
//! a torn write. The user gets a clear error and can stop the
//! server first.

use std::fs::File;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::storage::layout;
use crate::{MnemeError, Result};

/// `walk_dir` classifies each entry so [`backup_at`] picks the right
/// tar emitter. `Symlink` is split out from `File` because
/// [`File::open`] would follow the link — a symlink-to-directory then
/// returns `EISDIR` and crashes the backup. The symlink branch goes
/// through `append_path_with_name`, which under `follow_symlinks(false)`
/// emits a tar link entry without ever opening the target.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum EntryKind {
    Dir,
    File,
    Symlink,
}

/// Subdirectories deliberately excluded from backups. `models/` is
/// large and re-fetchable from upstream; `logs/` is operational
/// chatter, not user data.
const EXCLUDED_SUBDIRS: &[&str] = &["models", "logs"];

pub fn execute(output: PathBuf, include_models: bool) -> Result<()> {
    let root = layout::default_root()
        .ok_or_else(|| MnemeError::Config("could not resolve ~/.mneme".into()))?;
    backup_at(&root, &output, include_models)
}

/// Library entry point. Tests call this directly with a tempdir
/// root; the CLI wraps it with the default home-dir lookup.
pub fn backup_at(root: &Path, output: &Path, include_models: bool) -> Result<()> {
    if !root.exists() {
        return Err(MnemeError::Config(format!(
            "data directory {} does not exist; nothing to back up",
            root.display()
        )));
    }
    refuse_if_locked(root)?;

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(output)?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(gz);
    // Don't follow symlinks — store them as links. Avoids accidental
    // duplication if the user has symlinked anything inside ~/.mneme.
    tar.follow_symlinks(false);

    let mut included = 0usize;
    for entry in walk_dir(root)? {
        let abs = entry.path;
        let rel = abs
            .strip_prefix(root)
            .map_err(|e| MnemeError::Config(format!("path {abs:?} not under {root:?}: {e}")))?;
        if !include_models && is_excluded(rel) {
            continue;
        }
        match entry.kind {
            EntryKind::Dir => tar.append_dir(rel, &abs)?,
            EntryKind::File => {
                let mut f = File::open(&abs)?;
                tar.append_file(rel, &mut f)?;
            }
            // Symlinks: read the target via metadata-aware path
            // append. Under `follow_symlinks(false)` the tar crate
            // writes a link entry pointing at the original target —
            // it never opens the target itself, so a symlinked
            // directory no longer triggers EISDIR.
            EntryKind::Symlink => tar.append_path_with_name(&abs, rel)?,
        }
        included += 1;
    }
    let gz = tar
        .into_inner()
        .map_err(|e| MnemeError::Storage(format!("close tar: {e}")))?;
    gz.finish()
        .map_err(|e| MnemeError::Storage(format!("close gzip: {e}")))?;
    eprintln!(
        "wrote {included} entries to {} ({} bytes)",
        output.display(),
        std::fs::metadata(output).map(|m| m.len()).unwrap_or(0)
    );
    Ok(())
}

fn refuse_if_locked(root: &Path) -> Result<()> {
    let lock_path = root.join(".lock");
    if lock_path.exists() {
        return Err(MnemeError::Lock(format!(
            "{} is held — stop the running mneme instance before backing up",
            lock_path.display()
        )));
    }
    Ok(())
}

fn is_excluded(rel: &Path) -> bool {
    rel.components().next().is_some_and(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| EXCLUDED_SUBDIRS.contains(&s))
    })
}

struct Entry {
    path: PathBuf,
    kind: EntryKind,
}

/// Depth-first walk. Returns directories before their contents so
/// `tar.append_dir` lands at the right spot. Symlinks are emitted as
/// `EntryKind::Symlink` and never followed, even when they point at
/// directories — that's how a `models -> ~/.mneme/models` link gets
/// preserved without recursing into the target tree.
fn walk_dir(root: &Path) -> Result<Vec<Entry>> {
    let mut out: Vec<Entry> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Only emit subdirectories — skip the root itself so the
        // archive paths are relative.
        if dir != root {
            out.push(Entry {
                path: dir.clone(),
                kind: EntryKind::Dir,
            });
        }
        let mut children: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        // Stable order — sorted ascending — so backups are
        // reproducible across runs at the same DB state.
        children.sort();
        for child in children {
            let meta = std::fs::symlink_metadata(&child)?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                // Don't recurse: symlinks are stored as-is.
                out.push(Entry {
                    path: child,
                    kind: EntryKind::Symlink,
                });
            } else if ft.is_dir() {
                stack.push(child);
            } else if ft.is_file() {
                out.push(Entry {
                    path: child,
                    kind: EntryKind::File,
                });
            } else {
                tracing::warn!(
                    path = %child.display(),
                    "skipping unsupported file type during backup walk"
                );
            }
        }
    }
    // Pop order gives us depth-first reverse; reverse so directories
    // come before their children inside the archive.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn backup_excludes_models_and_logs_by_default() {
        let tmp_root = TempDir::new().unwrap();
        let root = tmp_root.path().join("mneme");
        write(&root.join("config.toml"), "x = 1");
        write(&root.join("episodic/wal/wal-0000.log"), "wal");
        write(&root.join("models/big-blob.bin"), "ignore me");
        write(&root.join("logs/mneme.log"), "ignore me too");

        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        backup_at(&root, &archive, false).unwrap();

        // Read back the archive and confirm contents.
        let f = std::fs::File::open(&archive).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut tar = tar::Archive::new(gz);
        let entries: Vec<String> = tar
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().display().to_string())
            .collect();
        assert!(entries.iter().any(|p| p.ends_with("config.toml")));
        assert!(entries.iter().any(|p| p.contains("episodic/wal")));
        assert!(
            !entries.iter().any(|p| p.starts_with("models")),
            "models/ must be excluded"
        );
        assert!(
            !entries.iter().any(|p| p.starts_with("logs")),
            "logs/ must be excluded"
        );
    }

    #[test]
    fn backup_with_include_models_packs_them() {
        let tmp_root = TempDir::new().unwrap();
        let root = tmp_root.path().join("mneme");
        write(&root.join("models/big.bin"), "weights");
        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        backup_at(&root, &archive, true).unwrap();

        let f = std::fs::File::open(&archive).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut tar = tar::Archive::new(gz);
        assert!(
            tar.entries().unwrap().filter_map(|e| e.ok()).any(|e| e
                .path()
                .unwrap()
                .display()
                .to_string()
                .contains("models"))
        );
    }

    #[test]
    fn backup_refuses_when_lockfile_present() {
        let tmp_root = TempDir::new().unwrap();
        let root = tmp_root.path().join("mneme");
        write(&root.join("config.toml"), "x = 1");
        write(&root.join(".lock"), "12345");
        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");
        match backup_at(&root, &archive, false) {
            Err(MnemeError::Lock(msg)) => {
                assert!(msg.contains("running mneme"));
            }
            other => panic!("expected Lock error, got {other:?}"),
        }
        // The output archive should not exist (or be empty).
        assert!(!archive.exists() || std::fs::metadata(&archive).unwrap().len() == 0);
    }

    #[test]
    fn backup_with_symlinked_subdir_does_not_eisdir() {
        // Regression for the EISDIR bug surfaced by the manual test
        // script's --reuse-models path: walk_dir used to flag a
        // symlink-to-dir as is_dir=false, then File::open followed
        // the link and hit EISDIR. The fix routes symlinks through
        // tar::Builder::append_path_with_name so they're stored as
        // tar link entries.
        let tmp_root = TempDir::new().unwrap();
        let root = tmp_root.path().join("mneme");
        write(&root.join("config.toml"), "x = 1");

        // Create a real dir and a symlink-to-dir inside the root.
        let target_dir = tmp_root.path().join("real-models");
        std::fs::create_dir_all(target_dir.join("subdir")).unwrap();
        std::fs::write(target_dir.join("weight.bin"), b"data").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_dir, root.join("models")).unwrap();
        #[cfg(windows)]
        {
            // Windows symlinks need elevated perms; skip on that
            // platform until we can rely on developer-mode being on.
            return;
        }

        let out = TempDir::new().unwrap();
        let archive = out.path().join("backup.tar.gz");

        // --include-models is the case where this used to blow up.
        backup_at(&root, &archive, true).expect("backup must not EISDIR on symlinked subdir");

        // Confirm the archive holds a Symlink entry for `models` and
        // does NOT inline the target tree (no `models/weight.bin`).
        let f = std::fs::File::open(&archive).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut tar = tar::Archive::new(gz);
        let mut saw_symlink = false;
        let mut saw_inlined_target = false;
        for entry in tar.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            if (path == "models" || path.trim_end_matches('/') == "models")
                && entry.header().entry_type().is_symlink()
            {
                saw_symlink = true;
            }
            if path.starts_with("models/") {
                saw_inlined_target = true;
            }
        }
        assert!(saw_symlink, "models must appear as a Symlink tar entry");
        assert!(
            !saw_inlined_target,
            "symlink target tree must not be inlined into the archive"
        );
    }

    #[test]
    fn backup_then_restore_preserves_symlink() {
        let tmp_root = TempDir::new().unwrap();
        let root = tmp_root.path().join("mneme");
        write(&root.join("config.toml"), "x = 1");
        let target_dir = tmp_root.path().join("link-target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("contents.bin"), b"hi").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_dir, root.join("models")).unwrap();
        #[cfg(windows)]
        {
            return;
        }

        let archive_dir = TempDir::new().unwrap();
        let archive = archive_dir.path().join("snap.tar.gz");
        backup_at(&root, &archive, true).unwrap();

        // Restore into a fresh root and confirm `models` came back
        // as a symlink (not as a copied directory).
        let restored_root = tmp_root.path().join("restored");
        std::fs::create_dir_all(&restored_root).unwrap();
        crate::cli::restore::restore_at(&archive, &restored_root, false).unwrap();
        let restored_models = restored_root.join("models");
        let meta = std::fs::symlink_metadata(&restored_models).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "restored models must still be a symlink, got {:?}",
            meta.file_type()
        );
    }

    #[test]
    fn backup_missing_root_is_clear_error() {
        let out = TempDir::new().unwrap();
        let nope = std::path::PathBuf::from("/does/not/exist/mneme-root");
        let archive = out.path().join("backup.tar.gz");
        match backup_at(&nope, &archive, false) {
            Err(MnemeError::Config(msg)) => assert!(msg.contains("does not exist")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
