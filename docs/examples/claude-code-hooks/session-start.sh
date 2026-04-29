#!/usr/bin/env bash
# Mneme SessionStart hook for Claude Code.
#
# Emits a one-shot nudge that asks the agent to consult mneme before
# answering. Read-only; the agent does the actual tool calls itself.
#
# Wire into ~/.claude/settings.json — see docs/CLAUDE_CODE_SETUP.md
# §10 "Lifecycle hooks (recommended)".

set -euo pipefail

cat <<'NUDGE'
Mneme is wired into this session. Before responding to the first
prompt:

  1. Read `mneme://procedural` and treat any pinned items as binding
     rules for this session.
  2. Read `mneme://context` to see pinned rules + recent events
     packed against the auto-context budget.
  3. If the user's request references prior context not covered by
     `mneme://context`, call `mneme.recall` with a relevant query
     before answering.

Skipping these reads is the most common way mneme appears to "not
work" — the data is there, but you have to ask.
NUDGE
