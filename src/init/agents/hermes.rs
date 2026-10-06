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
//! Context-reload hook (unless `--no-hook`): Hermes compresses long
//! conversations, and the pinned rules and working state the agent had
//! loaded go with the compressed turns. `$HERMES_HOME/agent-hooks/
//! mneme-context.py`, wired as a `pre_llm_call` shell hook in
//! `config.yaml`, re-reads `mneme://context` at session start and on
//! the first turn after a compression and injects it into that turn.
//! Hermes asks before running a new shell hook, and a gateway (no TTY)
//! skips unapproved hooks, so the installer also records the hook's
//! exact command in `$HERMES_HOME/shell-hooks-allowlist.json`, Hermes'
//! documented manual-approval file. Running `mneme init hermes` is the
//! approval; `--no-hook` opts out, `hermes hooks revoke` withdraws it.
//! Skipped on Windows in local mode, where Hermes runs `mneme run`
//! itself and a second instance can't share the store.
//!
//! Uninstall removes the entry, the skill, the marker block, the hook,
//! its state, and its approval. Everything else in those files is
//! preserved. Idempotent.

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

/// Top-level `config.yaml` key holding shell hooks, and the event ours
/// runs on.
const HOOKS_KEY: &str = "hooks";
const HOOK_EVENT: &str = "pre_llm_call";
/// File name of the installed hook; also how our hook entry and
/// allowlist approval are recognised.
const HOOK_FILE: &str = "mneme-context.py";
/// Seconds Hermes lets the hook run. It only does work at session
/// start and after a compression; otherwise it returns at once.
const HOOK_TIMEOUT_SECS: u32 = 20;

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
   not already loaded: `mcp_mneme_recall` with a short query. It matches by
   meaning and by exact words at once, so query a file name, account, env var
   or error string directly (`mode: "keyword"` for literal matches only).
   Filter with `tags` or `scope` when you know them.
3. **Remember** durable facts with `mcp_mneme_remember`: decisions and their
   reasons, preferences the user states, project conventions, conclusions
   from investigation. One fact per memory, under ~500 characters.
   **When a fact changes** (a new balance, a reversed decision), pass the old
   memory's id in `supersedes`: the old one stays readable by id but stops
   showing up in recall. If the reply says the new memory is close to an
   existing one, decide: it replaces it → `mcp_mneme_update` the new id with
   `supersedes: "<old id>"`; it only restates it → `mcp_mneme_forget` the new
   id; both are true → ignore the note.
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
    hook: PathBuf,
    hook_state: PathBuf,
    allowlist: PathBuf,
}

impl Paths {
    fn new(hermes_home: &Path) -> Self {
        Self {
            home: hermes_home.to_path_buf(),
            config: hermes_home.join("config.yaml"),
            skill: hermes_home.join("skills").join("mneme").join("SKILL.md"),
            soul: hermes_home.join("SOUL.md"),
            hook: hermes_home.join("agent-hooks").join(HOOK_FILE),
            hook_state: hermes_home.join("mneme-hook-state"),
            allowlist: hermes_home.join("shell-hooks-allowlist.json"),
        }
    }
}

/// The shell command Hermes runs for our hook. Hermes splits it with
/// `shlex.split` (no shell), so each part is POSIX-quoted.
fn hook_command(paths: &Paths, t: &Transport) -> String {
    let mut parts = vec![paths.hook.display().to_string()];
    match t {
        Transport::Local => {
            parts.extend(["--command", "mneme", "--arg", "client"].map(String::from));
        }
        Transport::Remote { url, token_env } => {
            parts.extend([
                "--url".into(),
                url.clone(),
                "--token-env".into(),
                token_env.clone(),
            ]);
        }
    }
    parts
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:@=+,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// Whether to install the hook for this transport.
fn wants_hook(opts: &InstallOptions, t: &Transport) -> bool {
    !opts.no_hook && (cfg!(unix) || matches!(t, Transport::Remote { .. }))
}

fn hook_item(command: &str) -> Vec<String> {
    vec![
        format!("- command: {}", yaml_config::quote(command)),
        format!("  timeout: {HOOK_TIMEOUT_SECS}"),
    ]
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
        InstallMode::Install | InstallMode::Upgrade => {
            let t = transport(opts)?;
            install(&paths, &t, wants_hook(opts, &t))
        }
        InstallMode::Uninstall => uninstall(&paths),
        InstallMode::Show => {
            let t = transport(opts)?;
            print_plan(&paths, &t, wants_hook(opts, &t));
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

fn install(paths: &Paths, t: &Transport, with_hook: bool) -> Result<(), AgentError> {
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
    // The hook entry rides along in the same write. Unlike the MCP
    // entry it's optional, so a `hooks:` layout we can't edit skips the
    // hook with a warning instead of failing the install.
    let command = hook_command(paths, t);
    let (updated, hook_installed) = if with_hook {
        match yaml_config::upsert_list_item(
            &updated,
            HOOKS_KEY,
            HOOK_EVENT,
            HOOK_FILE,
            &hook_item(&command),
        ) {
            Ok(with) => (with, true),
            Err(e) => {
                eprintln!(
                    "warning: not installing the context-reload hook ({e} in {}). To add it by hand:\n\n{HOOKS_KEY}:\n  {HOOK_EVENT}:\n{}",
                    paths.config.display(),
                    hook_item(&command)
                        .iter()
                        .map(|l| format!("    {l}\n"))
                        .collect::<String>()
                );
                (updated, false)
            }
        }
    } else {
        // `--no-hook` on a re-install takes a previously installed hook out.
        let without = yaml_config::remove_list_item(&updated, HOOKS_KEY, HOOK_EVENT, HOOK_FILE)
            .unwrap_or(updated);
        (without, false)
    };
    if updated != existing {
        assets::write_text(&paths.config, &updated)?;
    }
    if hook_installed {
        assets::write_executable(&paths.hook, assets::HERMES_CONTEXT_HOOK)?;
        approve_hook(&paths.allowlist, &paths.hook, &command)?;
    } else if !with_hook {
        remove_hook_files(paths)?;
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

    print_post_install(paths, t, soul_updated, hook_installed);
    Ok(())
}

/// Record our hook's exact command in Hermes' shell-hook allowlist,
/// replacing any earlier approval of a mneme hook command (the URL or
/// token variable may have changed). Other approvals are untouched.
/// Same entry shape Hermes writes itself.
fn approve_hook(allowlist: &Path, script: &Path, command: &str) -> Result<(), AgentError> {
    let mut data = read_allowlist(allowlist)?;
    let approvals = data
        .as_object_mut()
        .and_then(|o| o.get_mut("approvals"))
        .and_then(|a| a.as_array_mut())
        .ok_or_else(|| AgentError::Generic("unreachable: normalised allowlist".into()))?;
    approvals.retain(|e| !is_our_approval(e));
    let mtime = std::fs::metadata(script)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
    approvals.push(serde_json::json!({
        "event": HOOK_EVENT,
        "command": command,
        "approved_at": chrono::Utc::now().to_rfc3339(),
        "script_mtime_at_approval": mtime,
    }));
    write_allowlist(allowlist, &data)
}

fn is_our_approval(entry: &serde_json::Value) -> bool {
    entry.get("event").and_then(|v| v.as_str()) == Some(HOOK_EVENT)
        && entry
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(|c| c.contains(HOOK_FILE))
}

/// The allowlist as `{"approvals": [...], ...}`, other keys kept.
/// Missing or unreadable files start empty, as Hermes does.
fn read_allowlist(path: &Path) -> Result<serde_json::Value, AgentError> {
    let mut data = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(s.trim_start_matches('\u{feff}'))
            .unwrap_or_else(|_| serde_json::json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };
    if !data.is_object() {
        data = serde_json::json!({});
    }
    if !data["approvals"].is_array() {
        data["approvals"] = serde_json::json!([]);
    }
    Ok(data)
}

fn write_allowlist(path: &Path, data: &serde_json::Value) -> Result<(), AgentError> {
    let text = serde_json::to_string_pretty(data)
        .map_err(|e| AgentError::Generic(format!("serialise allowlist: {e}")))?;
    assets::write_text(path, &(text + "\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Remove the hook script, its per-session state, and its approval.
fn remove_hook_files(paths: &Paths) -> Result<(), AgentError> {
    if paths.hook.exists() {
        std::fs::remove_file(&paths.hook)?;
    }
    if let Some(dir) = paths.hook.parent()
        && dir.exists()
        && std::fs::read_dir(dir)?.next().is_none()
    {
        std::fs::remove_dir(dir)?;
    }
    if paths.hook_state.exists() {
        std::fs::remove_dir_all(&paths.hook_state)?;
    }
    if paths.allowlist.exists() {
        let mut data = read_allowlist(&paths.allowlist)?;
        if let Some(a) = data["approvals"].as_array_mut() {
            let before = a.len();
            a.retain(|e| !is_our_approval(e));
            if a.len() != before {
                write_allowlist(&paths.allowlist, &data)?;
            }
        }
    }
    Ok(())
}

fn uninstall(paths: &Paths) -> Result<(), AgentError> {
    if paths.config.exists() {
        let existing = std::fs::read_to_string(&paths.config)?;
        let updated = yaml_config::remove_entry(&existing, MCP_KEY, SERVER_NAME)
            .and_then(|s| yaml_config::remove_list_item(&s, HOOKS_KEY, HOOK_EVENT, HOOK_FILE))
            .map_err(|e| AgentError::Generic(format!("{e} in {}", paths.config.display())))?;
        if updated != existing {
            assets::write_text(&paths.config, &updated)?;
        }
    }
    remove_hook_files(paths)?;
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
    eprintln!(
        "    • the context-reload hook {} and its approval",
        paths.hook.display()
    );
    eprintln!();
    eprintln!("  Everything else in those files is preserved. Run /reload-mcp in");
    eprintln!("  Hermes (or restart the gateway) to drop the server.");
    eprintln!();
    Ok(())
}

fn print_plan(paths: &Paths, t: &Transport, with_hook: bool) {
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
    if with_hook {
        println!(
            "  {}  [context-reload hook, wired as hooks.{HOOK_EVENT} in config.yaml]",
            paths.hook.display()
        );
        println!(
            "  {}  [approval for that hook's exact command]",
            paths.allowlist.display()
        );
    }
    println!();
    println!("The config.yaml entry:");
    println!();
    for l in snippet(t).lines() {
        println!("  {l}");
    }
    if with_hook {
        println!("  {HOOKS_KEY}:");
        println!("    {HOOK_EVENT}:");
        for l in hook_item(&hook_command(paths, t)) {
            println!("      {l}");
        }
    }
    println!();
    println!("Re-run with --uninstall to reverse.");
}

fn print_post_install(paths: &Paths, t: &Transport, soul_updated: bool, hook_installed: bool) {
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
    if hook_installed {
        eprintln!("    • Context-reload hook   {}", paths.hook.display());
        eprintln!("      (hooks.{HOOK_EVENT}; reloads pinned rules and recent state at");
        eprintln!("      session start and after context compression. Approved in");
        eprintln!(
            "      {}; withdraw with `hermes hooks revoke`",
            paths.allowlist.display()
        );
        eprintln!("      or reinstall with --no-hook.)");
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

    /// The `mcp_servers:` block of a config, up to the next top-level key.
    fn mcp_block(cfg: &str) -> String {
        let start = cfg.find("mcp_servers:").expect("mcp_servers block");
        let rest = &cfg[start..];
        let end = rest[1..]
            .find("\n\n")
            .or_else(|| rest[1..].find("\nhooks:"))
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        rest[..end].to_owned()
    }

    fn allowlist(home: &Path) -> serde_json::Value {
        serde_json::from_str(&read(&home.join("shell-hooks-allowlist.json"))).unwrap()
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
        assert!(!mcp_block(&cfg).contains("command:"), "{cfg}");
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
        assert!(!mcp_block(&cfg).contains("command:"), "{cfg}");
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

    // ---------- context-reload hook ----------

    #[test]
    fn hook_is_installed_wired_and_approved() {
        let tmp = TempDir::new().unwrap();
        run_at(
            InstallMode::Install,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        let script = tmp.path().join("agent-hooks/mneme-context.py");
        assert_eq!(read(&script), assets::HERMES_CONTEXT_HOOK);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&script).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "hook must be executable");
        }
        let cfg = read(&tmp.path().join("config.yaml"));
        // Built with the same quoting as the installer: a Windows path has
        // backslashes, so it gets shell-quoted and then YAML-escaped.
        let command = format!(
            "{} --url http://mneme:7878/mcp --token-env MNEME_HTTP_TOKEN",
            shell_quote(&script.display().to_string())
        );
        assert!(
            cfg.contains(&format!(
                "hooks:\n  # managed by mneme — undo with: mneme init hermes --uninstall\n  pre_llm_call:\n    - command: {}\n      timeout: 20\n",
                yaml_config::quote(&command)
            )),
            "{cfg}"
        );
        let approvals = allowlist(tmp.path());
        let a = &approvals["approvals"][0];
        assert_eq!(a["event"], "pre_llm_call");
        assert_eq!(
            a["command"], command,
            "approval must match the configured command exactly"
        );
        assert!(a["approved_at"].is_string());
    }

    #[test]
    fn local_hook_spawns_mneme_client() {
        let tmp = TempDir::new().unwrap();
        run_at(InstallMode::Install, tmp.path(), &local()).unwrap();
        let cfg = read(&tmp.path().join("config.yaml"));
        if cfg!(unix) {
            assert!(
                cfg.contains("mneme-context.py --command mneme --arg client\""),
                "{cfg}"
            );
        } else {
            assert!(
                !cfg.contains("mneme-context.py"),
                "no local hook on Windows"
            );
        }
    }

    #[test]
    fn reinstalling_replaces_the_approval_and_keeps_other_approvals() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("shell-hooks-allowlist.json"),
            r#"{"approvals":[{"event":"post_tool_call","command":"fmt.sh"}],"note":"keep"}"#,
        )
        .unwrap();
        run_at(InstallMode::Install, tmp.path(), &remote("http://a:1/mcp")).unwrap();
        run_at(InstallMode::Upgrade, tmp.path(), &remote("http://b:2/mcp")).unwrap();
        let data = allowlist(tmp.path());
        let approvals = data["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), 2, "{data}");
        assert!(approvals.iter().any(|a| a["command"] == "fmt.sh"));
        assert!(
            approvals
                .iter()
                .any(|a| a["command"].as_str().unwrap().contains("http://b:2/mcp"))
        );
        assert_eq!(data["note"], "keep");
        let cfg = read(&tmp.path().join("config.yaml"));
        assert_eq!(cfg.matches("mneme-context.py").count(), 1, "{cfg}");
    }

    #[test]
    fn hook_sits_beside_the_users_own_hooks() {
        let tmp = TempDir::new().unwrap();
        let original = "hooks:\n  pre_llm_call:\n    - command: ~/.hermes/agent-hooks/git.sh\n";
        std::fs::write(tmp.path().join("config.yaml"), original).unwrap();
        run_at(
            InstallMode::Install,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        let cfg = read(&tmp.path().join("config.yaml"));
        assert!(cfg.contains("git.sh"), "{cfg}");
        assert!(cfg.contains("mneme-context.py"), "{cfg}");
        run_at(InstallMode::Uninstall, tmp.path(), &local()).unwrap();
        assert_eq!(read(&tmp.path().join("config.yaml")), original);
    }

    #[test]
    fn no_hook_skips_it_and_removes_an_earlier_one() {
        let tmp = TempDir::new().unwrap();
        run_at(
            InstallMode::Install,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        let mut opts = remote("http://mneme:7878/mcp");
        opts.no_hook = true;
        run_at(InstallMode::Upgrade, tmp.path(), &opts).unwrap();
        assert!(!read(&tmp.path().join("config.yaml")).contains("mneme-context.py"));
        assert!(!tmp.path().join("agent-hooks").exists());
        assert!(
            allowlist(tmp.path())["approvals"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn uninstall_removes_hook_state_and_approval() {
        let tmp = TempDir::new().unwrap();
        run_at(
            InstallMode::Install,
            tmp.path(),
            &remote("http://mneme:7878/mcp"),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("mneme-hook-state")).unwrap();
        std::fs::write(tmp.path().join("mneme-hook-state/abc.json"), "{}").unwrap();
        run_at(InstallMode::Uninstall, tmp.path(), &local()).unwrap();
        assert!(!tmp.path().join("agent-hooks").exists());
        assert!(!tmp.path().join("mneme-hook-state").exists());
        assert!(!read(&tmp.path().join("config.yaml")).contains("hooks:"));
        assert!(
            allowlist(tmp.path())["approvals"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn hook_paths_with_spaces_are_quoted_for_shlex() {
        assert_eq!(shell_quote("/opt/data/x.py"), "/opt/data/x.py");
        assert_eq!(shell_quote("/Users/A B/x.py"), "'/Users/A B/x.py'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }
}
