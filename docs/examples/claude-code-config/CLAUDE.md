## Memory (mneme)

You have [mneme](https://github.com/tr0mb1r/mneme) — a local-first MCP
memory server. Mneme has four memory layers:

- **L0 procedural** — pinned rules (`pin` / `unpin`); surfaces on every
  recall context.
- **L1 working session** — turn log, checkpointed; resets per process.
- **L3 episodic** — recent events (tool calls, summaries); time-ordered.
- **L4 semantic** — long-term facts / decisions / preferences /
  conversations; embedding-indexed.

The default scope is `personal`. Use `switch_scope` once per session if
you want a different default for write tools (`remember`, `pin`); the
filter tools (`recall`, `recall_recent`, `forget`, `summarize_session`,
`export`) treat omitted `scope` as no-filter.

### Tools

- `mneme.recall(query, limit?, scope?, kind?)` — semantic search across
  L3 + L4. Use this before answering questions about past work.
- `mneme.recall_recent(limit?, scope?)` — last-N L3 events. No embedding;
  fast. Use for "what was the last thing on this branch?"
- `mneme.remember(content, kind, tags?, scope?)` — write L3 + L4.
  `kind` ∈ {`fact`, `decision`, `preference`, `conversation`}.
- `mneme.pin(content, tags?, scope?)` — write L0 procedural. Use for
  hard rules ("always X", "never Y") that should surface every session.
- `mneme.update(id, ...)` — revise an existing memory; auto-re-embeds
  if `content` changes.
- `mneme.forget(id)` — permanent delete. **Always confirm with the user
  before calling.**
- `mneme.summarize_session(session_id)` — emits a session-summary
  prompt. Feed it back into your own completion path, then `remember`
  or `pin` the result.
- `mneme.stats` / `mneme.list_scopes` / `mneme.export(scope?, limit?)`
  — diagnostics. Cheap to call.
- `mneme.switch_scope(scope)` — sets the current default scope for the
  rest of the process.

### Resources

- `mneme://procedural` — every pinned rule. Read on turn 1; treat pins
  as binding for the rest of the session.
- `mneme://context` — assembled context (pinned + recent + working,
  packed against a token budget). Single-call replacement for reading
  procedural + recent + recall separately.
- `mneme://recent` — last-N L3 events.
- `mneme://session/{id}` — a specific session's full state.
- `mneme://stats` — per-layer counts, schema version, applied LSN,
  current scope.

### Protocol

- **On session start**: read `mneme://procedural` and `mneme://context`.
  The SessionStart hook nudges this; do it even if the nudge didn't
  fire.
- **Before answering about past work, decisions, or rules**: call
  `mneme.recall` first. If it returns nothing, say so — never
  hallucinate past context.
- **When the user shares a fact, decision, or preference** that should
  outlive the session: call `mneme.remember` with the matching `kind`.
- **When the user states a hard rule** ("always use uv", "never push to
  main"): call `mneme.pin` instead.
- **Before context compaction** (PreCompact hook fires) **and at the
  end of significant sessions**: call `mneme.summarize_session`, then
  selectively `remember` / `pin` the parts worth keeping.
- **Never invoke `forget` without explicit user confirmation.** Deletion
  is permanent.
