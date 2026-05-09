# MCP surface

`mneme run` speaks MCP `2025-06-18` over stdio and advertises 13 tools
and 5 resources. This page is the canonical inventory.

## Tools (13)

### Memory writes

| Tool | Layer | Use when |
|------|-------|----------|
| `remember` | L4 semantic | The user shares a fact, decision, or preference that should persist across sessions. **Size guidance:** target under 500 chars; 500–2k accepted with a `length_advisory`, 2k–10k with a `length_warning`, over 10k rejected with a structured error. See [§Size guardrails](#size-guardrails) below. |
| `update` | L4 semantic | The user revises an existing memory; re-embeds automatically when `content` changes. |
| `forget` | L4 semantic, L0 procedural, L3 episodic (hot+warm) | The user explicitly asks to remove a memory. `id=…` resolves the ULID across all three layers, first hit wins; cold-archive entries stay out of reach by design. Confirm before calling. |
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
| `mneme://stats` | JSON: per-layer counts, schema version, HNSW applied LSN, scheduler observability counters (consolidation + working blocks), and `memories.large_memory_count` (per-tier size distribution + IDs of over-limit entries — see [§Size guardrails](#size-guardrails)). | Diagnostics; first thing to read on any "mneme misbehaving" report. |
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

## Size guardrails

Mneme stores concise facts, not source material. The `remember` tool
description (visible to the agent in `tools/list`) makes the size
contract explicit so the agent calls the tool well rather than
needing the server to push back. v1.1 ships the size tiers in three
phases per release planning §5.6:

| Content length | Behavior |
|---|---|
| < 500 chars | Stored normally. No advisory. |
| 500–2,000 chars | Stored. Response carries a `length_advisory` field suggesting future memories be more concise. |
| 2,000–10,000 chars | Stored. Response carries a stronger `length_warning` field; logged at `info` level. |
| > 10,000 chars | Rejected with a structured error suggesting the agent extract a key insight or store a brief summary plus a source reference. |

The 10,000-character hard limit is configurable via
`[budgets] max_remember_chars`. Existing memories that exceed any
tier remain readable and `recall`-able — the verbatim principle is
preserved; only new writes/updates above the limit are rejected. v1.0
users upgrading to v1.1 see a one-time `~/.mneme/diagnostics.log`
audit summary on first boot (gated by the
`~/.mneme/run/upgrade-audit.done` marker) so any pre-existing
oversized memories are surfaced without any of them being
auto-modified.

C.M1 shipped the tool description; **C.M2 (current state)** ships
the runtime tier checks. `remember` and `update` now classify
content via `src/mcp/tools/size_tier.rs::classify` and attach
structured `_meta` to the tools/call response:

- **Advisory tier (500–2,000 chars).** `_meta = {"length_advisory":
  {"tier": "advisory", "content_length": <N>, "limit":
  <max_remember_chars>, "message": "..."}}`. Memory is stored;
  agent gets the nudge to be more concise next time.
- **Warning tier (2,000–10,000 chars).** `_meta = {"length_warning":
  {"tier": "warning", "content_length": <N>, "limit":
  <max_remember_chars>, "message": "..."}}` and a server-side
  `info`-level log line. Memory is stored.
- **Over-limit (> `max_remember_chars`).** Tools/call response has
  `isError=true`, the text content surfaces the human-readable
  rejection, and `_meta = {"error": {"code": "memory_too_large",
  "content_length": <N>, "limit": <max_remember_chars>,
  "suggestion": "..."}}`. Embedding is NEVER performed for
  rejected content (§5.5).

**C.M3 (current state)** wires the tier counts into `mneme://stats`
as `memories.large_memory_count`:

```json
{
  "memories": {
    "semantic": 1234,
    "large_memory_count": {
      "tier_normal":     1180,
      "tier_advisory":     45,
      "tier_warning":       8,
      "tier_over_limit":    1,
      "over_limit_ids": ["01HABC..."]
    }
  }
}
```

The scan walks the `mem:` prefix on every `mneme://stats` read (or
`mneme stats` CLI invocation) and classifies each `MemoryItem` via
`size_tier::classify` against the configured `max_remember_chars`.
O(N) over the L4 corpus — acceptable for v1.1 cardinalities given
that stats is a per-session diagnostic resource. If profiling
surfaces a problem in v1.2+, cache invalidation hooks into
`remember` / `update` / `forget` are the right place to add
memoisation. `over_limit_ids` lets agents surface the offenders so
the user can `recall` and trim them — never auto-modified
(verbatim principle).
