//! OpenCode per-agent install (release-planning v2.1 §4.4 —
//! previously Tier-2 conditional, promoted to Tier-1 in this PR
//! after re-validating the gate against OpenCode's current docs).
//!
//! The original §4.4 gate ("ships only if the plugin API has
//! stabilised by M4") referred to OpenCode's TypeScript plugin
//! convention (`/docs/plugins/`). This installer does NOT touch
//! that surface — it writes the `mcp` key in `opencode.json` and
//! a mneme-owned file referenced via the `instructions[]` array.
//! Both surfaces are documented stable as of 2026-05-16 and have
//! not changed shape in the last ~6 months of release notes.
//!
//! OpenCode differs from Claude Desktop and Cursor in two ways
//! that materially improve the install UX:
//!
//! **Different MCP entry shape.** OpenCode uses
//! `mcp.<name>.{type, command, enabled, ...}` instead of
//! `mcpServers.<name>.{command, args, ...}`. `type` is `"local"`
//! for stdio servers, `"remote"` for HTTP/SSE; `command` is an
//! array combining cmd + args. We write `{type: "local", command:
//! ["mneme", "client"], enabled: true}` so multiple OpenCode
//! sessions share one `mneme daemon` instead of contending on the
//! lockfile (the bug `["mneme", "run"]` would introduce per
//! ADR-0012 §D2's single-writer seam).
//!
//! **`instructions[]` array auto-loads files into the system
//! prompt.** Unlike Claude Desktop (paste into UI) or Cursor
//! (paste into Settings → Rules), OpenCode reads every entry in
//! `instructions[]` on every session. We exploit this: write
//! `~/.config/opencode/mneme-memory.md` (mneme-owned, contains the
//! SessionStart-style nudge + the MNEME.md memory protocol body)
//! and add its absolute path to `instructions[]`. Zero paste step.
//!
//! The auto-load file path is absolute, not relative. OpenCode's
//! `instructions[]` path-resolution convention (relative to
//! workspace root vs. opencode.json's directory vs. cwd) is not
//! pinned down by the public docs; absolute paths work in every
//! interpretation. This costs portability for users syncing
//! `~/.config/opencode/` across machines via dotfiles — but the
//! standalone `~/.mneme/MNEME.md` written by the Claude Desktop /
//! Cursor installers has the same constraint.
//!
//! What this installer DOES do:
//!
//! - Resolves `~/.config/opencode/opencode.json` (uniform path on
//!   macOS / Linux / Windows — OpenCode follows XDG conventions).
//! - Upserts `mcp.mneme = {type: "local", command: ["mneme",
//!   "client"], enabled: true}` via `json_config::upsert_file`.
//! - Appends the absolute path of
//!   `~/.config/opencode/mneme-memory.md` to `instructions[]` (no
//!   duplicates on re-install).
//! - Writes `~/.config/opencode/mneme-memory.md` =
//!   SessionStart-nudge body + the existing `MNEME_MD_TEMPLATE`.
//!
//! Uninstall reverses: removes `mcp.mneme`, prunes empty `mcp: {}`
//! if it was the only entry; removes the mneme-memory.md path from
//! `instructions[]`, prunes empty `instructions: []`; deletes the
//! mneme-memory.md file. Idempotent.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{AgentError, InstallMode};
use crate::init::assets::{self, MNEME_MD_TEMPLATE};
use crate::init::json_config;

/// SessionStart-style nudge prepended to the existing
/// `MNEME_MD_TEMPLATE` body. OpenCode has no hooks API, so the
/// nudge has to live in a file OpenCode auto-loads on every
/// session — the same load-bearing piece that Claude Code's
/// SessionStart hook provides and that Claude Desktop / Cursor
/// users have to paste manually.
const OPENCODE_SESSION_NUDGE: &str = r#"# Mneme — session start protocol

On the first turn of every conversation:

1. Read `mneme://procedural` and treat pinned items as binding rules.
2. Read `mneme://context` for recent events.
3. If the user references prior context not in those reads, call `mneme.recall`
   with a relevant query before answering.

Skipping these reads is the most common reason mneme appears not to work — the
data is there, but you have to ask.

If the conversation is clearly about a known project (under `~/projects/<name>`),
call `mneme.switch_scope("<name>")` near the start so writes land in the
project scope instead of `global`.

---

"#;

/// Path to OpenCode's global config JSON. Uniform across all
/// supported OSes — OpenCode follows the XDG convention everywhere
/// (no platform-specific dance like Claude Desktop's
/// `Application Support` directory).
fn config_path(home: &Path) -> PathBuf {
    home.join(".config/opencode/opencode.json")
}

/// Path to the auto-loaded memory file we write. Lives next to
/// `opencode.json` so users hand-inspecting the config find both
/// files in the same directory.
fn memory_file_path(home: &Path) -> PathBuf {
    home.join(".config/opencode/mneme-memory.md")
}

/// Compose the file content written to `mneme-memory.md`. Keeps the
/// nudge text and the shared `MNEME_MD_TEMPLATE` constant in lockstep
/// — every release of the template propagates to OpenCode on
/// `mneme init opencode --upgrade`.
fn memory_file_contents() -> String {
    let mut out = String::with_capacity(OPENCODE_SESSION_NUDGE.len() + MNEME_MD_TEMPLATE.len());
    out.push_str(OPENCODE_SESSION_NUDGE);
    out.push_str(MNEME_MD_TEMPLATE);
    out
}

pub fn run(mode: InstallMode, home_dir: &Path) -> Result<(), AgentError> {
    let settings = config_path(home_dir);
    let memory = memory_file_path(home_dir);
    match mode {
        InstallMode::Install | InstallMode::Upgrade => install(&settings, &memory),
        InstallMode::Uninstall => uninstall(&settings, &memory),
        InstallMode::Show => {
            print_plan(&settings, &memory);
            Ok(())
        }
    }
}

fn install(settings: &Path, memory: &Path) -> Result<(), AgentError> {
    let memory_path_str = memory.display().to_string();
    json_config::upsert_file(settings, |value| {
        json_config::set_path(
            value,
            &["mcp", "mneme"],
            json!({
                "type": "local",
                "command": ["mneme", "client"],
                "enabled": true,
            }),
        )?;
        push_unique_to_string_array(value, "instructions", &memory_path_str)?;
        Ok(())
    })?;
    assets::write_text(memory, &memory_file_contents())?;
    print_post_install(settings, memory);
    Ok(())
}

fn uninstall(settings: &Path, memory: &Path) -> Result<(), AgentError> {
    let memory_path_str = memory.display().to_string();
    if settings.exists() {
        json_config::upsert_file(settings, |value| {
            json_config::remove_path(value, &["mcp", "mneme"])?;
            prune_empty_object(value, "mcp");
            remove_from_string_array(value, "instructions", &memory_path_str);
            prune_empty_array(value, "instructions");
            Ok(())
        })?;
    }
    if memory.exists() {
        std::fs::remove_file(memory)?;
    }
    eprintln!();
    eprintln!("✓ mneme uninstalled from OpenCode");
    eprintln!();
    eprintln!("  Removed:");
    eprintln!("    • mcp.mneme entry from {}", settings.display());
    eprintln!("    • instructions[] entry for the mneme-memory.md path");
    eprintln!("    • {}", memory.display());
    eprintln!();
    eprintln!("  Other entries in {} are preserved.", settings.display());
    eprintln!();
    eprintln!("  Quit and relaunch OpenCode so the dropped MCP server stops");
    eprintln!("  appearing in its session.");
    eprintln!();
    eprintln!("  Re-install anytime with:  mneme init opencode");
    eprintln!();
    Ok(())
}

fn print_plan(settings: &Path, memory: &Path) {
    println!("`mneme init opencode` would write:");
    println!();
    println!(
        "  {}  [mcp.mneme entry + instructions[] reference, other keys preserved]",
        settings.display()
    );
    println!(
        "  {}  [auto-loaded into OpenCode's system prompt via instructions[]]",
        memory.display()
    );
    println!();
    println!("All writes are atomic (tmpfile + fsync + rename).");
    println!("Re-run with --uninstall to reverse.");
}

fn print_post_install(settings: &Path, memory: &Path) {
    eprintln!();
    eprintln!("✓ mneme installed for OpenCode");
    eprintln!();
    eprintln!("  Wired up:");
    eprintln!("    • mcp.mneme entry in    {}", settings.display());
    eprintln!("    • Auto-loaded memory    {}", memory.display());
    eprintln!("    • instructions[] reference to the memory file");
    eprintln!();
    eprintln!("  Unlike Claude Desktop and Cursor, OpenCode auto-loads the");
    eprintln!("  memory file via opencode.json's instructions[] array — no");
    eprintln!("  manual paste step required.");
    eprintln!();
    eprintln!("  Next steps:");
    eprintln!();
    eprintln!("    1. Quit and relaunch OpenCode so it picks up the new MCP");
    eprintln!("       server + the instructions[] entry.");
    eprintln!();
    eprintln!("    2. Try this conversation to confirm memory works end-to-end:");
    eprintln!();
    eprintln!("       You: \"Remember that I prefer Vim keybindings.\"");
    eprintln!("       (then quit and relaunch OpenCode)");
    eprintln!("       You: \"What editor do I prefer?\"");
    eprintln!();
    eprintln!("       OpenCode should recall the preference via mneme.recall.");
    eprintln!();
    eprintln!("    3. To verify storage outside OpenCode:");
    eprintln!("         mneme stats");
    eprintln!();
    eprintln!("  Reverse anytime with:  mneme init opencode --uninstall");
    eprintln!();
}

/// Append `entry` to the string array at top-level `key`. Creates
/// the array if absent; no-op if `entry` is already present
/// (idempotent re-install). Refuses to clobber if the key exists
/// but holds a non-array value — that's user-owned content we
/// won't overwrite.
fn push_unique_to_string_array(
    value: &mut Value,
    key: &str,
    entry: &str,
) -> Result<(), json_config::ConfigError> {
    let Some(map) = value.as_object_mut() else {
        return Err(json_config::ConfigError::NonObjectInPath {
            segment: "<root>".to_owned(),
            at: String::new(),
        });
    };
    let existing = map
        .entry(key.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(arr) = existing else {
        return Err(json_config::ConfigError::NonObjectInPath {
            segment: key.to_owned(),
            at: String::new(),
        });
    };
    let target = Value::String(entry.to_owned());
    if !arr.iter().any(|v| v == &target) {
        arr.push(target);
    }
    Ok(())
}

/// Remove every occurrence of `entry` (as a JSON string) from the
/// array at top-level `key`. No-op if the key is absent or holds
/// a non-array value — uninstall is idempotent.
fn remove_from_string_array(value: &mut Value, key: &str, entry: &str) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let Some(existing) = map.get_mut(key) else {
        return;
    };
    let Value::Array(arr) = existing else {
        return;
    };
    arr.retain(|v| !matches!(v, Value::String(s) if s == entry));
}

fn prune_empty_object(value: &mut Value, key: &str) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(child)) = map.get(key)
        && child.is_empty()
    {
        map.remove(key);
    }
}

fn prune_empty_array(value: &mut Value, key: &str) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if let Some(Value::Array(child)) = map.get(key)
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
    fn install_creates_mcp_entry_with_opencode_shape() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path());
        assert!(settings_path.exists(), "settings file must be written");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcp"]["mneme"]["type"], "local");
        assert_eq!(
            settings["mcp"]["mneme"]["command"],
            json!(["mneme", "client"])
        );
        assert_eq!(settings["mcp"]["mneme"]["enabled"], true);
    }

    #[test]
    fn install_adds_memory_path_to_instructions_array() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path());
        let memory_path_str = memory_file_path(home.path()).display().to_string();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let arr = settings["instructions"]
            .as_array()
            .expect("instructions must be an array");
        assert!(
            arr.iter()
                .any(|v| matches!(v, Value::String(s) if s == &memory_path_str)),
            "instructions[] must include the mneme-memory.md path, got {arr:?}"
        );
    }

    #[test]
    fn install_writes_memory_file_with_nudge_and_template() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let memory_path = memory_file_path(home.path());
        assert!(memory_path.exists());
        let body = std::fs::read_to_string(&memory_path).unwrap();
        assert!(
            body.contains("session start protocol"),
            "memory file must include the SessionStart nudge"
        );
        assert!(
            body.contains("# Memory instructions (managed by mneme)"),
            "memory file must include the MNEME_MD_TEMPLATE body"
        );
    }

    #[test]
    fn install_preserves_existing_settings_keys() {
        let home = fresh_home();
        let settings_path = config_path(home.path());
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{
                "mcp": {"other": {"type": "local", "command": ["elsewhere"]}},
                "instructions": ["AGENTS.md"],
                "theme": "dark"
            }"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcp"]["other"]["type"], "local");
        assert_eq!(settings["mcp"]["mneme"]["type"], "local");
        assert_eq!(settings["theme"], "dark");
        let arr = settings["instructions"].as_array().unwrap();
        assert!(
            arr.iter()
                .any(|v| matches!(v, Value::String(s) if s == "AGENTS.md")),
            "user's existing instructions[] entry must survive"
        );
    }

    #[test]
    fn install_is_idempotent() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        let settings_path = config_path(home.path());
        let memory_path = memory_file_path(home.path());
        let s_first = std::fs::read(&settings_path).unwrap();
        let m_first = std::fs::read(&memory_path).unwrap();
        run(InstallMode::Install, home.path()).unwrap();
        let s_second = std::fs::read(&settings_path).unwrap();
        let m_second = std::fs::read(&memory_path).unwrap();
        assert_eq!(s_first, s_second);
        assert_eq!(m_first, m_second);

        // And verify instructions[] didn't grow a duplicate entry.
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let memory_path_str = memory_path.display().to_string();
        let count = settings["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| matches!(v, Value::String(s) if s == &memory_path_str))
            .count();
        assert_eq!(count, 1, "instructions[] must dedupe on re-install");
    }

    #[test]
    fn uninstall_removes_mcp_entry_and_instructions_reference() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        let memory_path = memory_file_path(home.path());
        let settings_path = config_path(home.path());
        assert!(
            !memory_path.exists(),
            "mneme-memory.md must be removed on uninstall"
        );
        if settings_path.exists() {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert!(v.get("mcp").is_none() || v["mcp"].get("mneme").is_none());
            // instructions[] must not contain the mneme path; the key
            // may be present (if user had other entries) or pruned.
            let memory_path_str = memory_path.display().to_string();
            if let Some(arr) = v["instructions"].as_array() {
                assert!(
                    !arr.iter()
                        .any(|v| matches!(v, Value::String(s) if s == &memory_path_str)),
                    "instructions[] must drop mneme reference"
                );
            }
        }
    }

    #[test]
    fn uninstall_preserves_unrelated_settings_keys() {
        let home = fresh_home();
        let settings_path = config_path(home.path());
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{
                "mcp": {"other": {"type": "local", "command": ["x"]}},
                "instructions": ["AGENTS.md"],
                "theme": "dark"
            }"#,
        )
        .unwrap();

        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["mcp"]["other"]["type"], "local");
        assert_eq!(settings["theme"], "dark");
        assert!(settings["mcp"].get("mneme").is_none());
        // User's pre-existing instructions[] entry survives.
        let arr = settings["instructions"].as_array().unwrap();
        assert!(
            arr.iter()
                .any(|v| matches!(v, Value::String(s) if s == "AGENTS.md")),
            "user's pre-existing instructions[] entry must survive uninstall"
        );
    }

    #[test]
    fn uninstall_prunes_empty_mcp_and_instructions_when_we_were_only_entry() {
        let home = fresh_home();
        run(InstallMode::Install, home.path()).unwrap();
        run(InstallMode::Uninstall, home.path()).unwrap();
        let settings_path = config_path(home.path());
        if settings_path.exists() {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
            // mneme was the only entry in both keys; both should be pruned.
            assert!(v.get("mcp").is_none(), "empty mcp object must be pruned");
            assert!(
                v.get("instructions").is_none(),
                "empty instructions array must be pruned"
            );
        }
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
        let memory_path = memory_file_path(home.path());
        assert!(!settings_path.exists());
        assert!(!memory_path.exists());
    }

    #[test]
    fn config_path_is_xdg_uniform() {
        // OpenCode uses `~/.config/opencode/opencode.json` on macOS,
        // Linux, and Windows. Pin the literal path so a future
        // refactor doesn't introduce a `cfg(target_os)` split.
        let home = Path::new("/some/home");
        assert_eq!(
            config_path(home),
            home.join(".config/opencode/opencode.json")
        );
        assert_eq!(
            memory_file_path(home),
            home.join(".config/opencode/mneme-memory.md")
        );
    }

    #[test]
    fn push_unique_skips_existing_entry() {
        let mut v = json!({"instructions": ["AGENTS.md", "memory.md"]});
        push_unique_to_string_array(&mut v, "instructions", "memory.md").unwrap();
        let arr = v["instructions"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn push_unique_creates_missing_array() {
        let mut v = json!({});
        push_unique_to_string_array(&mut v, "instructions", "memory.md").unwrap();
        assert_eq!(v["instructions"], json!(["memory.md"]));
    }

    #[test]
    fn push_unique_errors_when_key_holds_non_array() {
        let mut v = json!({"instructions": "not-an-array"});
        let result = push_unique_to_string_array(&mut v, "instructions", "memory.md");
        assert!(result.is_err());
        // User's value untouched.
        assert_eq!(v, json!({"instructions": "not-an-array"}));
    }

    #[test]
    fn remove_from_array_is_idempotent_when_missing() {
        let mut v = json!({});
        remove_from_string_array(&mut v, "instructions", "memory.md");
        assert_eq!(v, json!({}));

        let mut v = json!({"instructions": ["AGENTS.md"]});
        remove_from_string_array(&mut v, "instructions", "memory.md");
        assert_eq!(v, json!({"instructions": ["AGENTS.md"]}));
    }
}
