## Memory (mneme)

You have [mneme](https://github.com/tr0mb1r/mneme) — a local-first MCP
memory server. Mneme has four memory layers:

- **L0 procedural** — pinned rules (`pin` / `unpin`); surfaces on every
  recall context.
- **L1 working session** — turn log, checkpointed; resets per process.
- **L3 episodic** — recent events (tool calls, lifecycle events,
  conversation turns, curated semantic events); time-ordered, not
  embedded (ADR-0007).
- **L4 semantic** — long-term facts / decisions / preferences /
  conversations; embedding-indexed.

The default scope is `personal`. Use `switch_scope` once per session if
you want a different default for write tools (`remember`, `pin`,
`record_event`); the filter tools (`recall`, `recall_recent`, `forget`,
`summarize_session`, `export`) treat omitted `scope` as no-filter.

### Tools

- `mneme.recall(query, limit?, scope?, kind?)` — semantic search across
  **L4 only** (ADR-0010). L3 events are not embedded (ADR-0007); to
  search them by similarity, also `remember` the distilled fact to L4.
  Use `recall` before answering questions about past work.
- `mneme.recall_recent(limit?, scope?, kind?)` — last-N L3 events. No
  embedding; fast. Use for "what was the last thing on this branch?"
  Pass `kind` to filter by event kind (e.g. `decision`, `milestone`).
- `mneme.remember(content, kind, tags?, scope?)` — write L3 + L4.
  `kind` ∈ {`fact`, `decision`, `preference`, `conversation`}.
- `mneme.pin(content, tags?, scope?)` — write L0 procedural. Use for
  hard rules ("always X", "never Y") that should surface every session.
- `mneme.record_event(kind, payload, tags?, scope?, retrieval_weight?)`
  — agent-driven L3 producer. `kind` is free-form; canonical kinds:
  `user_message`, `assistant_message`, `decision`, `problem`,
  `resolution`, `milestone`, `preference`, `pivot`, `observation`,
  `summary`. For `user_message` / `assistant_message` the server also
  pushes a matching turn into L1 — one tool call, both layers
  populated.
- `mneme.update(id, ...)` — revise an existing memory; auto-re-embeds
  if `content` changes.
- `mneme.forget(id)` — permanent delete. **Always confirm with the user
  before calling.**
- `mneme.summarize_session(session_id)` — returns a prompt template
  populated with recent L3 events. Feed it through your own completion
  path, then land the digest via `record_event(kind="summary",
  payload={"text": …, "covers": [event_ids]})` and `remember` / `pin`
  any durable facts or rules the digest surfaces.
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

### Layer separation (binding)

- **L3 stays unembedded.** Similarity search is L4's job exclusively.
  `recall` returns L4 hits only; `recall_recent` returns L3 hits only —
  no cross-pollination. Duplication via `remember` is the feature.
- **Auto-emits are server-emitted; never double-record.** The server
  writes `tool_call`, `tool_call_failed`, `session_start`, and
  `session_end` automatically. Do NOT call `record_event` for these
  kinds — you'll create duplicates.
- **`tool_call` payloads carry only the value-bearing arg** of each
  tool (`remember.content`, `recall.query`, `forget.id`, etc.). The
  full `arguments` object is never mirrored — that's an explicit
  privacy invariant.
- **Privacy discipline.** `record_event` and `remember` payloads are
  opaque to mneme. Never include credentials, secrets, API tokens, or
  other sensitive material — the L3 cold archive retains content for
  180+ days and round-trips through `mneme backup` / `mneme restore`.

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
- **Per substantive turn**: capture the exchange via
  `record_event(kind="user_message", payload={"content": <prompt>})`
  and
  `record_event(kind="assistant_message", payload={"content": <reply
  or honest precis>})`. Skip pure-ack turns and meta-only exchanges
  (the auto-emitted `tool_call` already covers tool dispatches).
  Truncate long content at ~4000 chars with a trailing `…` marker.
- **When a turn produces curated semantic content**, also call
  `record_event` with the matching kind: `decision` (with
  `reasoning`), `milestone`, `problem` / `resolution` (link via
  `references` in the resolution payload), `pivot`, `preference`, or
  `observation`.
- **Before context compaction** (PreCompact hook fires) **and at the
  end of significant sessions**: call `mneme.summarize_session`, fill
  the returned prompt template through your completion path, then
  `record_event(kind="summary", payload={"text": …, "covers":
  [event_ids]})` to land the digest in L3. `remember` / `pin` the
  durable facts and rules the digest surfaces.
- **Never invoke `forget` without explicit user confirmation.** Deletion
  is permanent.
