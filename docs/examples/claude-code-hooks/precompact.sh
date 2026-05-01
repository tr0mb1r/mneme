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
compaction summary lands, run the full consolidation workflow
through mneme:

  1. Call `mneme.summarize_session` to receive a prompt template
     populated with the recent L3 events. Mneme does not call any
     LLM itself — it just stages the prompt.

  2. Fill that prompt via your own LLM completion path to produce
     the digest (a structured summary of decisions, milestones,
     pivots, and any unresolved threads from this session).

  3. Call `mneme.record_event` with `kind="summary"` and a payload
     of the form `{"text": "<your digest>", "covers": ["<event_id>",
     ...]}` to land the digest in L3. This is the destination the
     summarize_session prompt was building toward.

  4. For each durable fact, decision, or preference the digest
     surfaces, call `mneme.remember` to land it in L4 (similarity
     retrieval). Reference the originating L3 event id in the
     remembered content so traceability survives compaction.

  5. For each *rule* established this session ("always X", "never
     Y"), call `mneme.pin` instead — pins surface on every
     subsequent context assembly.

Don't acknowledge the compaction until those calls return. Anything
that doesn't get into mneme here is lost when the host trims the
window.
NUDGE
