//! Embedded install assets for `mneme init <agent>` and atomic
//! write helpers (release-planning v2.1 §4.5 + §4.4 hook
//! installation).
//!
//! Per-agent installers in B.M2-M5 share two file-write idioms:
//!
//! 1. **Plain text** (`MNEME.md`, marker-block updates to instruction
//!    files) — write atomically via tmpfile + rename so a crashed
//!    install doesn't half-clobber the user's file.
//!
//! 2. **Executable** (lifecycle hook scripts shipped to
//!    `~/.claude/hooks/mneme/` and equivalents) — same atomicity
//!    plus owner-executable mode (`0755`) on Unix. Windows hosts
//!    typically don't use POSIX hooks; those installers fall back
//!    to whatever per-agent convention applies.
//!
//! Embedded constants live here so per-agent code in B.M2+ stays
//! a thin orchestration layer — no `include_str!` calls scattered
//! across multiple modules. The actual content is sourced from
//! `templates/` and `docs/examples/` so the in-tree files remain
//! the single source of truth (no copy-paste drift between the
//! "documented example" and the "thing the binary writes").

use std::io;
use std::path::Path;

/// Mneme-managed `MNEME.md` content per release-planning §4.5 —
/// behavioural guidance for the agent (when to call `remember`,
/// `pin`, `record_event`, etc.). Emitted by `mneme init <agent>`
/// alongside the agent's primary instruction file. Versioned with
/// the binary; `mneme init <agent> --upgrade` rewrites with the
/// current binary's copy.
pub const MNEME_MD_TEMPLATE: &str = include_str!("../../templates/MNEME.md");

/// Claude Code lifecycle hook scripts (release-planning §4.4 +
/// `docs/CLAUDE_CODE_SETUP.md` §7). Ship as embedded `&str`s so
/// `mneme init claude-code` writes them to
/// `~/.claude/hooks/mneme/<name>.sh` in one atomic operation each
/// — no manual `cp` from `docs/examples/` like the v1.0
/// instructions required.
pub const CLAUDE_CODE_HOOK_SESSION_START: &str =
    include_str!("../../docs/examples/claude-code-hooks/session-start.sh");
pub const CLAUDE_CODE_HOOK_PRECOMPACT: &str =
    include_str!("../../docs/examples/claude-code-hooks/precompact.sh");
pub const CLAUDE_CODE_HOOK_STOP: &str =
    include_str!("../../docs/examples/claude-code-hooks/stop.sh");

/// Tabular form of the Claude Code hooks: `(filename, contents)`.
/// `mneme init claude-code` iterates this and writes each entry
/// to `~/.claude/hooks/mneme/<filename>` via [`write_executable`].
/// Filename is the bare script name; the per-agent installer
/// chooses the destination directory.
pub const CLAUDE_CODE_HOOKS: &[(&str, &str)] = &[
    ("session-start.sh", CLAUDE_CODE_HOOK_SESSION_START),
    ("precompact.sh", CLAUDE_CODE_HOOK_PRECOMPACT),
    ("stop.sh", CLAUDE_CODE_HOOK_STOP),
];

/// Atomically write `contents` to `path`. Creates parent
/// directories as needed (mode is the OS default — typically 0755
/// on Unix; per-agent installers should follow up with explicit
/// `chmod` if the convention requires tighter perms). Implementation
/// writes to `<path>.tmp`, fsyncs the file, then renames over
/// `<path>` so a crashed install never leaves a half-written file.
pub fn write_text(path: &Path, contents: &str) -> io::Result<()> {
    write_inner(path, contents.as_bytes(), /* executable */ false)
}

/// Atomically write `contents` to `path` with owner-executable
/// permission bit set on Unix (`0755`). Used for lifecycle hook
/// scripts that the agent's hook runner needs to be able to
/// execute. On Windows, falls back to plain-text semantics —
/// Windows POSIX-mode bits don't apply, and the named-pipe-based
/// hook conventions (TBD per agent) don't need executable bits.
pub fn write_executable(path: &Path, contents: &str) -> io::Result<()> {
    write_inner(path, contents.as_bytes(), /* executable */ true)
}

fn write_inner(path: &Path, contents: &[u8], executable: bool) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    // Stage the write at `<path>.tmp` so a crashed install never
    // leaves a half-written final file. The tmp suffix avoids the
    // edge case where two concurrent installs in the same dir
    // race on a shared `.tmp` filename — vanishingly unlikely in
    // practice (init is interactive, not concurrent), but cheap to
    // bound the surface area.
    let tmp = match path.file_name() {
        Some(name) => path.with_file_name(format!("{}.mneme-init-tmp", name.to_string_lossy())),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination path has no file name",
            ));
        }
    };
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    if executable {
        set_executable(&tmp)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    // Windows uses ACLs + the .exe extension as the executability
    // signal; POSIX mode bits don't apply. The hook scripts are
    // shipped as `.sh` and aren't directly invoked on Windows
    // anyway — the per-agent installer there picks a different
    // hook convention (TBD per agent in B.M2+).
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn embedded_mneme_md_template_has_expected_sections() {
        // The template content is sourced from templates/MNEME.md.
        // These assertions pin the structural invariants that
        // every per-agent installer's contract assumes — drift in
        // the template that drops a section would break the agent
        // guidance silently.
        assert!(MNEME_MD_TEMPLATE.contains("# Memory instructions (managed by mneme)"));
        assert!(MNEME_MD_TEMPLATE.contains("## When to remember (call `remember`)"));
        assert!(MNEME_MD_TEMPLATE.contains("## When NOT to remember"));
        assert!(MNEME_MD_TEMPLATE.contains("## Pinned rules (call `pin`)"));
        assert!(MNEME_MD_TEMPLATE.contains("## Recall"));
        assert!(MNEME_MD_TEMPLATE.contains("## Episodic events (call `record_event`)"));
    }

    #[test]
    fn embedded_claude_code_hooks_are_runnable_shebangs() {
        for (name, contents) in CLAUDE_CODE_HOOKS {
            assert!(
                contents.starts_with("#!/usr/bin/env bash"),
                "hook {name} must start with bash shebang"
            );
            assert!(
                contents.contains("set -euo pipefail"),
                "hook {name} must enable strict mode"
            );
        }
    }

    #[test]
    fn write_text_creates_file_and_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("a/b/c/file.txt");
        write_text(&dest, "hello").unwrap();
        let read = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(read, "hello");
        assert!(!dest.with_extension("txt.mneme-init-tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_executable_sets_owner_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("hooks/script.sh");
        write_executable(&dest, "#!/usr/bin/env bash\necho hi\n").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "executable script must be 0o755, got {mode:o}");
    }

    #[test]
    fn write_overwrites_existing_file_atomically() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("file.txt");
        write_text(&dest, "first").unwrap();
        write_text(&dest, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "second");
        // Tmp file must have been renamed away — no leftover.
        let leftover = tmp.path().join("file.txt.mneme-init-tmp");
        assert!(!leftover.exists(), "tmp must be renamed away post-write");
    }

    #[test]
    fn write_to_path_without_parent_uses_cwd() {
        // A bare filename (no parent dir) should still work via the
        // current working directory. Verifies we don't choke on
        // `path.parent() == Some("")`.
        let tmp = TempDir::new().unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = write_text(Path::new("bare.txt"), "x");
        std::env::set_current_dir(original_cwd).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("bare.txt")).unwrap(),
            "x"
        );
    }
}
