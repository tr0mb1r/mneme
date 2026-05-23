# Memory instructions (managed by mneme)

You have a persistent memory tool called mneme. Use it to remember things that
should survive across sessions.

## When to remember (call `remember`)

- Decisions: "Decided to use redb because of stability concerns."
- Preferences the user expresses about how they want to work.
- Conventions and rules specific to this project.
- Conclusions reached through investigation that should not be re-derived.

Keep memories concise — target under 500 characters; the tool returns a
`length_advisory` between 500 and 2,000 chars and a stronger `length_warning`
between 2,000 and 10,000. Anything over 10,000 is rejected — extract the key
insight and remember that instead.

## When NOT to remember

- Code or file contents — those are read fresh from disk each session.
- Tool outputs — those are transient.
- Whole conversations — those don't belong in long-term memory.
- Things that are only true right now (project state, current task progress).

## Pinned rules (call `pin`)

For rules that must apply on every session ("always run cargo fmt before
commit"), use `pin` instead of `remember`. Pinned items surface on every
subsequent context assembly via `mneme://procedural`.

## Recall

The active context already includes relevant memories from `mneme://context`.
Call `recall` only when you need to search beyond what's already loaded.

## Episodic events (call `record_event`)

Use `record_event` to mark significant moments — decisions made, problems
encountered, milestones reached. These are time-anchored and queryable by
recency via `recall_recent`. For conversation turns specifically:

- `record_event(kind="user_message", payload={"content": "<text>"})` — server
  also mirrors into the L1 working session.
- `record_event(kind="assistant_message", payload={"content": "<text>"})` —
  same L1 mirror.

Skip ack-only or trivial turns — noise is the failure mode.

## Consolidation at session boundaries

When a session reaches a natural boundary (Claude Code's PreCompact hook,
end of significant task), call `summarize_session` to receive a prompt
template populated with recent L3 events. Fill the prompt via your own
LLM completion path, then land the digest with `record_event(kind="summary",
payload={"text": "<digest>", "covers": ["<event_id>", ...]})`. For each
durable fact the digest surfaces, also call `remember`. For each binding
rule, call `pin` instead. Mneme itself never calls an LLM — that's
cardinal rule #3 of the project.
