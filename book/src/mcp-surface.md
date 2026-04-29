# MCP surface

`mneme run` speaks MCP `2025-06-18` over stdio and advertises 12 tools
and 5 resources. This page is the canonical inventory.

## Tools (12)

### Memory writes

| Tool | Layer | Use when |
|------|-------|----------|
| `remember` | L4 semantic | The user shares a fact, decision, or preference that should persist across sessions. |
| `update` | L4 semantic | The user revises an existing memory; re-embeds automatically when `content` changes. |
| `forget` | L4 semantic | The user explicitly asks to remove a memory. Confirm before calling. |
| `pin` | L0 procedural | A *rule* should surface on every recall context (e.g. "always use `uv`, not `pip`"). |
| `unpin` | L0 procedural | A previously-pinned rule no longer applies. |

### Memory reads

| Tool | Layer | Use when |
|------|-------|----------|
| `recall` | L4 semantic | Semantic similarity search — find memories close to a natural-language query. |
| `recall_recent` | L3 episodic | "What did we just do?" — time-ordered events (tool calls, summaries). |

### Session helpers

| Tool | Use when |
|------|----------|
| `summarize_session` | End of a long working session. Produces a prompt template the host LLM fills in; the agent then `remember`s the summary. |
| `switch_scope` | Set the session's default scope. After this call, write tools (`remember`, `pin`) without an explicit `scope` argument land in the new scope. Filter tools (`recall`, `recall_recent`, ...) are unaffected. |

### Diagnostics

| Tool | Use when |
|------|----------|
| `stats` | Per-layer counts, schema version, scheduler health. Mirrors `mneme://stats`. |
| `list_scopes` | Show every distinct scope across all layers. |
| `export` | Read-only JSON dump of every memory. Useful for `jq` or grepping. |

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
