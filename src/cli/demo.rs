//! `mneme demo` — printed walkthrough of the v1.1 memory patterns
//! (release-planning v2.1 §7.2 / D.M2).
//!
//! Prints a 4-pattern walkthrough to stdout: storing a preference +
//! cross-session recall, recording an event, pinning a rule, and
//! reading `mneme://context`. Pure text — no live MCP traffic, no
//! storage writes, no Claude Code subprocess. Users pair the
//! walkthrough with a real Claude Code session to confirm
//! end-to-end behaviour.
//!
//! Why pure text rather than a live exec: a live walkthrough would
//! either need to spawn a subprocess `mneme run` and pipe MCP frames
//! through it (already covered by `tests/daemon_e2e.rs` + the
//! post-install `mneme stats` verification path), or it would need
//! to coordinate with the user's running daemon (lockfile contention
//! makes this fragile). The printed walkthrough complements
//! `mneme init`'s first-run prompt: same Vim-keybindings example
//! across both, plus three additional patterns the post-install
//! prompt doesn't cover. Per the pre-committed cut order in pin
//! `01KR5ZB7ED01HADZXZKKBV882Z`, this command is the explicit cut
//! candidate if September slips — the post-install prompt covers
//! the load-bearing 5-minute first-run experience either way.

use crate::Result;

pub fn execute() -> Result<()> {
    println!();
    println!("mneme — 4 memory patterns by example");
    println!("=====================================");
    println!();
    println!("This walkthrough shows the four memory patterns you'll use most.");
    println!("Run them in your MCP host (Claude Code, Claude Desktop, ...) to");
    println!("see them work end-to-end. Mneme stays the same; the host changes.");
    println!();

    pattern_1_remember_recall();
    pattern_2_record_event();
    pattern_3_pin_rule();
    pattern_4_context();

    println!("Where to next");
    println!("-------------");
    println!();
    println!("  • Verify storage from your shell:  mneme stats");
    println!("  • Inspect a memory by ID:          mneme inspect <ULID>");
    println!("  • Search semantically:             mneme inspect --query \"...\"");
    println!("  • Configure size + scopes:         mneme/config.toml or `~/.mneme/config.toml`");
    println!("  • Re-run installer:                mneme init claude-code");
    println!();
    println!("Reference docs:");
    println!("  • MCP surface:  https://tr0mb1r.github.io/mneme/mcp-surface.html");
    println!("  • Memory layers: https://tr0mb1r.github.io/mneme/memory-layers.html");
    println!("  • Troubleshooting: https://tr0mb1r.github.io/mneme/troubleshooting.html");
    println!();
    Ok(())
}

fn pattern_1_remember_recall() {
    println!("Pattern 1 — Cross-session recall (the load-bearing pattern)");
    println!("-----------------------------------------------------------");
    println!();
    println!("  Try this in Claude Code:");
    println!();
    println!("      You:    \"Remember that I prefer Vim keybindings.\"");
    println!("      Claude: (calls mneme.remember with content=\"user prefers");
    println!("               Vim keybindings\", kind=\"preference\")");
    println!("              \"Got it. I'll remember that.\"");
    println!();
    println!("  Quit Claude Code. Open a fresh session.");
    println!();
    println!("      You:    \"What editor settings do I prefer?\"");
    println!("      Claude: (calls mneme.recall with query=\"editor settings\")");
    println!("              \"Based on what you've told me, you prefer Vim");
    println!("               keybindings.\"");
    println!();
    println!("  This is the whole point of mneme: stuff that should survive");
    println!("  Claude's context window survives. Recall happens automatically");
    println!("  when the agent reads `mneme://context` on session start (the");
    println!("  SessionStart hook nudges it to do so).");
    println!();
}

fn pattern_2_record_event() {
    println!("Pattern 2 — Recording a structured event");
    println!("----------------------------------------");
    println!();
    println!("  `record_event` captures time-anchored decisions, problems,");
    println!("  pivots, milestones — anything you'd later want to query by");
    println!("  recency rather than by similarity.");
    println!();
    println!("  Try this:");
    println!();
    println!("      You:    \"I just decided to drop the JWT auth approach in");
    println!("               favor of session cookies — record that.\"");
    println!("      Claude: (calls mneme.record_event with");
    println!("                 kind=\"decision\",");
    println!("                 payload={{\"content\": \"Switched from JWT to");
    println!("                                       session cookies\",");
    println!("                          \"reasoning\": \"...\"}} )");
    println!("              \"Recorded.\"");
    println!();
    println!("  Later, in a future session:");
    println!();
    println!("      You:    \"What auth decisions have I made recently?\"");
    println!("      Claude: (calls mneme.recall_recent with kind=\"decision\")");
    println!("              \"You decided last Tuesday to switch from JWT to");
    println!("               session cookies.\"");
    println!();
    println!("  Canonical event kinds: decision, problem, resolution,");
    println!("  milestone, preference, pivot, observation. Plus the");
    println!("  conversation-capture kinds (user_message, assistant_message)");
    println!("  the Stop hook nudges automatically.");
    println!();
}

fn pattern_3_pin_rule() {
    println!("Pattern 3 — Pinning a binding rule");
    println!("----------------------------------");
    println!();
    println!("  `pin` is for rules that should fire on EVERY session, not");
    println!("  just when the agent thinks to recall them. Pinned items");
    println!("  surface in `mneme://procedural` (which the SessionStart");
    println!("  hook nudges the agent to read first).");
    println!();
    println!("  Try this:");
    println!();
    println!("      You:    \"Pin: in this project, never run cargo update");
    println!("               without explicit confirmation.\"");
    println!("      Claude: (calls mneme.pin with content=\"In this project,");
    println!("                                              never run cargo");
    println!("                                              update without");
    println!("                                              explicit");
    println!("                                              confirmation.\")");
    println!("              \"Pinned.\"");
    println!();
    println!("  In every subsequent session, when Claude reads");
    println!("  `mneme://procedural` on first turn, that rule appears in its");
    println!("  context — same effect as if you'd hand-typed it into");
    println!("  CLAUDE.md, but persisted across rooms / repos / projects.");
    println!();
    println!("  Use `pin` for hard rules (\"always X\", \"never Y\"). Use");
    println!("  `remember` for facts and decisions. The line is fuzzy on");
    println!("  purpose; over-pinning crowds the procedural feed, so when in");
    println!("  doubt prefer `remember`.");
    println!();
}

fn pattern_4_context() {
    println!("Pattern 4 — `mneme://context` (what the agent sees on first turn)");
    println!("-----------------------------------------------------------------");
    println!();
    println!("  `mneme://context` is the auto-assembled prompt the agent");
    println!("  reads at session start. It packs (within a token budget):");
    println!();
    println!("    • L0 procedural — every pinned rule.");
    println!("    • L1 working   — turns from the active session checkpoint.");
    println!("    • L3 episodic  — recent events (decisions, milestones, ...).");
    println!("    • L4 semantic  — top recall hits if a query is provided.");
    println!();
    println!("  Read it from your shell:");
    println!();
    println!("      mneme run --stdio </dev/null  # quick stdio one-shot");
    println!("      # or, against a live daemon, via the MCP resources/read");
    println!("      # call from any MCP-aware client");
    println!();
    println!("  Or look at one of its inputs directly:");
    println!();
    println!("      cat ~/.mneme/procedural/pinned.jsonl     # raw pin list");
    println!("      mneme stats                              # per-layer counts");
    println!("      mneme inspect --query \"your query\"      # L4 hits");
    println!();
    println!("  Per-layer scoring weights (procedural=1.0, working=0.9,");
    println!("  episodic=0.8, semantic=0.7) ensure pins always appear at");
    println!("  equal recency to their peer layers; recency uses a 14-day");
    println!("  half-life.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: the demo command runs to completion without error.
    /// Doesn't assert specific text — wording is meant to iterate.
    #[test]
    fn demo_runs_to_completion() {
        assert!(execute().is_ok());
    }
}
