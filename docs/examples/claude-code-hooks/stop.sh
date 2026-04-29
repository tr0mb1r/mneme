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
Turn ended. If this turn established a fact, decision, preference,
or rule that should outlive the session, call the matching mneme
tool now:

  - `mneme.remember` for facts / decisions / preferences /
    conversational context worth recalling later.
  - `mneme.pin` for hard rules ("always X", "never Y") that should
    surface on every subsequent session.

If only transient work happened (one-off questions, incidental
shell commands, exploratory reads), do nothing. Noise is the
failure mode for this hook.
NUDGE
