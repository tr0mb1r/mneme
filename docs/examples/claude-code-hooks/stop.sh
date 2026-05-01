#!/usr/bin/env bash
# Mneme Stop hook for Claude Code.
#
# Fires at the end of every assistant turn. Permissive by design —
# most turns produce nothing worth persisting and we don't want to
# spam `remember` calls. The agent decides whether anything actually
# matters.
#
# Wire into ~/.claude/settings.json — see docs/CLAUDE_CODE_SETUP.md
# §10 "Lifecycle hooks (recommended)".

set -euo pipefail

cat <<'NUDGE'
Turn ended. Three options for what to record, depending on
significance:

  1. Conversation capture (default for substantive turns):
       - Call `mneme.record_event` with `kind="user_message"` for
         the user's prompt and `kind="assistant_message"` for your
         response. Server mirrors these into the L1 working session
         too, so `mneme://context` reflects real conversation.
       - Skip for one-line throwaway exchanges.

  2. Curated semantic events (when something meaningful happened):
       - Call `mneme.record_event` with `kind="decision"` /
         `"problem"` / `"resolution"` / `"milestone"` /
         `"preference"` / `"pivot"` / `"observation"` and a
         payload that captures the substance + context.

  3. Durable facts and rules (when something should be retrievable
     across sessions):
       - `mneme.remember` for facts / decisions / preferences /
         conversational context worth recalling by similarity later.
       - `mneme.pin` for hard rules ("always X", "never Y") that
         should surface on every subsequent session.

If only transient work happened (one-off questions, incidental
shell commands, exploratory reads), do nothing. Noise is the
failure mode for this hook.
NUDGE
