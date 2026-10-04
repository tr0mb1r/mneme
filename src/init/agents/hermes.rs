//! Hermes Agent (Nous Research) per-agent install.
//!
//! Hermes reads MCP servers from `$HERMES_HOME/config.yaml`
//! (`~/.hermes` by default, `/opt/data` in the official Docker image)
//! under `mcp_servers:`, and supports both transports mneme offers:
//!
//! - **Local** (default): `{command: mneme, args: [client]}`. Hermes
//!   spawns the stdio↔socket bridge, which auto-starts and shares one
//!   `mneme daemon` with every other agent on the machine — the same
//!   shape as the Claude Desktop / Cursor installers.
//! - **Remote** (`--url`): `{url: <endpoint>, headers: {Authorization:
//!   "Bearer ${MNEME_HTTP_TOKEN}"}}`. For Hermes and mneme in separate
//!   containers on one Compose network (`--url http://mneme:7878/mcp`),
//!   or Hermes reaching a `mneme serve` / `[http]`-enabled daemon on
//!   another host. Hermes resolves `${VAR}` in `headers` at connect
//!   time from its environment and `$HERMES_HOME/.env`, so the token
//!   value never lands in `config.yaml` (Invariant 3 — configs
//!   reference secrets, they don't contain them). An unset variable
//!   makes Hermes fail the connect loudly rather than send a literal
//!   `${...}`.
//!
//! `config.yaml` is edited through [`crate::init::yaml_config`], which
//! touches only the `mcp_servers.mneme` lines and keeps every comment
//! Hermes seeded into the file.
//!
//! Calibration guidance goes in two places:
//!
//! - `$HERMES_HOME/skills/mneme/SKILL.md` — a Hermes skill holding the
//!   full memory protocol, with tool names as Hermes registers them
//!   (`mcp_mneme_<tool>`). Skills are listed by name and description
//!   in every system prompt and loaded on demand.
//! - A short marker block in `$HERMES_HOME/SOUL.md`, the one global
//!   file Hermes always injects, telling the agent to read pinned rules
//!   and recent context at the start of a conversation. Only added when
//!   `SOUL.md` already exists: Hermes seeds its default persona into a
//!   *missing* `SOUL.md` on first start, and creating the file first
//!   would suppress that. The post-install message says how to finish.
//!
//! Uninstall removes the entry, the skill, and the marker block.
//! Everything else in those files is preserved. Idempotent.

use std::path::{Path, PathBuf};

use super::{AgentError, InstallMode, InstallOptions};
use crate::init::{assets, marker, yaml_config};

/// Env var Hermes reads the bearer token from in remote mode, unless
/// `--token-env` names another. Same name `mneme serve` reads, so one
/// Compose `.env` entry can feed both containers.
pub const DEFAULT_TOKEN_ENV: &str = "MNEME_HTTP_TOKEN";

/// Top-level `config.yaml` key holding MCP servers.
const MCP_KEY: &str = "mcp_servers";
/// Our entry's name. Hermes prefixes tools with it: `mcp_mneme_recall`.
const SERVER_NAME: &str = "mneme";

/// Seconds Hermes waits for the server to connect and finish
/// `initialize`. Generous: in local mode the first connect may
/// auto-start the daemon, which loads the embedding model.
const CONNECT_TIMEOUT_SECS: u32 = 120;

/// Marker-block body for `SOUL.md`. Kept short: SOUL.md is the
/// agent's identity slot, so the full protocol lives in the skill.
const SOUL_BLOCK: &str = "\
## Long-term memory (mneme)

You have persistent memory through the `mneme` MCP server. At the start of
every conversation, read `mneme://procedural` (pinned rules: follow them) and
`mneme://context` (recent activity) with `mcp_mneme_read_resource`. When the
user refers to something from an earlier session, call `mcp_mneme_recall`
before answering. Load the `mneme` skill for when to remember, pin, and record
events.";

const SKILL_MD: &str = r#"---
name: mneme
description: Persistent long-term memory across sessions via the mneme MCP server. Use at the start of every conversation, whenever the user refers to earlier work, and whenever a decision, preference, or rule is worth keeping.
version: 1.0.0
metadata:
  hermes:
    tags: [memory, mcp]
    category: memory
---

# mneme — long-term memory

mneme is an MCP server that stores memories outside this conversation. Its
tools appear as `mcp_mneme_<tool>`; its resources are read with
`mcp_mneme_read_resource`.

## When to Use

- First turn of every conversation: load pinned rules and recent context.
- The user mentions something from a previous session ("like last time",
  "the approach we agreed on").
- A decision, user preference, project convention, or hard-won conclusion
  comes up that should not have to be re-derived next time.
- A natural boundary: a task is finished or the session is ending.

## Procedure

1. **Start of conversation.** Read `mneme://procedural` and treat each pinned
   item as a binding rule. Read `mneme://context` for recent events; for a
   topic-focused view read `mneme://context?q=<topic>`.
2. **Recall before answering** when the user references prior context that is
   not already loaded: `mcp_mneme_recall` with a short query. Filter with
   `tags` or `scope` when you know them.
3. **Remember** durable facts with `mcp_mneme_remember`: decisions and their
   reasons, preferences the user states, project conventions, conclusions
   from investigation. One fact per memory, under ~500 characters. Correct a
   stale memory with `mcp_mneme_update`; delete one with `mcp_mneme_forget`.
4. **Pin** rules that must apply every session ("always answer in British
   English") with `mcp_mneme_pin` instead of `remember`.
5. **Record events** worth a timeline with `mcp_mneme_record_event`
   (`kind`: `decision`, `milestone`, `problem`, ...). `mcp_mneme_recall_recent`
   lists them by time.
6. **Session boundary.** Call `mcp_mneme_summarize_session`, write the digest
   it asks for, store it with `mcp_mneme_record_event` (`kind: "summary"`),
   and `remember` / `pin` any durable fact or rule it surfaces.
7. **Projects.** When the conversation is clearly about one project, call
   `mcp_mneme_switch_scope` with the project name so new memories land in
   that scope instead of `global`.

## Pitfalls

- Don't store code, file contents, tool output, or whole conversations:
  those are transient or re-readable.
- Don't store things that are only true right now (current task progress).
- Writes over 10,000 characters are rejected; extract the key insight.
- mneme complements Hermes' built-in `memory` tool rather than replacing it:
  keep that for the tiny always-on profile, and use mneme for everything
  that needs search, history, or more room.

## Verification

`mcp_mneme_stats` reports memory counts and the active scope. If the
`mcp_mneme_*` tools are missing, the server is not connected: check
`hermes mcp list` and run `/reload-mcp`.
"#;

/// Where `mneme init hermes` connects Hermes to mneme.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    /// Spawn `mneme client` (or `mneme run` on Windows).
    Local,
    /// Connect to a Streamable HTTP endpoint.
    Remote { url: String, token_env: String },
}

struct Paths {
    home: PathBuf,
    config: PathBuf,
    skill: PathBuf,
    soul: PathBuf,
}

impl Paths {
    fn new(hermes_home: &Path) -> Self {
        Self {
            home: hermes_home.to_path_buf(),
            config: hermes_home.join("config.yaml"),
            skill: hermes_home.join("skills").join("mneme").join("SKILL.md"),
            soul: hermes_home.join("SOUL.md"),
        }
    }
}

/// Resolve Hermes' home: `--hermes-home`, then `$HERMES_HOME`, then
/// `~/.hermes`.
pub fn hermes_home(home_dir: &Path, opts: &InstallOptions) -> PathBuf {
    if let Some(h) = &opts.agent_home {
        return h.clone();
    }
    match std::env::var("HERMES_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h.trim()),
        _ => home_dir.join(".hermes"),
    }
}

pub fn run(mode: InstallMode, home_dir: &Path, opts: &InstallOptions) -> Result<(), AgentError> {
    run_at(mode, &hermes_home(home_dir, opts), opts)
}

/// [`run`] with Hermes' home already resolved. Tests call this so a
/// developer's own `$HERMES_HOME` can never be touched.
fn run_at(mode: InstallMode, hermes_home: &Path, opts: &InstallOptions) -> Result<(), AgentError> {
    let paths = Paths::new(hermes_home);
    match mode {
        InstallMode::Install | InstallMode::Upgrade => install(&paths, &transport(opts)?),
        InstallMode::Uninstall => uninstall(&paths),
        InstallMode::Show => {
            print_plan(&paths, &transport(opts)?);
            Ok(())
        }
    }
}

fn transport(opts: &InstallOptions) -> Result<Transport, AgentError> {
    let token_env = opts
        .token_env
        .clone()
        .unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_owned());
    match &opts.url {
        None => {
            if opts.token_env.is_some() {
                return Err(AgentError::Generic(
                    "--token-env only applies together with --url".into(),
                ));
            }
            Ok(Transport::Local)
        }
        Some(url) => {
            validate_url(url)?;
            validate_env_name(&token_env)?;
            Ok(Transport::Remote {
                url: url.trim().to_owned(),
                token_env,
            })
        }
    }
}

fn validate_url(url: &str) -> Result<(), AgentError> {
    let u = url.trim();
    let scheme_ok = u.starts_with("http://") || u.starts_with("https://");
    let clean = !u.is_empty()
        && !u
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"');
    if scheme_ok && clean && u.len() > "https://".len() {
        Ok(())
    } else {
        Err(AgentError::Generic(format!(
            "--url must be an http:// or https:// URL, e.g. http://mneme:7878/mcp (got {url:?})"
        )))
    }
}

fn validate_env_name(name: &str) -> Result<(), AgentError> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(AgentError::Generic(format!(
            "--token-env must be an environment variable name like MNEME_HTTP_TOKEN (got {name:?})"
        )))
    }
}

/// `mcp_servers.mneme` value lines, unindented.
fn entry_body(t: &Transport) -> Vec<String> {
    match t {
        Transport::Local => {
            // `mneme client` needs Unix sockets; on Windows the host
            // spawns its own `mneme run` instead.
            let sub = if cfg!(unix) { "client" } else { "run" };
            vec![
                "command: \"mneme\"".into(),
                format!("args: [\"{sub}\"]"),
                format!("connect_timeout: {CONNECT_TIMEOUT_SECS}"),
            ]
        }
        Transport::Remote { url, token_env } => vec![
            format!("url: {}", yaml_config::quote(url)),
            "headers:".into(),
            format!(
                "  Authorization: {}",
                yaml_config::quote(&format!("Bearer ${{{token_env}}}"))
            ),
            format!("connect_timeout: {CONNECT_TIMEOUT_SECS}"),
        ],
    }
}

/// The entry as a standalone snippet, for the manual-paste fallback.
fn snippet(t: &Transport) -> String {
    let mut s = format!("{MCP_KEY}:\n  {SERVER_NAME}:\n");
    for l in entry_body(t) {
        s.push_str("    ");
        s.push_str(&l);
        s.push('\n');
    }
    s
}

fn read_or_empty(path: &Path) -> Result<String, AgentError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

fn install(paths: &Paths, t: &Transport) -> Result<(), AgentError> {
    // 1. config.yaml — the only step that can be refused, so it goes
    //    first and nothing else is written if it fails.
    let existing = read_or_empty(&paths.config)?;
    let updated = yaml_config::upsert_entry(&existing, MCP_KEY, SERVER_NAME, &entry_body(t))
        .map_err(|e| {
            AgentError::Generic(format!(
                "{e} in {}.\nAdd this to the file by hand instead:\n\n{}",
                paths.config.display(),
                snippet(t)
            ))
        })?;
    if updated != existing {
        assets::write_text(&paths.config, &updated)?;
    }

    // 2. The skill.
    assets::write_text(&paths.skill, SKILL_MD)?;

    // 3. SOUL.md marker block, only into an existing file.
    let soul_updated = if paths.soul.exists() {
        let soul = std::fs::read_to_string(&paths.soul)?;
        let new = marker::upsert_block(&soul, SOUL_BLOCK)?;
        if new != soul {
            assets::write_text(&paths.soul, &new)?;
        }
        true
    } else {
        false
    };

    print_post_install(paths, t, soul_updated);
    Ok(())
}

fn uninstall(paths: &Paths) -> Result<(), AgentError> {
    if paths.config.exists() {
        let existing = std::fs::read_to_string(&paths.config)?;
        let updated = yaml_config::remove_entry(&existing, MCP_KEY, SERVER_NAME)
            .map_err(|e| AgentError::Generic(format!("{e} in {}", paths.config.display())))?;
        if updated != existing {
            assets::write_text(&paths.config, &updated)?;
        }
    }
    if paths.skill.exists() {
        std::fs::remove_file(&paths.skill)?;
    }
    // Prune `skills/mneme/`, then `skills/` itself, if we left them
    // empty. Hermes recreates `skills/` whenever it needs it.
    for dir in paths.skill.ancestors().skip(1).take(2) {
        if dir.exists() && std::fs::read_dir(dir)?.next().is_none() {
            std::fs::remove_dir(dir)?;
        }
    }
    if paths.soul.exists() {
        let soul = std::fs::read_to_string(&paths.soul)?;
        let cleaned = marker::remove_block(&soul)?;
        // Never delete SOUL.md, even if it ends up empty: it is the
        // user's identity file, and Hermes treats empty and missing
        // differently.
        if cleaned != soul {
            assets::write_text(&paths.soul, &cleaned)?;
        }
    }

    eprintln!();
    eprintln!(
        "✓ mneme uninstalled from Hermes Agent ({})",
        paths.home.display()
    );
    eprintln!();
    eprintln!("  Removed:");
    eprintln!("    • mcp_servers.mneme from {}", paths.config.display());
    eprintln!("    • {}", paths.skill.display());
    eprintln!("    • the mneme block in {}", paths.soul.display());
    eprintln!();
    eprintln!("  Everything else in those files is preserved. Run /reload-mcp in");
    eprintln!("  Hermes (or restart the gateway) to drop the server.");
    eprintln!();
    Ok(())
}

fn print_plan(paths: &Paths, t: &Transport) {
    println!(
        "`mneme init hermes` would write (Hermes home: {}):",
        paths.home.display()
    );
    println!();
    println!(
        "  {}  [mcp_servers.mneme entry; comments and other keys preserved]",
        paths.config.display()
    );
    println!(
        "  {}  [memory protocol as a Hermes skill]",
        paths.skill.display()
    );
    println!(
        "  {}  [short mneme block, only if the file already exists]",
        paths.soul.display()
    );
    println!();
    println!("The config.yaml entry:");
    println!();
    for l in snippet(t).lines() {
        println!("  {l}");
    }
    println!();
    println!("Re-run with --uninstall to reverse.");
}

fn print_post_install(paths: &Paths, t: &Transport, soul_updated: bool) {
    eprintln!();
    eprintln!(
        "✓ mneme installed for Hermes Agent ({})",
        paths.home.display()
    );
    eprintln!();
    eprintln!("  Wired up:");
    match t {
        Transport::Local => eprintln!(
            "    • mcp_servers.mneme (local: spawns `mneme client`) in {}",
            paths.config.display()
        ),
        Transport::Remote { url, .. } => eprintln!(
            "    • mcp_servers.mneme (remote: {url}) in {}",
            paths.config.display()
        ),
    }
    eprintln!("    • Skill                 {}", paths.skill.display());
    if soul_updated {
        eprintln!("    • Memory block in       {}", paths.soul.display());
    }
    eprintln!();
    eprintln!("  Next steps:");
    eprintln!();
    let mut step = 1;
    if let Transport::Remote { token_env, .. } = t {
        eprintln!("    {step}. Give Hermes the token mneme expects, as {token_env}, either in");
        eprintln!(
            "       the Hermes container's environment or in {}:",
            paths.home.join(".env").display()
        );
        eprintln!();
        eprintln!("         {token_env}=<same value as MNEME_HTTP_TOKEN on the mneme side>");
        eprintln!();
        eprintln!(
            "       The config only references ${{{token_env}}}; the value never goes in it."
        );
        eprintln!();
        step += 1;
    } else {
        eprintln!("    {step}. Make sure `mneme` is on the PATH Hermes runs with (a systemd");
        eprintln!("       gateway service may not see ~/.local/bin). Optional: start the");
        eprintln!("       daemon once with `mneme daemon` so the first connect is fast.");
        eprintln!();
        step += 1;
    }
    if !soul_updated {
        eprintln!(
            "    {step}. {} doesn't exist yet. Start Hermes once so it seeds its",
            paths.soul.display()
        );
        eprintln!("       default persona, then run `mneme init hermes --upgrade` to add the");
        eprintln!("       memory block. (Creating it now would replace Hermes' default persona.)");
        eprintln!();
        step += 1;
    }
    eprintln!("    {step}. Run /reload-mcp in Hermes, or restart the gateway.");
    eprintln!();
    step += 1;
    eprintln!("    {step}. Check it end to end:");
    eprintln!();
    eprintln!("         You: \"Remember that I prefer Vim keybindings.\"");
    eprintln!("         (start a new session with /new)");
    eprintln!("         You: \"What editor do I prefer?\"");
    eprintln!();
    eprintln!("  Reverse anytime with:  mneme init hermes --uninstall");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn local() -> InstallOptions {
        InstallOptions::default()
    }

    fn remote(url: &str) -> InstallOptions {
        InstallOptions {
            url: Some(url.into()),
            ..InstallOptions::default()
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn local_install_writes_command_entry_and_skill() {
        let tmp = TempDir::new().unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        let cfg = read(&tmp.path().join("config.yaml"));
        assert!(cfg.starts_with("mcp_servers:\n"), "{cfg}");
        assert!(cfg.contains("  mneme:\n    command: \"mneme\"\n"), "{cfg}");
        let sub = if cfg!(unix) { "client" } else { "run" };
        assert!(cfg.contains(&format!("    args: [\"{sub}\"]\n")), "{cfg}");
        assert!(cfg.contains("connect_timeout: 120"));
        let skill = read(&tmp.path().join("skills/mneme/SKILL.md"));
        assert!(skill.starts_with("---\nname: mneme\n"));
        assert!(skill.contains("mcp_mneme_recall"));
    }

    #[test]
    fn remote_install_references_the_token_env_var_not_its_value() {
        let tmp = TempDir::new().unwrap();
        run_at(
            InstallMode::Install,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        let cfg = read(&tmp.path().join("config.yaml"));
        assert!(
            cfg.contains("    url: \"http://mneme:7878/mcp\"\n"),
            "{cfg}"
        );
        assert!(
            cfg.contains("    headers:\n      Authorization: \"Bearer ${MNEME_HTTP_TOKEN}\"\n"),
            "{cfg}"
        );
        assert!(!cfg.contains("command:"));
    }

    #[test]
    fn custom_token_env() {
        let tmp = TempDir::new().unwrap();
        let mut opts = remote("https://mem.example/mcp");
        opts.token_env = Some("MY_TOKEN".into());
        run_at(InstallMode::Install, tmp.path(), &opts).unwrap();
        assert!(read(&tmp.path().join("config.yaml")).contains("\"Bearer ${MY_TOKEN}\""));
    }

    #[test]
    fn switching_local_to_remote_replaces_the_entry() {
        let tmp = TempDir::new().unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        run_at(
            InstallMode::Upgrade,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        let cfg = read(&tmp.path().join("config.yaml"));
        assert!(!cfg.contains("command:"), "{cfg}");
        assert_eq!(cfg.matches("  mneme:\n").count(), 1, "{cfg}");
    }

    #[test]
    fn install_preserves_comments_and_other_servers() {
        let tmp = TempDir::new().unwrap();
        let original = "\
# Hermes Agent configuration
model:
  default: anthropic/claude   # main model

mcp_servers:
  # GitHub, for PR work
  github:
    command: npx
    args: [\"-y\", \"@modelcontextprotocol/server-github\"]
";
        std::fs::write(tmp.path().join("config.yaml"), original).unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        let cfg = read(&tmp.path().join("config.yaml"));
        assert!(cfg.starts_with(original), "{cfg}");
        assert!(cfg.contains("  mneme:\n"));
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("SOUL.md"), "You are Hermes.\n").unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        let cfg1 = read(&tmp.path().join("config.yaml"));
        let soul1 = read(&tmp.path().join("SOUL.md"));
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        assert_eq!(cfg1, read(&tmp.path().join("config.yaml")));
        assert_eq!(soul1, read(&tmp.path().join("SOUL.md")));
    }

    #[test]
    fn soul_block_added_only_when_soul_exists() {
        let tmp = TempDir::new().unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        assert!(
            !tmp.path().join("SOUL.md").exists(),
            "must not create SOUL.md: that would suppress Hermes' seeded persona"
        );

        std::fs::write(tmp.path().join("SOUL.md"), "You are Hermes.\n").unwrap();
        run_at(InstallMode::Upgrade, tmp.path(), &local()).unwrap();
        let soul = read(&tmp.path().join("SOUL.md"));
        assert!(soul.starts_with("You are Hermes.\n"), "{soul}");
        assert!(soul.contains(marker::BEGIN_MARKER));
        assert!(soul.contains("mneme://procedural"));
    }

    #[test]
    fn soul_block_avoids_injection_scanner_phrases() {
        // Hermes scans context files for prompt-injection patterns.
        let lower = SOUL_BLOCK.to_lowercase();
        for phrase in [
            "ignore",
            "disregard",
            "previous instructions",
            ".env",
            "curl",
        ] {
            assert!(!lower.contains(phrase), "SOUL block contains {phrase:?}");
        }
    }

    #[test]
    fn uninstall_reverses_install_exactly() {
        let tmp = TempDir::new().unwrap();
        let cfg = "model:\n  default: x\n\nmcp_servers:\n  github:\n    command: npx\n";
        let soul = "You are Hermes.\n";
        std::fs::write(tmp.path().join("config.yaml"), cfg).unwrap();
        std::fs::write(tmp.path().join("SOUL.md"), soul).unwrap();
        run_at(
            InstallMode::Install,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        run_at(InstallMode::Uninstall, tmp.path(), &local()).unwrap();
        assert_eq!(read(&tmp.path().join("config.yaml")), cfg);
        assert_eq!(read(&tmp.path().join("SOUL.md")), soul);
        assert!(
            !tmp.path().join("skills").exists(),
            "empty skills/ is pruned"
        );
    }

    #[test]
    fn uninstall_keeps_other_skills() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("skills/other")).unwrap();
        std::fs::write(tmp.path().join("skills/other/SKILL.md"), "x").unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        run_at(InstallMode::Uninstall, tmp.path(), &local()).unwrap();
        assert!(tmp.path().join("skills/other/SKILL.md").exists());
    }

    #[test]
    fn uninstall_is_idempotent_on_a_fresh_home() {
        let tmp = TempDir::new().unwrap();
        run_at(InstallMode::Uninstall, tmp.path(), &local()).unwrap();
        run_at(InstallMode::Uninstall, tmp.path(), &local()).unwrap();
        assert!(!tmp.path().join("config.yaml").exists());
    }

    #[test]
    fn show_does_not_write() {
        let tmp = TempDir::new().unwrap();
        run_at(
            InstallMode::Show,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn unsupported_config_layout_fails_without_writing_anything() {
        let tmp = TempDir::new().unwrap();
        let cfg = "mcp_servers: {github: {command: npx}}\n";
        std::fs::write(tmp.path().join("config.yaml"), cfg).unwrap();
        let err = run_at(InstallMode::Install, tmp.path(), &local()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Add this to the file by hand"), "{msg}");
        assert!(msg.contains("  mneme:\n    command: \"mneme\""), "{msg}");
        assert_eq!(read(&tmp.path().join("config.yaml")), cfg);
        assert!(!tmp.path().join("skills").exists());
    }

    #[test]
    fn bad_url_and_env_name_are_rejected() {
        let tmp = TempDir::new().unwrap();
        for url in [
            "mneme:7878",
            "ftp://x/mcp",
            "http://a b",
            "http://",
            "http://x/\"y",
        ] {
            assert!(
                run_at(InstallMode::Install, tmp.path(), &remote(url)).is_err(),
                "{url} must be rejected"
            );
        }
        let mut opts = remote("http://mneme:7878/mcp");
        opts.token_env = Some("1BAD-NAME".into());
        assert!(run_at(InstallMode::Install, tmp.path(), &opts).is_err());
        assert!(!tmp.path().join("config.yaml").exists());
    }

    #[test]
    fn token_env_without_url_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let opts = InstallOptions {
            token_env: Some("X".into()),
            ..InstallOptions::default()
        };
        assert!(run_at(InstallMode::Install, tmp.path(), &opts).is_err());
    }

    #[test]
    fn explicit_hermes_home_wins() {
        let opts = InstallOptions {
            agent_home: Some(PathBuf::from("/opt/data")),
            ..InstallOptions::default()
        };
        assert_eq!(
            hermes_home(Path::new("/home/u"), &opts),
            PathBuf::from("/opt/data")
        );
    }
}
