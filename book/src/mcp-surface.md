# MCP surface

`mneme run` speaks MCP `2025-06-18` over stdio and advertises 13 tools
and 5 resources. This page is the canonical inventory.

## Tools (13)

### Memory writes

| Tool | Layer | Use when |
|------|-------|----------|
| `remember` | L4 semantic | The user shares a fact, decision, or preference that should persist across sessions. |
| `update` | L4 semantic | The user revises an existing memory; re-embeds automatically when `content` changes. |
| `forget` | L4 semantic | The user explicitly asks to remove a memory. Confirm before calling. |
| `pin` | L0 procedural | A *rule* should surface on every recall context (e.g. "always use `uv`, not `pip`"). |
| `unpin` | L0 procedural | A previously-pinned rule no longer applies. |
| `record_event` | L3 episodic (+ L1 mirror for message kinds) | Capture a structured event with a free-form `kind`. Use `user_message`/`assistant_message` for conversation turns; `decision`/`milestone`/`pivot`/etc. for curated semantic events; `summary` after `summarize_session` to land a digest. See [§Event kinds](#event-kinds) below. |

### Memory reads

| Tool | Layer | Use when |
|------|-------|----------|
| `recall` | L4 semantic | Semantic similarity search — find memories close to a natural-language query. |
| `recall_recent` | L3 episodic | "What did we just do?" — time-ordered events (tool calls, lifecycle events, conversation, decisions). |

### Session helpers

| Tool | Use when |
|------|----------|
| `summarize_session` | End of a long working session, or fired by the `PreCompact` hook. Returns a prompt template the host LLM fills in; the agent then calls `record_event(kind="summary", ...)` to land the digest in L3 (and `remember` for any durable facts the digest surfaces). |
| `switch_scope` | Set the session's default scope. After this call, write tools (`remember`, `pin`, `record_event`) without an explicit `scope` argument land in the new scope. Filter tools (`recall`, `recall_recent`, ...) are unaffected. |

### Diagnostics

| Tool | Use when |
|------|----------|
| `stats` | Per-layer counts, schema version, scheduler health. Mirrors `mneme://stats`. |
| `list_scopes` | Show every distinct scope across all layers. |
| `export` | Read-only JSON dump of every memory. Useful for `jq` or grepping. |

### Event kinds

`record_event` takes a free-form `kind: String`. Canonical kinds the
tool description recommends:

| Kind | Recommended payload | Purpose |
|---|---|---|
| `user_message` | `{"content": "<user prompt>"}` | One per turn. Server also pushes a matching turn to L1 working session. |
| `assistant_message` | `{"content": "<assistant response>"}` | Same; for the assistant's response. |
| `decision` | `{"content": "<choice>", "reasoning": "<why>"}` | A choice was made with reasoning. |
| `problem` | `{"content": "<issue>"}` | An issue was identified. |
| `resolution` | `{"content": "<fix>", "references": ["<problem_id>"]}` | A problem was solved. |
| `milestone` | `{"content": "<thing accomplished>"}` | Something significant was completed. |
| `preference` | `{"content": "<the way the user wants it>"}` | The user expressed a preference. |
| `pivot` | `{"content": "<change>", "from": "<previous>", "to": "<new>"}` | A direction was changed. |
| `observation` | `{"content": "<thing noticed>"}` | Something noticed worth recording. |
| `summary` | `{"text": "<digest>", "covers": ["<event_id>", ...]}` | Output of the `summarize_session` workflow. |

Server-emitted kinds (the agent never writes these — they appear in
`recall_recent` results because the server records them on its own):

| Kind | Trigger | Payload |
|---|---|---|
| `tool_call` | Successful `tools/call` dispatch | Per-tool value-bearing arg(s) only — no full args object (privacy) |
| `tool_call_failed` | Errored `tools/call` dispatch | `{tool, error_kind, message}` (message truncated to 500 chars) |
| `session_start` | `mneme run` boot | `{session_id, started_at, embedder, embed_dim, version, protocol}` |
| `session_end` | Graceful shutdown | `{session_id, ended_at, clean_shutdown, turns_total, checkpoints_total}` |

Free-form `kind` means agents can invent new kinds without a schema
migration. `recall_recent` accepts any `kind` filter. Cross-references
between events travel inside the JSON payload (e.g. `"references":
["01ABC..."]`); structured graph fields ship in v1.1 per ADR-0011.

## Resources (5)

| URI | What it returns | When to read |
|-----|----------------|--------------|
| `mneme://stats` | JSON: per-layer counts, schema version, HNSW applied LSN, scheduler observability counters (consolidation + working blocks). | Diagnostics; first thing to read on any "mneme misbehaving" report. |
| `mneme://procedural` | JSON: every pinned item. | On session start — these are the binding rules. |
| `mneme://recent` | JSON: most recent episodic events, newest-first. | "What was the last thing on this branch?" |
| `mneme://context` | Pre-assembled prompt context: pinned rules + recent events + working-session turns + (optional) semantic-recall hits, packed against a token budget. | On session start — single-call replacement for reading procedural + recent + recall separately. |
| `mneme://session/{id}` | JSON: a session's full state (turns + checkpoint metadata). The active session is served from in-memory state; past sessions load from disk. | Reviewing a prior session's turn log. |

## Scoring weights (auto-context)

When the orchestrator assembles `mneme://context`, it scores items as
`semantic_score × layer_weight × recency_decay`. The layer weights are:

| Layer | Weight | Rationale |
|-------|--------|-----------|
| L0 Procedural | 1.0 | Anchors. Must appear at equal recency. |
| L1 Working session | 0.9 | Most-recent context — what's actually happening now. |
| L3 Episodic | 0.8 | Recent history. |
| L4 Semantic | 0.7 | Long-term recall. |

Recency decay is a 14-day half-life. The per-layer reservation in the
budget pass guarantees no single layer is starved by another.
