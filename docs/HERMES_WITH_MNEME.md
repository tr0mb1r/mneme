# Hermes Agent with mneme: what it adds

Hermes Agent ships with its own memory: a `memory` file with a fixed
character budget and `chat_history_lookup` over past conversations. That
is enough for a few preferences. It is not enough for an agent that works
the same project every day, follows rules that must not drift, and has its
long sessions compressed. This page is what running Hermes on mneme day to
day showed: what mneme covers that Hermes does not, what we tightened along
the way, and a checklist for verifying your own setup.

## What mneme covers that Hermes doesn't

| Need | Hermes on its own | With mneme |
|---|---|---|
| Rules that must hold every session | `memory` has a 2,200-character budget; a handful of real operating rules does not fit | Pinned rules (`pin`), unlimited, loaded whole at the start of every session and after every compaction |
| Surviving context compaction | The compressed summary drops detail; what was decided an hour ago can be gone | The context hook reloads pins, relevant memories and recent events on the first turn after compression, and records Hermes' own compaction summary as a `summary` event |
| Finding something without the exact words | `chat_history_lookup` finds text you already know how to search for | `recall` combines meaning (embeddings) with exact keywords (BM25), so both a loosely worded question and an exact file name or id find the right entry |
| Facts that change | Old and new versions sit side by side | `supersedes` retires the old memory; `recall` returns the current one |
| A history of what happened | Scattered across chat transcripts | A timeline (`record_event`, `recall_recent`) of milestones, decisions and summaries that reads as the project's log |
| Keeping topics apart | One memory file | Scopes per project or topic, so research, setup notes and the operator's profile don't bleed into each other |
| Keeping secrets out of memory | Nothing checks | `[guardrails] secrets` refuses credential-shaped writes by default |
| Cost and data location | — | Local embeddings, no API calls; the store stays on your machine |

A store after a week of daily use looks like this: 50 facts,
5 pins, 218 events, 1,405 consolidation runs, 0 errors.

## What we tightened from running it

Day-to-day use surfaced rough edges. All of these are fixed in 1.5.0; see
the changelog for detail.

| Symptom | Cause | Fix |
|---|---|---|
| Long pinned rules ended in "…" | The Hermes hook clipped every bullet at 300 characters | Pins are injected whole |
| Timeline entries with a `null` payload | `record_event` accepted a call with the text at the top level instead of inside `payload` | Empty payloads are rejected, and every tool rejects undeclared arguments instead of dropping them |
| Events showed up as raw JSON | The context resource emits payloads as JSON strings; the hook only decoded objects | The hook decodes both and skips empty events |
| The context budget was mostly `tool_call` noise | Auto-logged kinds competed with real events | `mneme://context` leaves auto-logged kinds out by default (`kinds` / `exclude_kinds`), and read-only calls are no longer logged (`[episodic] auto_log_tool_calls`) |
| A retried `remember` stored a second copy | No duplicate check | Exact duplicates return the existing id |
| "Consolidation runs but promotes nothing" | Correct: promotion needs events 28 days old, and `stats` couldn't say so | `stats` reports thresholds, the oldest hot event's age, eligible events and days to the first promotion, plus the server version (also at `GET /version`) |
| `keyword` search and `supersedes` went unused | Hermes loads MCP tools on demand, so the agent learns features from the skill text and the session-start block, not from tool schemas | The hook's footer and the `mneme` skill name both features |
| A "secrets never go into mneme" rule was advice only | Nothing enforced it | Writes are scanned for credential shapes |
| Scope changes didn't stick | The scope is per MCP session and resets on reconnect | `switch_scope` says so in its reply |

Two lessons carried into the code. A feature the agent cannot discover
does not exist, so the skill text and the session-start block are part of
the interface. And a number that looks broken must say why, or the agent
will conclude the system is broken and tell its operator so.

## What stays as it is, on purpose

- Saving is agent-driven. Mneme does not extract facts from conversations;
  the compaction-summary capture is the one automatic write, and it
  records what Hermes already summarised. The practice that works: close
  every substantive session with a `summary` event.
- Scopes partition data; they are not a security boundary. One token, one
  store. Anything confidential needs a second instance.
- The 28-day hot tier. Nothing moves in week one, and now the stats say so.

## Checklist: verifying a memory setup

Useful for any agent memory system. For each item, decide: working,
expected behaviour, unused feature, misuse, or bug.

1. Version and wiring: `mneme --version`; does the installed skill text
   mention the features you rely on; is the hook approved.
2. Consolidation: oldest event age versus `hot_to_warm_days`.
3. Empty events: list them and find the call that created each.
4. Pins: compare `mneme://procedural` with the session-start block.
5. Search: run a query with and without `mode`, with and without a
   similarity cutoff, and compare the scores.
6. Idempotency: write the same fact twice in a scratch scope; compare the
   replies; clean up.
7. Scope: switch scope, reconnect, read it back.
8. Cost: measure the session-start block and count what is in it by kind.
9. Trust labelling: check that the injected block marks memories and
   events as stored data, not instructions.
10. Practice: sessions in the last week with zero writes.
