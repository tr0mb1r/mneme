//! Claude Desktop per-agent install (release-planning v2.1 §4.4).
//!
//! Claude Desktop differs from Claude Code in two ways relevant to
//! this installer:
//!
//! 1. **No hooks API.** Claude Desktop doesn't support
//!    SessionStart / PreCompact / Stop hook scripts. The lifecycle
//!    nudges that Claude Code's hooks deliver are unavailable here;
//!    the agent calibration story is "the user pastes guidance into
//!    the system prompt or the agent rediscovers via tool descriptions
//!    on first use."
//! 2. **No project-level instruction file.** Claude Desktop doesn't
//!    read CLAUDE.md or any equivalent per-project guidance file.
//!    The marker-block primitive isn't applicable.
//!
//! What this installer DOES do:
//!
//! - Resolves the platform-specific config path:
//!   - macOS:   `~/Library/Application Support/Claude/claude_desktop_config.json`
//!   - Linux:   `~/.config/Claude/claude_desktop_config.json`
//!   - Windows: `%APPDATA%/Claude/claude_desktop_config.json`
//!     (Windows path resolution lands when daemon Windows support
//!     does in M4 — for now Claude Desktop on Windows is `cfg`-gated
//!     to defer.)
//! - Adds (or updates) `mcpServers.mneme = {"command": "mneme",
//!   "args": ["client"]}` in that file via the existing
//!   `init::json_config::upsert_file` atomic helper. Surrounding
//!   keys (other `mcpServers`, top-level fields) preserved verbatim.
//! - Writes a copy of `MNEME.md` to `~/.mneme/MNEME.md` so users
//!   can paste its content into Claude Desktop's system-prompt
//!   field if they want — opt-in agent calibration since Claude
//!   Desktop has nowhere to auto-reference it.
//!
//! Uninstall reverses: removes `mcpServers.mneme`, prunes empty
//! `mcpServers: {}` if it was the only entry, deletes the
//! `~/.mneme/MNEME.md` copy. Idempotent.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{AgentError, InstallMode};
use crate::init::assets::{self, MNEME_MD_TEMPLATE};
use crate::init::json_config;

/// Platform-specific path to Claude Desktop's config JSON. Returns
/// `None` on Windows for now — Windows path resolution lands
/// alongside daemon Windows support in M4.
fn config_path(home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support/Claude/claude_desktop_config.json"))
    }
    #[cfg(target_os = "linux")]
    {
        Some(home.join(".config/Claude/claude_desktop_config.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// Path to the standalone MNEME.md copy users can paste from. Lives
/// under the data dir (not the agent's config dir) because Claude
/// Desktop has no convention for per-agent instruction files —
/// stashing it in `~/.mneme/MNEME.md` makes it findable from any
/// shell.
fn standalone_mneme_md(home: &Path) -> PathBuf {
    home.join(".mneme/MNEME.md")
}

pub fn run(mode: InstallMode, home_dir: &Path) -> Result<(), AgentError> {
    let Some(settings) = config_path(home_dir) else {
        return Err(AgentError::Generic(
            "Claude Desktop install path resolution for this OS is not yet \
             implemented; tracked alongside daemon Windows support in M4"
                .into(),
        ));
    };
    let mneme_md = standalone_mneme_md(home_dir);
    match mode {
        InstallMode::Install | InstallMode::Upgrade => install(&settings, &mneme_md),
        InstallMode::Uninstall => uninstall(&settings, &mneme_md),
        InstallMode::Show => {
            print_plan(&settings, &mneme_md);
            Ok(())
        }
    }
}

fn install(settings: &Path, mneme_md: &Path) -> Result<(), AgentError> {
    // `args=["client"]` so each Claude Desktop session spawns the
    // thin stdio↔unix-socket wrapper. Two Claude Desktop instances
    // (or Claude Desktop alongside Claude Code) all share one
    // `mneme daemon` instead of fighting over the lockfile that
    // `args=["run"]` would imply. Token stays at
    // `~/.mneme/run/auth.token`; the wrapper reads it at spawn —
    // never embedded in this config file.
    json_config::upsert_file(settings, |value| {
        json_config::set_path(
            value,
            &["mcpServers", "mneme"],
            json!({
                "command": "mneme",
                "args": ["client"],
            }),
        )?;
        Ok(())
    })?;
    assets::write_text(mneme_md, MNEME_MD_TEMPLATE)?;
    print_post_install(settings, mneme_md);
    Ok(())
}

fn uninstall(settings: &Path, mneme_md: &Path) -> Result<(), AgentError> {
    if settings.exists() {
        json_config::upsert_file(settings, |value| {
            json_config::remove_path(value, &["mcpServers", "mneme"])?;
            prune_empty_object(value, "mcpServers");
            Ok(())
        })?;
    }
    if mneme_md.exists() {
        std::fs::remove_file(mneme_md)?;
    }
    eprintln!();
    eprintln!("✓ mneme uninstalled from Claude Desktop");
    eprintln!();
    eprintln!(
        "  Removed mneme's mcpServers entry and the MNEME.md copy at {}.",
        mneme_md.display()
    );
    eprintln!("  Other entries in {} are preserved.", settings.display());
    eprintln!();
    eprintln!("  Quit and relaunch Claude Desktop so the dropped MCP server stops");
    eprintln!("  appearing in its session.");
    eprintln!();
    eprintln!("  Re-install anytime with:  mneme init claude-desktop");
    eprintln!();
    Ok(())
}

fn print_plan(settings: &Path, mneme_md: &Path) {
    println!("`mneme init claude-desktop` would write:");
    println!();
    println!(
        "  {}  [mcpServers.mneme entry, other keys preserved]",
        settings.display()
    );
    println!(
        "  {}  [standalone copy users paste from]",
        mneme_md.display()
    );
    println!();
    println!("All writes are atomic (tmpfile + fsync + rename).");
    println!("Re-run with --uninstall to reverse.");
}

fn print_post_install(settings: &Path, mneme_md: &Path) {
    eprintln!();
    eprintln!("✓ mneme installed for Claude Desktop");
    eprintln!();
    eprintln!("  Wired up:");
    eprintln!("    • mcpServers.mneme entry in  {}", settings.display());
    eprintln!("    • Standalone MNEME.md copy   {}", mneme_md.display());
    eprintln!();
    eprintln!("  Claude Desktop has no hooks API and no per-project instruction");
    eprintln!("  file — agent calibration is opt-in. To get the SessionStart-style");
    eprintln!("  guidance Claude Code's hooks provide, copy the contents of");
    eprintln!("  MNEME.md into Claude Desktop's system-prompt field via:");
    eprintln!();
    eprintln!("    Settings → \"Configure Claude\" → System Prompt");
    eprintln!();
    eprintln!("  Next steps:");
    eprintln!();
    eprintln!("    1. Quit and relaunch Claude Desktop so it picks up the new");
    eprintln!("       MCP server.");
    eprintln!();
    eprintln!("    2. Try this conversation to confirm memory works end-to-end:");
    eprintln!();
    eprintln!("       You: \"Remember that I prefer Vim keybindings.\"");
    eprintln!("       (then quit and relaunch Claude Desktop)");
    eprintln!("       You: \"What editor do I prefer?\"");
    eprintln!();
    eprintln!("       Claude should recall the preference via mneme.recall.");
    eprintln!();
    eprintln!("    3. To verify storage outside Claude Desktop:");
    eprintln!("         mneme stats");
    eprintln!();
    eprintln!("  Reverse anytime with:  mneme init claude-desktop --uninstall");
    eprintln!();
}

fn prune_empty_object(value: &mut serde_json::Value, key: &str) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if let Some(serde_json::Value::Object(child)) = map.get(key)
        && child.is_empty()
    {
        map.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only used by the macOS/Linux-gated assertion helpers below.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use serde_json::Value;
    use tempfile::TempDir;

    fn fresh_home() -> TempDir {
        TempDir::new().unwrap()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn install_creates_settings_with_mcpserver_entry() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path()).unwrap();
        assert!(settings_path.exists(), "settings file must be written");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["mneme"]["command"], "mneme");
        assert_eq!(settings["mcpServers"]["mneme"]["args"], json!(["client"]));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn install_creates_standalone_mneme_md() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let mneme_md_path = standalone_mneme_md(home.path());
        assert!(mneme_md_path.exists());
        let body = std::fs::read_to_string(&mneme_md_path).unwrap();
        assert_eq!(body, MNEME_MD_TEMPLATE);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn install_preserves_existing_settings_keys() {
        let home = fresh_home();
        let settings_path = config_path(home.path()).unwrap();
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"mcpServers":{"other":{"command":"elsewhere"}},"globalShortcut":"⌘+Shift+M"}"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["other"]["command"], "elsewhere");
        assert_eq!(settings["mcpServers"]["mneme"]["command"], "mneme");
        assert_eq!(settings["globalShortcut"], "⌘+Shift+M");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn install_is_idempotent() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path()).unwrap();
        let mneme_md_path = standalone_mneme_md(home.path());
        let s_first = std::fs::read(&settings_path).unwrap();
        let m_first = std::fs::read(&mneme_md_path).unwrap();
        run(InstallMode::Install, home.path()).unwrap();
        let s_second = std::fs::read(&settings_path).unwrap();
        let m_second = std::fs::read(&mneme_md_path).unwrap();
        assert_eq!(s_first, s_second);
        assert_eq!(m_first, m_second);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn uninstall_removes_mneme_entry_and_md_copy() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        let mneme_md_path = standalone_mneme_md(home.path());
        let settings_path = config_path(home.path()).unwrap();
        assert!(!mneme_md_path.exists(), "MNEME.md copy must be removed");
        // settings.json may exist as `{}` or be absent depending
        // on whether prune ran; both are correct outcomes.
        if settings_path.exists() {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert!(v.get("mcpServers").is_none() || v["mcpServers"].get("mneme").is_none());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn uninstall_preserves_unrelated_settings_keys() {
        let home = fresh_home();
        let settings_path = config_path(home.path()).unwrap();
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["other"]["command"], "x");
        assert_eq!(settings["theme"], "dark");
        assert!(settings["mcpServers"].get("mneme").is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn uninstall_is_idempotent() {
        let home = fresh_home();
        run(InstallMode::Uninstall, home.path()).unwrap();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn show_does_not_write() {
        let home = fresh_home();
        run(InstallMode::Show, home.path()).unwrap();
        let settings_path = config_path(home.path()).unwrap();
        let mneme_md_path = standalone_mneme_md(home.path());
        assert!(!settings_path.exists());
        assert!(!mneme_md_path.exists());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn install_returns_not_yet_implemented_on_other_os() {
        let home = fresh_home();
        let result = run(InstallMode::Install, home.path());
        assert!(matches!(result, Err(AgentError::Generic(_))));
    }
}
