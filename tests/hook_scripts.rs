//! Smoke coverage for the Claude Code lifecycle hook scripts shipped under
//! `docs/examples/claude-code-hooks/`.
//!
//! The scripts are tiny on purpose — they exit 0 and emit a nudge on stdout
//! that asks the agent to call mneme. CI catches three regression classes:
//!
//! 1. The script no longer parses or runs cleanly under `bash -euo pipefail`.
//! 2. The nudge wording drifts away from the tools/resources it should mention
//!    (typo `mneme.recal`, accidental rename, deleted line).
//! 3. The `~/.claude/settings.json` snippet in `docs/CLAUDE_CODE_SETUP.md`
//!    stops parsing as JSON or stops wiring all three events.
//!
//! Unix-only: the hooks are POSIX shell, and bash isn't on the Windows CI
//! runner. Windows users wire the same nudge text via `*.cmd` if they need it.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn hook_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/examples/claude-code-hooks")
}

fn setup_doc() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/CLAUDE_CODE_SETUP.md")
}

fn run_hook(name: &str) -> String {
    let path = hook_dir().join(name);
    let output = Command::new("bash")
        .arg(&path)
        .output()
        .expect("spawn bash for hook script");
    assert!(
        output.status.success(),
        "hook {name} exited non-zero: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stdout.trim().is_empty(),
        "hook {name} produced no stdout — Claude Code surfaces stdout as the nudge"
    );
    stdout
}

#[test]
fn session_start_hook_nudges_agent_to_read_procedural_and_context() {
    let out = run_hook("session-start.sh");
    for needle in ["mneme://procedural", "mneme://context", "mneme.recall"] {
        assert!(
            out.contains(needle),
            "session-start nudge missing `{needle}`: full stdout was:\n{out}"
        );
    }
}

#[test]
fn precompact_hook_nudges_agent_to_consolidate_before_compaction() {
    let out = run_hook("precompact.sh");
    for needle in ["summarize_session", "mneme.remember", "mneme.pin"] {
        assert!(
            out.contains(needle),
            "precompact nudge missing `{needle}`: full stdout was:\n{out}"
        );
    }
}

#[test]
fn stop_hook_is_permissive_about_no_op_turns() {
    let out = run_hook("stop.sh");
    for needle in ["mneme.remember", "mneme.pin"] {
        assert!(
            out.contains(needle),
            "stop nudge missing `{needle}`: full stdout was:\n{out}"
        );
    }
    let lower = out.to_lowercase();
    assert!(
        lower.contains("transient") || lower.contains("do nothing") || lower.contains("noise"),
        "stop nudge must explicitly permit no-op for transient turns; \
         otherwise the agent will spam `remember` calls. full stdout was:\n{out}"
    );
}

#[test]
fn setup_doc_settings_snippet_parses_and_wires_all_three_events() {
    let doc = std::fs::read_to_string(setup_doc()).expect("read CLAUDE_CODE_SETUP.md");
    let json = extract_first_json_fence_with(&doc, "\"hooks\"")
        .expect("expected a ```json fence containing a \"hooks\" key in CLAUDE_CODE_SETUP.md");
    let parsed: serde_json::Value =
        serde_json::from_str(&json).expect("settings snippet must be valid JSON");

    let hooks = parsed
        .get("hooks")
        .and_then(|v| v.as_object())
        .expect("snippet missing top-level `hooks` object");

    for event in ["SessionStart", "PreCompact", "Stop"] {
        let arr = hooks
            .get(event)
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("snippet missing `{event}` array"));
        assert!(
            !arr.is_empty(),
            "snippet has empty `{event}` array — would silently do nothing"
        );
        let cmd = arr[0]
            .get("hooks")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|h| h.get("command"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            cmd.contains("mneme"),
            "`{event}` command should point at a mneme hook script, got `{cmd}`"
        );
    }
}

fn extract_first_json_fence_with(md: &str, must_contain: &str) -> Option<String> {
    let mut in_fence = false;
    let mut buf = String::new();
    for line in md.lines() {
        let trimmed = line.trim_start();
        if !in_fence && trimmed.starts_with("```json") {
            in_fence = true;
            buf.clear();
            continue;
        }
        if in_fence && trimmed.starts_with("```") {
            if buf.contains(must_contain) {
                return Some(buf.clone());
            }
            in_fence = false;
            buf.clear();
            continue;
        }
        if in_fence {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    None
}
