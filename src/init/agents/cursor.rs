//! Cursor per-agent install (release-planning v2.1 §4.4).
//!
//! Cursor differs from Claude Code in two ways relevant to this
//! installer (and matches Claude Desktop on both):
//!
//! **No hooks API.** Cursor doesn't expose SessionStart /
//! PreCompact / Stop hook scripts. The lifecycle nudges that
//! Claude Code's hooks deliver are unavailable here; agent
//! calibration is "the user pastes guidance into the system
//! prompt or the agent rediscovers via tool descriptions on
//! first use."
//!
//! **Multiple instruction-file conventions, none auto-installable
//! globally.** Cursor reads legacy `.cursorrules` (per-repo,
//! deprecated), modern `.cursor/rules/*.mdc` (per-repo,
//! frontmatter-gated), and a global "Rules for AI" field set
//! only via Cursor's UI — no filesystem path the installer can
//! write to. Per the design call locked in this PR, this installer
//! takes the Claude Desktop precedent: write a standalone
//! `~/.mneme/MNEME.md` for the user to paste once into Cursor's
//! global "Rules for AI" field. Per-workspace `.cursor/rules/`
//! auto-attach is intentionally out of scope (would require a
//! `--workspace <path>` flag and per-repo state tracking).
//!
//! What this installer DOES do:
//!
//! - Resolves `~/.cursor/mcp.json` (uniform path on macOS / Linux /
//!   Windows — Cursor doesn't use the platform-specific
//!   `Application Support` convention Claude Desktop does).
//! - Adds (or updates) `mcpServers.mneme = {"command": "mneme",
//!   "args": ["client"]}` in that file via the existing
//!   `init::json_config::upsert_file` atomic helper. Surrounding
//!   keys (other `mcpServers`, top-level fields) preserved verbatim.
//!   `args=["client"]` (not `["run"]`) so multiple Cursor windows
//!   share one `mneme daemon` and can coexist with concurrent
//!   Claude Code / Claude Desktop sessions instead of fighting
//!   over the lockfile.
//! - Writes a copy of `MNEME.md` to `~/.mneme/MNEME.md` so users
//!   can paste its content into Cursor's "Rules for AI" field —
//!   opt-in agent calibration since Cursor has nowhere to
//!   auto-reference it.
//!
//! Uninstall reverses: removes `mcpServers.mneme`, prunes empty
//! `mcpServers: {}` if it was the only entry, deletes the
//! `~/.mneme/MNEME.md` copy. Idempotent.
//!
//! NOTE on shared `~/.mneme/MNEME.md`: this file is also written
//! by the Claude Desktop installer (intentional — it's mneme's
//! single user-pasteable reference doc). If a user has both
//! agents installed and uninstalls one, the file is removed; the
//! remaining agent loses its paste source until the user re-runs
//! its installer (idempotent install regenerates it). Acceptable
//! because (a) `mneme init <agent>` is a one-line command, and
//! (b) the file is reference docs, not behaviourally load-bearing
//! — Cursor / Claude Desktop work even without it; only the
//! "where do I copy the system-prompt text from?" UX degrades.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{AgentError, InstallMode};
use crate::init::assets::{self, MNEME_MD_TEMPLATE};
use crate::init::json_config;

/// Path to Cursor's MCP config JSON. Uniform across all supported
/// OSes — Cursor uses `~/.cursor/mcp.json` on macOS, Linux, and
/// Windows alike (no platform-specific dance like Claude Desktop's
/// `Application Support` directory).
fn config_path(home: &Path) -> PathBuf {
    home.join(".cursor/mcp.json")
}

/// Path to the standalone MNEME.md copy users can paste from. Same
/// path as the Claude Desktop installer — this is mneme's single
/// user-pasteable reference doc, deliberately shared. See module
/// docs for the implications when both agents are installed.
fn standalone_mneme_md(home: &Path) -> PathBuf {
    home.join(".mneme/MNEME.md")
}

pub fn run(mode: InstallMode, home_dir: &Path) -> Result<(), AgentError> {
    let settings = config_path(home_dir);
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
    eprintln!("✓ mneme uninstalled from Cursor");
    eprintln!();
    eprintln!(
        "  Removed mneme's mcpServers entry and the MNEME.md copy at {}.",
        mneme_md.display()
    );
    eprintln!("  Other entries in {} are preserved.", settings.display());
    eprintln!();
    eprintln!("  Quit and relaunch Cursor so the dropped MCP server stops");
    eprintln!("  appearing in its session.");
    eprintln!();
    eprintln!("  Re-install anytime with:  mneme init cursor");
    eprintln!();
    Ok(())
}

fn print_plan(settings: &Path, mneme_md: &Path) {
    println!("`mneme init cursor` would write:");
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
    eprintln!("✓ mneme installed for Cursor");
    eprintln!();
    eprintln!("  Wired up:");
    eprintln!("    • mcpServers.mneme entry in  {}", settings.display());
    eprintln!("    • Standalone MNEME.md copy   {}", mneme_md.display());
    eprintln!();
    eprintln!("  Cursor has no hooks API and no auto-installable global");
    eprintln!("  instruction file — agent calibration is opt-in. To get the");
    eprintln!("  SessionStart-style guidance Claude Code's hooks provide, copy");
    eprintln!("  the contents of MNEME.md into Cursor's global rules field via:");
    eprintln!();
    eprintln!("    Cursor Settings → Rules → \"Rules for AI\"");
    eprintln!();
    eprintln!("  Next steps:");
    eprintln!();
    eprintln!("    1. Quit and relaunch Cursor so it picks up the new MCP server.");
    eprintln!();
    eprintln!("    2. Try this conversation to confirm memory works end-to-end:");
    eprintln!();
    eprintln!("       You: \"Remember that I prefer Vim keybindings.\"");
    eprintln!("       (then quit and relaunch Cursor)");
    eprintln!("       You: \"What editor do I prefer?\"");
    eprintln!();
    eprintln!("       Cursor should recall the preference via mneme.recall.");
    eprintln!();
    eprintln!("    3. To verify storage outside Cursor:");
    eprintln!("         mneme stats");
    eprintln!();
    eprintln!("  Reverse anytime with:  mneme init cursor --uninstall");
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
    use serde_json::Value;
    use tempfile::TempDir;

    fn fresh_home() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn install_creates_settings_with_mcpserver_entry() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path());
        assert!(settings_path.exists(), "settings file must be written");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["mneme"]["command"], "mneme");
        assert_eq!(settings["mcpServers"]["mneme"]["args"], json!(["client"]));
    }

    #[test]
    fn install_creates_standalone_mneme_md() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let mneme_md_path = standalone_mneme_md(home.path());
        assert!(mneme_md_path.exists());
        let body = std::fs::read_to_string(&mneme_md_path).unwrap();
        assert_eq!(body, MNEME_MD_TEMPLATE);
    }

    #[test]
    fn install_preserves_existing_settings_keys() {
        let home = fresh_home();
        let settings_path = config_path(home.path());
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"mcpServers":{"other":{"command":"elsewhere"}},"editor.fontSize":14}"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["other"]["command"], "elsewhere");
        assert_eq!(settings["mcpServers"]["mneme"]["command"], "mneme");
        assert_eq!(settings["editor.fontSize"], 14);
    }

    #[test]
    fn install_is_idempotent() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path());
        let mneme_md_path = standalone_mneme_md(home.path());
        let s_first = std::fs::read(&settings_path).unwrap();
        let m_first = std::fs::read(&mneme_md_path).unwrap();
        run(InstallMode::Install, home.path()).unwrap();
        let s_second = std::fs::read(&settings_path).unwrap();
        let m_second = std::fs::read(&mneme_md_path).unwrap();
        assert_eq!(s_first, s_second);
        assert_eq!(m_first, m_second);
    }

    #[test]
    fn uninstall_removes_mneme_entry_and_md_copy() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        let mneme_md_path = standalone_mneme_md(home.path());
        let settings_path = config_path(home.path());
        assert!(!mneme_md_path.exists(), "MNEME.md copy must be removed");
        if settings_path.exists() {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert!(v.get("mcpServers").is_none() || v["mcpServers"].get("mneme").is_none());
        }
    }

    #[test]
    fn uninstall_preserves_unrelated_settings_keys() {
        let home = fresh_home();
        let settings_path = config_path(home.path());
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

    #[test]
    fn uninstall_is_idempotent() {
        let home = fresh_home();
        run(InstallMode::Uninstall, home.path()).unwrap();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
    }

    #[test]
    fn show_does_not_write() {
        let home = fresh_home();
        run(InstallMode::Show, home.path()).unwrap();
        let settings_path = config_path(home.path());
        let mneme_md_path = standalone_mneme_md(home.path());
        assert!(!settings_path.exists());
        assert!(!mneme_md_path.exists());
    }

    #[test]
    fn config_path_is_uniform_across_oses() {
        // Cursor uses `~/.cursor/mcp.json` on macOS, Linux, and
        // Windows. Pin the literal path so a future refactor
        // doesn't accidentally introduce a `cfg(target_os)`
        // platform split that would diverge from Cursor's actual
        // convention.
        let home = Path::new("/some/home");
        assert_eq!(config_path(home), home.join(".cursor/mcp.json"));
    }
}
