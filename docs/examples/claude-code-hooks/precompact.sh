#!/usr/bin/env bash
# Mneme PreCompact hook for Claude Code.
#
# Fires when Claude Code is about to compress conversation context.
# This is the last chance to promote anything worth keeping into
# mneme — once compaction lands, the raw turns are gone.
#
# Wire into ~/.claude/settings.json — see docs/CLAUDE_CODE_SETUP.md
# §10 "Lifecycle hooks (recommended)".

set -euo pipefail

cat <<'NUDGE'
Claude Code is about to compact this conversation. Before the
compaction summary lands, run consolidation through mneme:

  1. Call `mneme.summarize_session` with the current `session_id`.
  2. For each fact, decision, or preference that should outlive
     this session, call `mneme.remember` with the appropriate
     `kind` (`fact` / `decision` / `preference` / `conversation`).
  3. For each *rule* established this session ("always X", "never
     Y"), call `mneme.pin` instead — pins surface on every
     subsequent context assembly.

Don't acknowledge the compaction until those calls return. Anything
that doesn't get into mneme here is lost.
NUDGE
