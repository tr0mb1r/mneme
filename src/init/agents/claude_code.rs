//! Claude Code per-agent install (release-planning v2.1 §4.4).
//!
//! Reference implementation for the B-track per-agent installers
//! — every Tier-1 integration that lands in B.M3-M4 follows this
//! same shape, swapping the per-agent paths and any agent-specific
//! settings keys.
//!
//! Layout (global `~/.claude/` install only — project mode is a
//! deferred follow-up):
//!
//! - `~/.claude/settings.json` — `mcpServers.mneme` entry +
//!   `hooks.{SessionStart,PreCompact,Stop}` block wired to the
//!   shipped scripts. Surrounding settings keys preserved verbatim
//!   via `init::json_config`.
//! - `~/.claude/MNEME.md` — written from
//!   `init::assets::MNEME_MD_TEMPLATE`. Mneme owns this file
//!   (`mneme init claude-code --upgrade` overwrites; `--uninstall`
//!   removes).
//! - `~/.claude/CLAUDE.md` — marker block (`<!-- mneme:begin -->
//!   ... <!-- mneme:end -->`) upserted with the reference line.
//!   Surrounding user content preserved verbatim via
//!   `init::marker`.
//! - `~/.claude/hooks/mneme/{session-start,precompact,stop}.sh` —
//!   written executable from `init::assets::CLAUDE_CODE_HOOKS`.
//!
//! All file writes go through atomic helpers (`init::assets`,
//! `init::json_config::upsert_file`) so a crashed install never
//! half-clobbers the user's settings.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{AgentError, InstallMode};
use crate::init::assets::{self, CLAUDE_CODE_HOOKS, MNEME_MD_TEMPLATE};
use crate::init::json_config;
use crate::init::marker;

/// Reference line written inside the marker block in CLAUDE.md.
/// Short by design — the agent should follow the link to MNEME.md
/// for guidance, not read substantive instructions inside the
/// marker.
const MARKER_BODY: &str = "See @MNEME.md for memory instructions (managed by mneme).";

/// Hook event name → script filename, in install order. Matches
/// the names Claude Code's settings.json schema expects.
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start.sh"),
    ("PreCompact", "precompact.sh"),
    ("Stop", "stop.sh"),
];

/// Resolved set of paths under the agent's `~/.claude/` directory.
/// Pulled out so tests can override `home_dir` with a tempdir
/// and so `--show` can print the plan without writing.
struct Paths {
    settings: PathBuf,
    mneme_md: PathBuf,
    claude_md: PathBuf,
    hooks_dir: PathBuf,
}

impl Paths {
    fn under(home: &Path) -> Self {
        let claude = home.join(".claude");
        Self {
            settings: claude.join("settings.json"),
            mneme_md: claude.join("MNEME.md"),
            claude_md: claude.join("CLAUDE.md"),
            hooks_dir: claude.join("hooks").join("mneme"),
        }
    }
}

pub fn run(mode: InstallMode, home_dir: &Path) -> Result<(), AgentError> {
    let paths = Paths::under(home_dir);
    match mode {
        InstallMode::Install | InstallMode::Upgrade => install(&paths),
        InstallMode::Uninstall => uninstall(&paths),
        InstallMode::Show => {
            print_plan(&paths);
            Ok(())
        }
    }
}

fn install(paths: &Paths) -> Result<(), AgentError> {
    // 1. Write the hook scripts (executable, mode 0755 on Unix).
    //    Done first so the settings.json hook entries we add in
    //    step 4 reference scripts that already exist on disk.
    for (filename, contents) in CLAUDE_CODE_HOOKS {
        let dest = paths.hooks_dir.join(filename);
        assets::write_executable(&dest, contents)?;
    }

    // 2. Write MNEME.md from the embedded template.
    assets::write_text(&paths.mneme_md, MNEME_MD_TEMPLATE)?;

    // 3. Upsert the marker block in CLAUDE.md (creates the file if
    //    absent). Per §4.3 mneme owns the marker block; user
    //    content outside it is preserved verbatim by `marker`.
    let claude_md_existing = match std::fs::read_to_string(&paths.claude_md) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let claude_md_new = marker::upsert_block(&claude_md_existing, MARKER_BODY)?;
    assets::write_text(&paths.claude_md, &claude_md_new)?;

    // 4. settings.json: add mcpServers.mneme + hooks.<event>
    //    entries. Single upsert so atomic — the user's existing
    //    settings file is read once, transformed, and written back.
    //    `args=["client"]` not `["run"]` so each Claude Code session
    //    spawns a thin stdio↔unix-socket bridge to the long-running
    //    `mneme daemon`. Multiple Claude Code sessions can run
    //    concurrently this way (the lockfile contention that bites
    //    `args=["run"]` only allows one stdio session per data dir).
    //    Token stays at `~/.mneme/run/auth.token`; the wrapper reads
    //    it at spawn — never embedded in this settings file.
    json_config::upsert_file(&paths.settings, |value| {
        json_config::set_path(
            value,
            &["mcpServers", "mneme"],
            json!({
                "command": "mneme",
                "args": ["client"],
            }),
        )?;
        for (event, filename) in HOOK_EVENTS {
            let script = paths.hooks_dir.join(filename);
            json_config::set_path(
                value,
                &["hooks", event],
                json!([{
                    "hooks": [{
                        "type": "command",
                        "command": script.display().to_string(),
                    }]
                }]),
            )?;
        }
        Ok(())
    })?;

    print_post_install(paths);
    Ok(())
}

/// First-run prompt (release-planning v2.1 §7.2 / D.M1). The
/// post-install message is what the user sees after a successful
/// install; per §7.2 the design goal is "every new user
/// experiences memory recall within their first 5 minutes" — so
/// the message names what was wired, the next concrete action
/// (restart Claude Code), a 2-message conversation that
/// demonstrates recall across sessions, and the verification
/// command. Kept on stderr so it doesn't pollute scripted
/// pipelines that consume stdout.
fn print_post_install(paths: &Paths) {
    let claude_dir = paths
        .settings
        .parent()
        .map(Path::display)
        .map_or_else(|| "~/.claude".to_owned(), |d| d.to_string());
    eprintln!();
    eprintln!("✓ mneme installed for Claude Code at {claude_dir}");
    eprintln!();
    eprintln!("  Wired up:");
    eprintln!(
        "    • MNEME.md (your agent's memory guide)            {}",
        paths.mneme_md.display()
    );
    eprintln!(
        "    • mcpServers.mneme entry in settings.json         {}",
        paths.settings.display()
    );
    eprintln!(
        "    • Lifecycle hooks (SessionStart/PreCompact/Stop)  {}/",
        paths.hooks_dir.display()
    );
    eprintln!(
        "    • Marker block in CLAUDE.md (your existing content preserved)  {}",
        paths.claude_md.display()
    );
    eprintln!();
    eprintln!("  Architecture: Claude Code now spawns `mneme client` per session,");
    eprintln!("    which connects to a long-running `mneme daemon` over a unix");
    eprintln!("    socket. Multiple Claude Code sessions can run concurrently.");
    eprintln!("    The auth token at `~/.mneme/run/auth.token` (mode 0600) stays");
    eprintln!("    out of settings.json — the wrapper reads it at spawn.");
    eprintln!();
    eprintln!("  Next steps:");
    eprintln!();
    eprintln!("    1. Start the daemon in a long-lived terminal");
    eprintln!("       (or background it with launchd/systemd later):");
    eprintln!();
    eprintln!("         mneme daemon");
    eprintln!();
    eprintln!("    2. Restart Claude Code so it picks up the new MCP server.");
    eprintln!();
    eprintln!("    3. Try this conversation to confirm memory works end-to-end:");
    eprintln!();
    eprintln!("       You: \"Remember that I prefer Vim keybindings.\"");
    eprintln!("       (then quit and start a fresh Claude Code session)");
    eprintln!("       You: \"What editor do I prefer?\"");
    eprintln!();
    eprintln!("       Claude should recall the preference via mneme.recall.");
    eprintln!();
    eprintln!("    4. To verify storage outside Claude Code:");
    eprintln!("         mneme stats");
    eprintln!();
    eprintln!("  If `mneme client` errors with `daemon socket: No such file`,");
    eprintln!("  the daemon isn't running — start it with `mneme daemon`.");
    eprintln!();
    eprintln!("  Rotate the auth token anytime with:  mneme auth rotate");
    eprintln!("  (no settings.json rewrite needed — the wrapper re-reads on");
    eprintln!("  every spawn.)");
    eprintln!();
    eprintln!("  Reverse anytime with:  mneme init claude-code --uninstall");
    eprintln!();
}

fn uninstall(paths: &Paths) -> Result<(), AgentError> {
    // Reverse the install order: settings.json edits first (so a
    // partial uninstall leaves the agent in a clean state pointing
    // at no-longer-existent scripts only briefly), then file
    // removals.
    if paths.settings.exists() {
        json_config::upsert_file(&paths.settings, |value| {
            json_config::remove_path(value, &["mcpServers", "mneme"])?;
            for (event, _) in HOOK_EVENTS {
                json_config::remove_path(value, &["hooks", event])?;
            }
            // Don't leave empty `mcpServers: {}` / `hooks: {}`
            // blocks behind — clean up containers that go empty.
            prune_empty_object(value, &["mcpServers"]);
            prune_empty_object(value, &["hooks"]);
            Ok(())
        })?;
    }

    // Remove MNEME.md and the marker block. Idempotent: missing
    // files / absent block are not errors.
    if paths.mneme_md.exists() {
        std::fs::remove_file(&paths.mneme_md)?;
    }
    if paths.claude_md.exists() {
        let existing = std::fs::read_to_string(&paths.claude_md)?;
        let cleaned = marker::remove_block(&existing)?;
        // If the file is now empty (block was the only content),
        // remove the file entirely. Otherwise rewrite.
        if cleaned.is_empty() {
            std::fs::remove_file(&paths.claude_md)?;
        } else if cleaned != existing {
            assets::write_text(&paths.claude_md, &cleaned)?;
        }
    }

    // Remove the per-mneme hooks directory. Use remove_dir_all so
    // partial state from a crashed install gets cleaned too. Then
    // best-effort prune the parent ~/.claude/hooks/ if it's empty
    // (we created it on install only because mneme/ went underneath
    // it; if the user had no other hooks, the empty parent would be
    // a leftover).
    if paths.hooks_dir.exists() {
        std::fs::remove_dir_all(&paths.hooks_dir)?;
    }
    if let Some(hooks_parent) = paths.hooks_dir.parent()
        && hooks_parent.exists()
        && std::fs::read_dir(hooks_parent)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(hooks_parent);
    }

    eprintln!();
    eprintln!("✓ mneme uninstalled from Claude Code");
    eprintln!();
    eprintln!("  Removed mneme's mcpServers entry, lifecycle hooks, MNEME.md, and the");
    eprintln!("  marker block from CLAUDE.md. Your settings.json keeps every other");
    eprintln!("  entry; CLAUDE.md keeps every line outside the marker.");
    eprintln!();
    eprintln!("  Restart Claude Code so the MCP server entry drops out of its session.");
    eprintln!();
    eprintln!("  Re-install anytime with:  mneme init claude-code");
    eprintln!();
    Ok(())
}

fn prune_empty_object(value: &mut serde_json::Value, segments: &[&str]) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if segments.len() == 1
        && let Some(serde_json::Value::Object(child)) = map.get(segments[0])
        && child.is_empty()
    {
        map.remove(segments[0]);
    }
}

fn print_plan(paths: &Paths) {
    println!("`mneme init claude-code` would write:");
    println!();
    for (filename, _) in CLAUDE_CODE_HOOKS {
        println!("  {} (mode 0755)", paths.hooks_dir.join(filename).display());
    }
    println!("  {}", paths.mneme_md.display());
    println!(
        "  {}  [marker block upserted, surrounding content preserved]",
        paths.claude_md.display()
    );
    println!(
        "  {}  [mcpServers.mneme + hooks block, other keys preserved]",
        paths.settings.display()
    );
    println!();
    println!("All writes are atomic (tmpfile + fsync + rename).");
    println!("Re-run with --uninstall to reverse.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    fn fresh_home() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn install_creates_all_artifacts_under_dot_claude() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let paths = Paths::under(home.path());

        // All three hook scripts present + executable on Unix.
        for (filename, _) in CLAUDE_CODE_HOOKS {
            let p = paths.hooks_dir.join(filename);
            assert!(p.exists(), "hook {filename} missing");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o755, "hook {filename} should be 0o755, got {mode:o}");
            }
        }

        // MNEME.md content matches the embedded template.
        let mneme_md = std::fs::read_to_string(&paths.mneme_md).unwrap();
        assert_eq!(mneme_md, MNEME_MD_TEMPLATE);

        // CLAUDE.md has the marker block with the reference line.
        let claude_md = std::fs::read_to_string(&paths.claude_md).unwrap();
        assert!(claude_md.contains("<!-- mneme:begin -->"));
        assert!(claude_md.contains(MARKER_BODY));
        assert!(claude_md.contains("<!-- mneme:end -->"));

        // settings.json has mcpServers.mneme + all three hook events.
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.settings).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["mneme"]["command"], "mneme");
        assert_eq!(settings["mcpServers"]["mneme"]["args"], json!(["client"]));
        for (event, _) in HOOK_EVENTS {
            assert!(
                settings["hooks"][event].is_array(),
                "hooks.{event} must be an array, got {:?}",
                settings["hooks"][event]
            );
        }
    }

    #[test]
    fn install_preserves_existing_settings_keys() {
        let home = fresh_home();
        let paths = Paths::under(home.path());
        std::fs::create_dir_all(paths.settings.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.settings,
            r#"{
  "mcpServers": {
    "other-server": {"command": "elsewhere"}
  },
  "unrelated_top_level": 42,
  "hooks": {
    "PreToolUse": [{"hooks": [{"type": "command", "command": "user-script.sh"}]}]
  }
}
"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.settings).unwrap()).unwrap();
        // User's other-server preserved.
        assert_eq!(
            settings["mcpServers"]["other-server"]["command"],
            "elsewhere"
        );
        // User's PreToolUse hook preserved.
        assert!(settings["hooks"]["PreToolUse"].is_array());
        // User's top-level key preserved.
        assert_eq!(settings["unrelated_top_level"], 42);
        // mneme entry added.
        assert_eq!(settings["mcpServers"]["mneme"]["command"], "mneme");
    }

    #[test]
    fn install_preserves_existing_claude_md_around_marker() {
        let home = fresh_home();
        let paths = Paths::under(home.path());
        std::fs::create_dir_all(paths.claude_md.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.claude_md,
            "# Project guide\n\nMy stuff up top.\n\n## My section\n\nMore content.\n",
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();

        let claude_md = std::fs::read_to_string(&paths.claude_md).unwrap();
        assert!(claude_md.starts_with("# Project guide"));
        assert!(claude_md.contains("My stuff up top."));
        assert!(claude_md.contains("My section"));
        assert!(claude_md.contains(MARKER_BODY));
    }

    #[test]
    fn install_is_idempotent() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let snapshot = capture_snapshot(home.path());
        run(InstallMode::Install, home.path()).unwrap();
        let after = capture_snapshot(home.path());
        assert_eq!(snapshot, after, "second install must be byte-identical");
    }

    #[test]
    fn upgrade_overwrites_mneme_md_and_marker_block() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let paths = Paths::under(home.path());

        // User edits MNEME.md (which mneme owns) — upgrade
        // overwrites it back per §4.3 ownership model.
        std::fs::write(&paths.mneme_md, "user tampered with this").unwrap();

        run(InstallMode::Upgrade, home.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.mneme_md).unwrap(),
            MNEME_MD_TEMPLATE
        );
    }

    #[test]
    fn uninstall_removes_all_artifacts() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        let paths = Paths::under(home.path());

        for (filename, _) in CLAUDE_CODE_HOOKS {
            assert!(
                !paths.hooks_dir.join(filename).exists(),
                "hook should be removed"
            );
        }
        assert!(!paths.mneme_md.exists(), "MNEME.md should be removed");
        // hooks_dir removed (was only used by mneme).
        assert!(
            !paths.hooks_dir.exists(),
            "~/.claude/hooks/mneme/ should be removed"
        );
        // CLAUDE.md was created with only the marker block, so
        // uninstall removes it entirely.
        assert!(
            !paths.claude_md.exists(),
            "CLAUDE.md was marker-only; should be removed"
        );

        // settings.json: mneme entries removed, file may not exist
        // if it had nothing else (it had only the things we put
        // there during install).
        if paths.settings.exists() {
            let settings: Value =
                serde_json::from_str(&std::fs::read_to_string(&paths.settings).unwrap()).unwrap();
            assert!(
                settings.get("mcpServers").is_none()
                    || settings["mcpServers"].get("mneme").is_none()
            );
            for (event, _) in HOOK_EVENTS {
                if let Some(hooks) = settings.get("hooks") {
                    assert!(hooks.get(event).is_none(), "hooks.{event} should be gone");
                }
            }
        }
    }

    #[test]
    fn uninstall_preserves_user_content_around_marker_in_claude_md() {
        let home = fresh_home();
        let paths = Paths::under(home.path());
        std::fs::create_dir_all(paths.claude_md.parent().unwrap()).unwrap();
        std::fs::write(&paths.claude_md, "user content above\n").unwrap();

        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();

        // CLAUDE.md still exists with just the user's pre-existing
        // content — the marker block was removed cleanly.
        let claude_md = std::fs::read_to_string(&paths.claude_md).unwrap();
        assert_eq!(claude_md, "user content above\n");
    }

    #[test]
    fn uninstall_preserves_unrelated_settings_keys() {
        let home = fresh_home();
        let paths = Paths::under(home.path());
        std::fs::create_dir_all(paths.settings.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.settings,
            r#"{
  "mcpServers": {"other": {"command": "elsewhere"}},
  "hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "u.sh"}]}]},
  "extra": "preserved"
}
"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&paths.settings).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["other"]["command"], "elsewhere");
        assert!(settings["mcpServers"].get("mneme").is_none());
        assert!(settings["hooks"]["PreToolUse"].is_array());
        for (event, _) in HOOK_EVENTS {
            assert!(settings["hooks"].get(event).is_none());
        }
        assert_eq!(settings["extra"], "preserved");
    }

    #[test]
    fn uninstall_is_idempotent() {
        let home = fresh_home();
        // Uninstall on a fresh dir (nothing installed) — no error.
        run(InstallMode::Uninstall, home.path()).unwrap();
        // Install + uninstall + uninstall — second uninstall is no-op.
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
    }

    #[test]
    fn show_does_not_write_anything() {
        let home = fresh_home();
        run(InstallMode::Show, home.path()).unwrap();
        let paths = Paths::under(home.path());
        assert!(!paths.settings.exists());
        assert!(!paths.mneme_md.exists());
        assert!(!paths.claude_md.exists());
        assert!(!paths.hooks_dir.exists());
    }

    /// Capture the on-disk artifacts so byte-identical
    /// idempotency can be asserted across two installs.
    fn capture_snapshot(home: &Path) -> Vec<(String, Vec<u8>)> {
        let paths = Paths::under(home);
        let mut entries = Vec::new();
        for p in [&paths.settings, &paths.mneme_md, &paths.claude_md] {
            if p.exists() {
                entries.push((p.display().to_string(), std::fs::read(p).unwrap()));
            }
        }
        for (filename, _) in CLAUDE_CODE_HOOKS {
            let p = paths.hooks_dir.join(filename);
            if p.exists() {
                entries.push((p.display().to_string(), std::fs::read(&p).unwrap()));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
}
