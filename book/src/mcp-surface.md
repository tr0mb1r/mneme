# MCP surface

The mneme MCP server speaks MCP `2025-06-18` and advertises 13 tools,
5 resources, and 2 resource templates. Two interchangeable entry points serve the same
surface against the same data dir: `mneme daemon` + `mneme client`
(v1.1 default — one warm process, many clients) and `mneme run` (single-
host stdio fallback). See [CLI surface](./cli.md#two-mcp-server-modes-daemon-vs-run)
for the choice; this page is the canonical inventory of what either
serves.

## Tools (13)

### Memory writes

| Tool | Layer | Use when |
|------|-------|----------|
| `remember` | L4 semantic | The user shares a fact, decision, or preference that should persist across sessions. Classify it with `kind` (`fact`, the default, `decision`, `preference` or `conversation`); see [§Argument names](#argument-names). **Size guidance:** target under 500 chars; 500–2k accepted with a `length_advisory`, 2k–10k with a `length_warning`, over 10k rejected with a structured error. See [§Size guardrails](#size-guardrails) below. **Replacing a fact:** pass `supersedes` (an id or array of ids) and the old memories drop out of `recall` and auto-context while staying readable by id. When the nearest current memory has similarity ≥ 0.80 the reply names it, both in the text and in `_meta` (`related_memory`, or `duplicate_advisory` at ≥ 0.95). See [§Superseding and similarity advisories](#superseding-and-similarity-advisories). **Retries are safe:** content identical to a current memory in the same scope (ignoring leading/trailing and repeated whitespace) writes nothing and replies `already stored as <id>` with `_meta.deduplicated: true` — see [§Retries and exact duplicates](#retries-and-exact-duplicates). Content with a credential in a well-known format is rejected — see [§Write guardrails](#write-guardrails). **Relating facts:** `relations: [{type, id \| ref, reason}]` says this memory `refines`, `contradicts` or `supersedes` another, or what it is `evidence_for`; `provenance` and `source` say how it is known. See [§Relations](#relations) and [§Provenance](#provenance). |
| `update` | L4 semantic | The user revises an existing memory: any subset of `content`, `kind`, `tags`, `scope`, `status`. `status: "archived"` keeps a memory that is no longer current out of `recall` and auto-context without deleting it (`"active"` restores it) — see [§Edit time and archiving](#edit-time-and-archiving). Re-embeds automatically when `content` changes. Also takes `supersedes`, to retire older memories after the fact (e.g. after `remember` reported a close match). New `content` goes through the same [secret guard](#write-guardrails) as `remember`. Takes `relations`, `provenance` and `source` like `remember`; relations are added, never removed, by an update. |
| `forget` | L4 semantic, L0 procedural, L3 episodic (hot+warm) | The user explicitly asks to remove a memory. `id=…` resolves the ULID across all three layers, first hit wins; cold-archive entries stay out of reach by design. The `tool_call` events that mirrored the memory's text are redacted in the same call — see [§Forgetting and the timeline](#forgetting-and-the-timeline). `id` is the only argument: there is no bulk delete by `query` or `scope` (find the ids with `recall` / `recall_recent` and forget them one at a time). Confirm before calling. |
| `pin` | L0 procedural | A *rule* should surface on every recall context (e.g. "always use `uv`, not `pip`"). Passes the [secret guard](#write-guardrails). To replace a rule, `unpin` the old one and `pin` the new. |
| `unpin` | L0 procedural | A previously-pinned rule no longer applies. |
| `record_event` | L3 episodic (+ L1 mirror for message kinds) | Capture a structured event with a free-form `kind`. Use `user_message`/`assistant_message` for conversation turns; `decision`/`milestone`/`pivot`/etc. for curated semantic events; `summary` after `summarize_session` to land a digest. **`payload` is required** and must carry content, e.g. `{"content": "<what happened>"}`; there is no top-level `text` or `content` argument, a bare boolean or number payload counts as empty, and (as for every tool) undeclared arguments are rejected with `INVALID_PARAMS` naming the keys rather than ignored. See [§Event kinds](#event-kinds) below. `promote: {type, tags}` also stores the event's text as a memory with an `evidence_for` link back to the event — see [§Promoting an event](#promoting-an-event). |

### Memory reads

| Tool | Layer | Use when |
|------|-------|----------|
| `recall` | L4 semantic | Hybrid search — meaning *and* exact words, fused, so both questions and exact names (`live_bt.py`, "D account", an env var, an error string) work. `mode` picks `hybrid` (default), `semantic`, or `keyword`; each row says which search found it in `match` (`semantic` / `keyword` / `both`). See [§Hybrid recall](#hybrid-recall). Filter with `scope`, `kind`, `tags` (a memory must carry *every* listed tag), and `min_similarity` (cosine floor, `1.0` = identical; omit to take the nearest `limit` however far away they are). Each row carries both `score` (cosine *distance*, lower is closer) and `similarity` (`1 - score`, the orientation `min_similarity` uses). Superseded memories are left out unless `include_superseded: true`, in which case they come after every current match and carry `superseded_by`. `layers: ["semantic", "episodic"]` also searches recorded events by keyword (the default is `["semantic"]`); rows carry `layer` and are ordered by `rank`. `since` / `until` (RFC3339 or ULID) keep rows in a `[since, until)` window on `time_field` (`created_at` by default). `scope` takes one scope or several separated by commas. See [§Searching events](#searching-events). A memory that a newer one refines or contradicts is returned lower down with `demoted: true`. `ref` returns the memories citing an external reference, `provenance` filters, `expand: 1` quotes linked memories — see [§Relations](#relations). |
| `recall_recent` | L3 episodic | "What did we just do?" — time-ordered events (tool calls, lifecycle events, conversation, decisions). Optional `since` / `until` bound the result to a `[since, until)` window against `created_at` (RFC3339 or 26-char ULID); when either bound is set, `limit` caps at 1000 instead of 200. The server does not parse natural language — convert phrases like "last Tuesday" to RFC3339 client-side before calling. Reads the hot tier, and the warm tier too when the window can reach it: `since` older than `hot_to_warm_days` (28 by default), or `until` set without `since`. `include_warm: true` reads warm on any call, `false` never does. Each row has `tier` (`hot` / `warm` / `cold`). Events in the cold archive are returned only with `include_cold: true` (slow; give a window; at most 1000 cold rows). See [§Warm and cold reads](#warm-and-cold-reads). |

### Session helpers

| Tool | Use when |
|------|----------|
| `summarize_session` | End of a long working session, or fired by the `PreCompact` hook. Returns a prompt template the host LLM fills in; the agent then calls `record_event(kind="summary", ...)` to land the digest in L3 (and `remember` for any durable facts the digest surfaces). |
| `switch_scope` | Set the session's default scope. After this call, write tools (`remember`, `pin`, `record_event`) without an explicit `scope` argument land in the new scope. Filter tools (`recall`, `recall_recent`, ...) are unaffected. **Held per MCP session only** (one connection on stdio or the daemon, one `Mcp-Session-Id` over HTTP): it is not saved and resets to the configured default when the client reconnects or the server restarts. An agent that spans topics should pass `scope` on every write and filtered read. See [§Scopes](#scopes). |

### Diagnostics

| Tool | Use when |
|------|----------|
| `stats` | Per-layer counts, server `version`, schema version, scheduler health, and consolidation status (tier thresholds, age of the oldest hot event, days to the next promotion — see [§Consolidation status](#consolidation-status)). Mirrors `mneme://stats`. |
| `list_scopes` | Show every distinct scope across all layers. |
| `export` | Read-only JSON dump of every memory. Useful for `jq` or grepping. The `episodic` array covers the hot and warm tiers, newest first, each row with `tier`; `include_warm: false` gives hot only. Each layer is capped at `limit_per_layer` (default 200, max 5000). Cold-archive events are included only with `include_cold: true`. |

### L1 is session-local

The working session (L1) is the conversation in progress: the messages
an agent records with `record_event` and one `tool` turn per write. It is
what the `working` section of `mneme://context` shows and what
`stats` → `working.turns_total` counts.

Each MCP session has its own:

| Transport | One working session per |
|---|---|
| `mneme run` (stdio) | process |
| `mneme daemon` (Unix socket) | accepted connection |
| HTTP (`mneme serve`, or the daemon with `[http] enabled`) | `Mcp-Session-Id` |

So two agents on one daemon never see each other's turns, and
`turns_total` describes one conversation. Up to 1.5.x the daemon and the
HTTP listener shared a single working session between all clients:
agent A's turns were served to agent B in `mneme://context`. Stored
memories (L0, L3, L4) are shared as before; only L1 and the current
scope are per session.

Lifecycle:

- A working session starts empty when the client connects (HTTP: on
  `initialize`). It is checkpointed to
  `~/.mneme/sessions/<session_id>.snapshot` on the `[checkpoints]`
  cadence, by its own scheduler.
- It ends when the connection closes, when the HTTP client sends
  `DELETE /mcp`, when an idle HTTP session expires
  (`[http] session_idle_minutes`), or when the server stops. In every
  case the turns not yet on disk are flushed first; `DELETE /mcp`
  replies only after the flush.
- A session that never recorded a turn writes no snapshot, so health
  probes and clients that only read leave no files behind.
- An HTTP client that reconnects gets a new `Mcp-Session-Id` and a new,
  empty working session. Earlier ones stay readable as
  `mneme://session/{id}`.

### Forgetting and the timeline

`remember`, `pin` and `update` each mirror their `content` into an L3
`tool_call` event (unless `[episodic] auto_log_tool_calls = "off"`). Up
to 1.5.1 `forget` deleted the memory and left those events alone, so the
text stayed readable through `recall_recent`, `export` and auto-context.

`forget(id)` now also rewrites every hot- and warm-tier `tool_call`
event about that memory, in place, to:

```json
{"tool": "remember", "memory_id": "01M7…", "redacted": "forgotten", "at": "2026-11-15T10:42:07Z"}
```

The row keeps its id, timestamps, scope and tier, so the timeline still
shows that a write happened and when; the content, kind and the scope
argument are gone from the payload. `at` is when the redaction ran. The
reply says how many rows were redacted, in the text and as
`_meta.redacted_tool_calls`.

A row is redacted when it is a `remember`, `pin` or `update` event and:

- its `memory_id` is the forgotten id (rows written by 1.5.2 or later:
  successful writes report the id as `_meta.id`, and the event records
  it); or
- its `id` is the forgotten id (every `update` row, old or new); or
- it has neither, and its `content` equals the forgotten memory's
  content after whitespace normalisation. This reaches `remember` / `pin`
  rows written before `memory_id` existed.

Limits, so you know what is and is not covered:

- **Cold archive.** Events already folded into a cold quarter bundle are
  not rewritten; the bundles are immutable.
- **Legacy rows of an edited memory.** A pre-1.5.2 `remember` row whose
  memory was later changed with `update` no longer equals the final text
  and has no id, so it is not found. Its `update` rows are.
- **Legacy match is by text.** A pre-1.5.2 row for a *different* memory
  with identical content is redacted too.
- **Already forgotten.** For a memory forgotten before 1.5.2, call
  `forget` with its id again: the reply is `no such memory …`, and any
  rows carrying that id (its `update` events) are still redacted. The
  redaction runs on every `forget`, which also makes it safe to retry.
- **`log_tool_call_values`.** With `[episodic] log_tool_call_values =
  "ids"` the events never held the text, only its length and a short
  hash; `forget` finds them by id and removes those too. With `"off"`
  they hold the tool name only and there is nothing to redact.
- **Other event kinds.** Only `tool_call` rows are touched. An event you
  wrote yourself with `record_event` that quotes the memory is a separate
  item: `forget` it by its own id.

### Argument names

Every reply names a memory's class `kind` (`recall` rows, `export`,
the resources), so that is the name to pass. `remember`, `update` and
`recall` take **`kind`**, with **`type`** kept as an alias for clients
written against 1.5.1 and earlier, where it was the only accepted name
and a call with `kind` failed. Pass one or the other. Both with the same
value is accepted; both with different values is `INVALID_PARAMS`
("pass either kind or type"), not a silent pick.

Undeclared arguments are rejected with `INVALID_PARAMS`, naming the stray
keys and the accepted ones. A few commonly guessed arguments also say
where to go instead:

| Call | Reply |
|---|---|
| `forget` with `query` / `scope` | No bulk delete; find the ids with `recall` or `recall_recent` and call `forget` once per `id`. |
| `remember` with `pinned: true` | `remember` does not pin; call `pin` to add an always-on rule. (`pinned: false` is accepted and does nothing.) |
| `record_event` with `text` / `content` | Put the text under `payload`: `{"content": "…"}`. |

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
| `summary` | `{"text": "<digest>", "covers": ["<event_id>", ...]}` | Output of the `summarize_session` workflow. The Hermes context hook also records each compaction summary this way, with `"source": "hermes-compaction"`. |

Server-emitted kinds (the agent never writes these — they appear in
`recall_recent` results because the server records them on its own):

| Kind | Trigger | Payload |
|---|---|---|
| `tool_call` | Successful `tools/call` dispatch, subject to `[episodic] auto_log_tool_calls` (default `writes`: read-only tools are not mirrored) | Per-tool value-bearing arg(s) only — no full args object (privacy). For `remember`, `pin` and `update` that includes `content`, plus `memory_id`: the id of the memory the write stored or changed. After `forget` of that memory the row is reduced to `{tool, memory_id, redacted: "forgotten", at}` — see [§Forgetting and the timeline](#forgetting-and-the-timeline). |
| `tool_call_failed` | Errored `tools/call` dispatch (not logged under `auto_log_tool_calls = "off"`) | `{tool, error_kind, message}` (message truncated to 500 chars). A secret-guard rejection records only `secret_detected`. |
| `session_start` | `mneme run` boot | `{session_id, started_at, embedder, embed_dim, version, protocol}` |
| `session_end` | Graceful shutdown | `{session_id, ended_at, clean_shutdown, turns_total, checkpoints_total}` |

Free-form `kind` means agents can invent new kinds without a schema
migration. `recall_recent` accepts any `kind` filter. Cross-references
between events travel inside the JSON payload (e.g. `"references":
["01ABC..."]`); structured graph fields ship in v1.1 per ADR-0011.

## Resources (5)

| URI | What it returns | When to read |
|-----|----------------|--------------|
| `mneme://stats` | JSON: server `version`, per-layer counts, schema version, HNSW applied LSN, scheduler observability counters (consolidation + working blocks; consolidation includes the tier thresholds and hot-tier ages), and `memories.large_memory_count` (per-tier size distribution + IDs of over-limit entries — see [§Size guardrails](#size-guardrails)). | Diagnostics; first thing to read on any "mneme misbehaving" report. |
| `mneme://procedural` | JSON: every pinned item. | On session start — these are the binding rules. |
| `mneme://recent` | JSON: most recent episodic events, newest-first. | "What was the last thing on this branch?" |
| `mneme://context` | Pre-assembled prompt context: pinned rules + working-session turns + recent events, packed against a token budget. Append `?q=<text>` to fold in semantically-similar long-term memories and rank events against the query, and `?scope=<name>` (or `?scope=a,b`) to restrict to those scopes. Auto-logged event kinds are left out unless asked for — see [§Auto-context parameters](#auto-context-parameters). | On session start — single-call replacement for reading procedural + recent + recall separately. |
| `mneme://session/{id}` | JSON: a session's full state (turns + checkpoint metadata). The active session is served from in-memory state; past sessions load from disk. | Reviewing a prior session's turn log. |

## Resource templates (2)

Parameterised resources, advertised through `resources/templates/list`.
A client that only reads `resources/list` will not see them.

| URI template | What it returns |
|---|---|
| `mneme://context{?q,scope,limit,kinds,exclude_kinds}` | Auto-context, optionally seeded with a semantic query, restricted to a scope, and filtered by event kind. |
| `mneme://session/{id}` | A single session's full state. Substitute a session ULID. |

## Auto-context parameters

`resources/read` hands the server a URI and nothing else, so
`mneme://context` takes its parameters from the URI's query string:

| Param | Effect |
|---|---|
| `q` (alias `query`) | Natural-language seed for the L4 semantic layer. Percent-encoded; `+` is accepted for space. Without it the `semantic` array is empty — a vector search needs something to be similar *to*. |
| `scope` | One scope, or a comma-separated set (`scope=a,b`). Restricts L3 / L4 to those scopes. L0 returns the pins of those scopes plus the always-on `[auto_context] pin_scopes` (default `global`). L1 working turns are [session-local](#l1-is-session-local) and always included. |
| `limit` | How many semantic candidates to over-fetch before the token-budget pass trims. Clamped server-side. |
| `kinds` | Comma-separated event kinds. The `episodic` section then holds only these. Naming a hidden kind opts back in to it. |
| `exclude_kinds` | Comma-separated event kinds to hide from `episodic`, in addition to the default exclusions. |

```
mneme://context                             → L0 + L1 + L3
mneme://context?q=how%20do%20we%20deploy     → + L4 semantic hits
mneme://context?q=deploy&scope=work          → all four layers; work scope, plus global pins
mneme://context?q=deploy&scope=work,infra    → two scopes, plus global pins
mneme://context?kinds=decision,summary       → only those event kinds in L3
mneme://context?kinds=tool_call              → the auto-logged rows, opted back in
```

**Auto-logged kinds are hidden by default.** The `episodic` section omits
`tool_call`, `tool_call_failed`, `session_start` and `session_end`: the
server writes them itself, and in one deployment 56 of 61 context events
were `tool_call` rows that used most of the token budget. A `kinds` list
replaces that default exclusion (name `tool_call` to get it back);
`exclude_kinds` adds to it. The filter runs inside the scan, so hidden rows
do not use up the over-fetch window. `recall_recent` and `mneme://recent`
are unaffected and return every kind. Each `episodic` row also carries its
`retrieval_weight`.

**Events the agent has marked as low-signal are hidden too.** Events of
kind `archived`, and events whose `retrieval_weight` is at or below
`[auto_context] min_event_weight` (0.3), are left out before the budget
pass, so they are not charged for and then discarded by the client.
`?kinds=archived` brings the first group back.

**Which events are chosen.** Candidates are the 64 most recent events
plus, when the read has `?q=`, the episodic index's best lexical matches
for it (so an on-topic event from two months ago can be offered). They are
ranked by the score in [§Scoring](#scoring-auto-context) and then thinned:

1. exact duplicates (same kind and payload, ignoring case and whitespace)
   are dropped;
2. at most `max_events_per_kind` (6) events of one kind are kept;
3. an event whose words overlap an already-kept one by Jaccard ≥ 0.6 is
   dropped (events of fewer than six distinct words are exempt).

**Long payloads are clipped.** A payload longer than `event_clip_chars`
(400) is cut and the row carries `"truncated": true`; the budget is
charged for the clipped text. When the payload is a JSON object, the cut
is made inside its `content` (or `text`) field, so the payload still
parses. Read the whole event with `recall_recent`.

**Scopes and pins.** Before 1.6, `?scope=myproject` returned only the pins
stored under `myproject`, so every rule pinned under `global` vanished
from a scoped read. A scoped read now returns the pins of the requested
scopes and of `[auto_context] pin_scopes`. Set `pin_scopes = []` to get the
old behaviour. With no `scope`, every pin is returned, as before.

The response body has one array per layer (`procedural`, `working`,
`episodic`, `semantic`), a `request` block echoing how the URI was
interpreted (so "no hits" is distinguishable from "no query supplied"),
`layer_tokens`, and `total_tokens` / `max_tokens`. The reply's `uri` echoes
exactly what was requested, parameters included, so a client can correlate
it.

| `request` field | Meaning |
|---|---|
| `query` | The `q` seed, or `null`. |
| `scopes` | The requested scope set; `[]` for none. |
| `scope` | The requested scope when exactly one was given, else `null`. |
| `pin_scopes` | Scopes whose pins are added to a scoped read. |
| `kinds`, `exclude_kinds` | The event-kind filters. |
| `shares` | The layer shares in force: `episodic_max`, `semantic_min`, `semantic_min_items`, `working_max`. |

`layer_tokens` gives the tokens spent on `procedural`, `working`,
`episodic` and `semantic`; they sum to `total_tokens`. Semantic rows carry
`updated_at` and `rank` beside `similarity`.

Parsing is deliberately lenient — an unknown key, a malformed `limit`,
or a stray `&` yields a request that still assembles something useful.
Auto-context is read unattended; failing it closed would leave the
session with no memory at all.

> **Changed in v1.3.** Before v1.3 the resource passed no query and
> emitted a hardcoded empty `semantic` array, so nothing written with
> `remember` could reach auto-context. The `working` section was also
> missing from the body while its turns were still charged against the
> token budget. Both fixed; the query parameters are new.

## Warm and cold reads

Consolidation moves an event from the hot tier to the warm tier once its
`last_accessed` is `hot_to_warm_days` old (28 by default). Up to 1.5.1
every read stopped at the hot tier, so those events could not be read
back: a `recall_recent` window over an older month returned only the few
rows that were still hot, and `export` left the warm rows out of what
looked like a full dump.

From 1.5.2:

| Call | Tiers read |
|---|---|
| `recall_recent` with no `since` / `until` | hot |
| `recall_recent` with `since` newer than `hot_to_warm_days` ago | hot |
| `recall_recent` with `since` older than that | hot + warm |
| `recall_recent` with `until` only | hot + warm |
| `recall_recent` with `include_warm: true` | hot + warm, whatever the window |
| `recall_recent` with `include_warm: false` | hot, whatever the window |
| `recall_recent` with `include_cold: true` (1.6) | hot + warm + cold, unless `include_warm: false` |
| `export` (tool and `mneme export`) | hot + warm |
| `export` tool with `include_warm: false` | hot |
| `export` tool with `include_cold: true`, `mneme export --include-cold` (1.6) | hot + warm + cold |
| `recall` with `layers` containing `episodic` (1.6) | hot + warm, events of `index_kinds` only |

Ordering and limits are unchanged. `recall_recent` still sorts by
`last_accessed` (newest first), so hot rows come before warm ones, and
`limit` applies to the merged result. `export` sorts by `created_at`,
newest first. Rows carry `"tier": "hot"`, `"warm"` or `"cold"`.

Reading the cold tier means decompressing quarterly zstd bundles, so it
is never automatic. `recall_recent` opens only the quarters that overlap
`since` / `until` and reads at most 1000 cold rows per call (the newest
that pass the filters); without a window every quarter is a candidate.
`export` counts cold rows toward `limit_per_layer`. An event found in
both warm and cold (a crash between the two writes of the move) is
returned once, as warm.

The automatic rule uses the threshold in effect now. If you raised
`hot_to_warm_days` after events were promoted, some warm events are
younger than the new threshold; pass `include_warm: true` to reach them.

`mneme://recent` and `summarize_session` are still hot only. Events in
the cold archive (`warm_to_cold_days`, 180 by default) are not searched
by `recall`. Kinds listed in `[consolidation] cold_exempt_kinds`
(`archived`, `summary`, `milestone`, `lesson`, `decision` by default)
never go to cold, so they stay searchable; see
[Configuration](./configuration.md).

## Searching events

L3 events have no embeddings, so `recall` used to find only what had
been stored with `remember`. A decision written with `record_event` and
never promoted to a memory could be listed by `recall_recent` if you
knew when it happened, and not found by what it said.

`recall` with `layers: ["semantic", "episodic"]` searches both. The
episodic half is keyword search over the events whose kind is in
`[episodic] index_kinds` (`decision`, `milestone`, `lesson`, `summary`,
`resolution`, `problem`, `pivot`, `observation`, `archived` by default;
`tool_call` and message kinds are left out). The text searched is
`payload.content`, else `payload.text`, plus the tags. Hot and warm
events are covered; cold ones are not.

```json
{ "query": "why did we move the cutover", "layers": ["semantic", "episodic"] }
```

An episodic row looks like this:

```json
{
  "id": "01K…", "layer": "episodic", "kind": "decision",
  "content": "moved the cutover to zeta-9", "tags": [], "scope": "work",
  "created_at": "2026-09-14T10:02:11+00:00", "retrieval_weight": 1.0,
  "tier": "warm", "match": "keyword", "score": null, "similarity": null,
  "rank": 0.65
}
```

Every row, from either layer, has `layer` and `rank`, and the list is
ordered by `rank`. For a memory, `rank` is its similarity plus its
keyword evidence. An event has no similarity, so its rank is built from
the keyword evidence alone, scaled to the same range:

```
rank = 0.8 × (0.8125 × coverage² + 0.1 if the query is a phrase in it) × retrieval_weight
```

`coverage` is the share of the query's words the event contains, rare
words counting for more. An event containing every query word ranks
0.65, about where a memory at cosine similarity 0.65 would. Things to
know:

- The default is unchanged. Without `layers`, `recall` returns the same
  rows in the same order as before 1.6.
- A query word the events never use counts against coverage, and common
  English question words (`what`, `the`, `did`, …) are ignored. Events
  covering less than a quarter of the query are not returned.
- Events of kind `archived`, and events recorded with
  `retrieval_weight` 0.3 or less, are ranked at half: demoted, not
  hidden.
- `min_similarity` compares against an event's `rank`. `kind` keeps
  events of the same kind name (`decision`). `tags`, `scope`, `since`
  and `until` apply to both layers. `mode: "semantic"` returns no
  events.
- `content` is the searched text. The full payload is on
  `recall_recent`.
- Returned hot events have their `last_accessed` refreshed afterwards,
  at most once per 24 hours per event, so events that keep being used
  are not moved to warm on age alone. `[episodic] touch_on_recall =
  false` turns that off. Warm events are returned but not rewritten.

### Time window and scopes on `recall`

`since` and `until` take an RFC3339 timestamp or a 26-character ULID,
as on `recall_recent`, and bound the result to `[since, until)`. The
filter is applied after the search, and the search is widened until the
window is filled or the index is exhausted, so a narrow window does not
come back short. `time_field` chooses `created_at` (default) or
`updated_at`; events are never updated, so for them both mean
`created_at`.

`scope` accepts a comma-separated list (`"projectx,global"`); a row
passes if it is in any of them.

## Tool annotations

Every entry in `tools/list` carries a `title` and an `annotations`
object with all four MCP behaviour hints stated explicitly (the spec's
defaults for `destructiveHint` and `openWorldHint` are both `true`,
which is wrong for most of this surface):

| Tools | `readOnlyHint` | `destructiveHint` | `idempotentHint` |
|---|---|---|---|
| `recall`, `recall_recent`, `summarize_session`, `stats`, `list_scopes`, `export` | `true` | `false` | `true` |
| `remember`, `pin`, `record_event` | `false` | `false` | `false` |
| `update`, `forget`, `unpin` | `false` | `true` | `true` |
| `switch_scope` | `false` | `false` | `true` |

`openWorldHint` is `false` everywhere: no tool path makes a network
call. The additive writes are non-idempotent because each call mints a
new ULID. `remember` is the exception to "calls multiply": a repeat with
identical content in the same scope is a no-op (see
[§Retries and exact duplicates](#retries-and-exact-duplicates)), but the
annotation stays `false` because that does not hold when `supersedes` is
passed. `record_event` with the same arguments records a second event.

Hosts use these to decide what needs a confirmation prompt. Before v1.3
mneme emitted no annotations at all, so a host could not tell `forget`
from `stats`.

## Scoring (auto-context)

When the orchestrator assembles `mneme://context`, each layer is scored
its own way:

| Layer | Score | Notes |
|-------|-------|-------|
| L0 Procedural | — | Not scored. Every pin is kept. |
| L1 Working session | `0.9 × recency` | Recency has a 14-day half-life. |
| L3 Episodic | `retrieval_weight × 0.8 × recency × (β + (1−β)·lex)` | Recency: 14-day half-life on `last_accessed`. `lex` is the share of the query the event contains (IDF-weighted, 0 to 1), from the episodic keyword index. `β` is `episodic_query_mix` (0.3). With no `?q=`, `lex` is 1. |
| L4 Semantic | `rank × 0.7 × freshness` | `rank` is similarity plus keyword evidence, the value `recall` reports. `freshness = max(floor, 2^(−age/half_life))`, age measured from `updated_at`, with `semantic_half_life_days` (90) and `semantic_recency_floor` (0.75). |

Scores order the rows *within* the budget; they do not decide how much
each layer gets. That is the job of the layer shares:

| Layer | Share of `auto_context_token_budget` |
|-------|--------------------------------------|
| L0 Procedural | Never trimmed. |
| L1 Working session | At most `working_max_share` (5%). |
| L3 Episodic | At most `episodic_max_share` (35%). |
| L4 Semantic | At least `semantic_min_share` (30%), or `semantic_min_items` (5) rows if that is fewer tokens. No ceiling. |

Packing order: pins first; then the semantic reserve, best rows first;
then everything else together, best score first, with events and turns
stopping at their ceilings. Fresh events and turns outscore memories, so
they fill their shares and memories take what is left. A row that does not
fit is skipped and smaller rows after it are still tried.

Pins are binding. If they alone exceed the budget, `total_tokens` is
larger than `max_tokens` and the other layers are empty; the server logs a
warning once pins pass 30% of the budget.

L4 takes part only when the read carries a `?q=` seed. Without one the
semantic section is empty and events are ranked by recency alone.

> **Changed in v1.6.** Before 1.6 every layer was reserved 1/16 of the
> budget and the remainder went by score. Memories decayed on a 14-day
> half-life from `created_at`, so a six-week-old memory at similarity 0.58
> scored 0.05 against 0.8 for a fresh event, and events were charged at
> full payload size. In one store events took 77% of the context and two
> memories fit. Events also ignored the query, so every prompt got the same
> list.

## Edit time and archiving

A memory has two timestamps. `created_at` is when it was first stored and
never changes. `updated_at` is when its **content** last changed through
`update`. Changing only `kind`, `tags`, `scope` or `status`, or sending the
same text again, leaves it alone. A memory that was never edited reports
its `created_at`. `recall` rows, `mneme://context` semantic rows and
`export` carry both.

Auto-context measures a memory's freshness from `updated_at`, so a
"current state" memory that is revised in place stays fresh. `recall` does
not decay by age at all.

`update(id, status: "archived")` marks a memory as no longer current
without deleting it:

| Surface | Archived memory |
|---------|-----------------|
| `recall` | Left out. |
| `mneme://context` | Never included. |
| `export` | Included, with `"status": "archived"`. |
| `remember` | Not offered as a duplicate or related memory, and an identical new memory is stored rather than deduplicated against it. |
| `update(id, status: "active")` | Restores it. |

It keeps its id, content, vector and `updated_at`. Use `supersedes` when a
newer memory replaces an old one, and `status: "archived"` when a fact
simply stopped being current.

Both values are stored beside the memory's row rather than in it (keys
`mts:<id>` and `st:<id>`, present only for an edited or an archived
memory), so stores written by earlier versions need no migration. `forget`
removes them with the memory.

## Hybrid recall

`recall` (and the `semantic` section of `mneme://context?q=…`) runs two
searches and merges them:

- **Vector search** over the embeddings, as before.
- **Keyword search**: BM25 over an in-memory inverted index of memory
  contents. Identifiers are indexed whole *and* in parts —
  `live_bt.py` is the terms `live_bt.py`, `live`, `bt`, `py` — so a
  query for either form matches. Single letters count ("D account"),
  Unicode letters count, and there's no stemming (inflections are the
  vector side's job).

Ranking starts from each memory's cosine similarity to the query (also
computed for memories only the keyword search found), so with no literal
evidence the order is exactly the vector order. A memory that contains
the query's terms gains up to +0.2, scaled by the square of how much of
the query it covers (IDF-weighted, so `live_bt.py` counts for far more
than `the`), and +0.1 more if it contains the query as a contiguous
phrase. Every filter, `min_similarity` included, applies to both kinds
of hit.

`mode: "keyword"` is **not an exact-phrase search**. It returns memories
that share *any* word with the query (identifier parts count), best BM25
match first, then ranks them by how much of the query each one covers; a
contiguous run of the query's words earns only the smaller phrase bonus
(0.1, against up to 0.2 for coverage).
In keyword mode that evidence leads and cosine similarity only breaks
ties. So `"D account"` puts a memory containing those words next to each
other first, but a memory that only mentions `account` can still come
back behind it. Use `keyword` when near-synonyms would be noise (an env
var, an id, error text); keep `hybrid` otherwise. For a broad question
("what do we know about billing?") filter by `tags` rather than
`min_similarity`: the floor drops weak matches, which is the wrong trade
when the answer is spread across loosely related memories.

Measured on a corpus of realistic agent memories full of lookalikes
(`live_bt.py` beside `live_mm.py` and `live_bt_old.py`, accounts A–G,
`IBKR_GATEWAY_PORT` beside `…_HOST` and `…_USER`, ports 9090/9091):

| model | query type | semantic top-1 | hybrid top-1 |
|---|---|---|---|
| MiniLM-L6 | exact name | 6 / 10 | 10 / 10 |
| MiniLM-L6 | name inside a question | 6 / 8 | 8 / 8 |
| MiniLM-L6 | natural-language question | 9 / 10 | 10 / 10 |
| BGE-M3 | exact name | 9 / 10 | 10 / 10 |
| BGE-M3 | name inside a question | 8 / 8 | 8 / 8 |
| BGE-M3 | natural-language question | 10 / 10 | 10 / 10 |

The keyword index is built from storage on the first `recall` after
startup (one pass over the stored memories) and kept current by every
write. It exists only in memory, so it adds nothing to the data
directory and nothing to encrypt.

## Relations

A memory can say how it stands to something else. Pass `relations` to
`remember` or `update`:

```json
{"name": "remember", "arguments": {
  "content": "Gateway listens on 9090 since the March move",
  "relations": [
    {"type": "refines", "id": "01K8Z…", "reason": "port changed in March"},
    {"type": "evidence_for", "ref": "path:/etc/gateway.toml"}
  ]}}
```

| Type | Use when | Effect on the target |
|---|---|---|
| `supersedes` | The new memory replaces the old one outright. | Hidden from `recall` and auto-context, as with the `supersedes` argument. |
| `refines` | The new memory narrows, updates or corrects *part* of the old one. | Demoted: still returned, ranked lower. |
| `contradicts` | The new memory disputes the old one. | Demoted, as for `refines`. |
| `evidence_for` | The target is what this memory is based on. | None. |

Each entry has a `type`, a target and an optional `reason` (at most 300
characters). The target is an `id` or a `ref`:

- `id` is a memory id. For `evidence_for` it may also be an event id.
- `ref` is an external reference, `evidence_for` only: any string of up to
  128 bytes, such as `research:pricing-study`, `path:/docs/adr-7.md` or
  `kanban:t_142`. mneme stores it and matches it exactly; it does not
  parse, resolve or check it.

Input is validated before anything is written. An unknown `type`, both or
neither of `id` and `ref`, an `id` that names nothing, a relation from a
memory to itself, or a `reason` or `ref` over its limit is `INVALID_PARAMS`
(-32602), and a rejected `remember` stores nothing. Writing a relation that
already exists replaces its reason. There is no argument that removes a
relation: forget the memory that carries it.

### Demotion

`supersedes` is all-or-nothing, which is why it goes unused for partial
corrections: superseding a memory to fix one detail hides everything else
it said. `refines` and `contradicts` push the older memory down instead.

A memory is **demoted** while another memory refines or contradicts it and
that memory is:

- still stored,
- current (not archived and not itself superseded), and
- newer by `updated_at`.

A demoted memory's keyword evidence is not counted and its `rank` is
lowered by `[recall] demote_margin` (0.10), in `hybrid`, `semantic` and
`keyword` mode alike. Its row carries `demoted: true`. This is what stops a
stale fact from outranking the current one because the question happens to
use the stale wording.

Demotion is undone by forgetting or archiving the newer memory, or by
revising the older one afterwards (its `updated_at` then passes the
refiner's). `mneme://context` leaves demoted memories out; set
`[auto_context] include_demoted = true` to keep them.

### Ties

Without any relation, two memories can still rank the same: a fact and
its corrected version are close in meaning and share most of their
words. `recall` orders rows of equal `rank` (within 0.0001) by
`updated_at`, newest first, in every mode, so the version edited last
leads even when nobody has linked the two. Before 1.6 the order of tied
rows was whatever the search returned, and in keyword mode that was the
older memory.

### Relations on a row

`recall` and `mneme://context` rows carry a `relations` object when the
memory has any. Keys appear only when non-empty:

| Key | Value |
|---|---|
| `superseded_by` | Id of the memory that replaced this one. |
| `refined_by`, `contradicted_by` | `[{id, reason}]`: memories that refine or contradict this one. |
| `refines`, `contradicts`, `supersedes` | `[{id, reason}]`: memories this one refines, contradicts or supersedes. |
| `evidence` | What this memory cites: `{id}` for a memory, `{id, layer: "episodic"}` for an event, `{ref}` for a reference; each with `reason` if one was given. |

A relation whose other end is a memory that has since been forgotten is
not shown. `expand: 1` on `recall` adds `linked` to each row with
relations: `[{id, layer, content}]`, the first 120 characters of every
linked memory or event, so the agent can see what a relation points at
without a second call.

### Recall by reference

`recall` with `ref` is a lookup, not a search: it returns the memories that
have a relation to exactly that reference.

```json
{"name": "recall", "arguments": {"ref": "research:pricing-study"}}
```

`query` is optional with `ref`. Without it the rows are ordered newest
edit first and `score`, `similarity` and `rank` are `null`; with it they
are ordered by similarity to the query. `match` is `"ref"`. Scope, kind,
tag, time-window, archived and provenance filters apply; `layers` and
`mode` do not.

### Storage

Relations are stored beside the memory, not in it, so stores written by
earlier versions need no migration: a forward row
`rel:<from><type><tag><target>` and the same relation keyed from the
target, `rrel:<tag><target><type><from>`, each holding the reason and
creation time. `forget` removes a memory's relations in both directions.
`export` lists the relations each memory carries; `mneme backup` copies
them with the rest of the store.

## Provenance

`remember` and `update` take `provenance`, how the writer came to know the
fact, and `source`, where from:

| `provenance` | Meaning |
|---|---|
| `user_stated` | The user said it. |
| `observed` | The agent saw it: a file, a tool result, a log. |
| `inferred` | The agent concluded it. |
| `imported` | Brought in from another system. |

`source` is free-form (`telegram:…`, `research:…`, `tool:…`), at most 200
characters, and needs `provenance`. Both are optional; a memory stored
without them has none, which is every memory written before 1.6.

Rows from `recall`, `mneme://context` and `export` carry `provenance` and
`source` when set. `recall(provenance: "inferred")` keeps only memories
with that value; memories with none, and events, never match. `stats`
reports `memories.by_provenance`, with `unspecified` for the rest.

Provenance does not affect ranking. It is there for review: "inferred,
older than a month, never confirmed" is a list worth reading.

## Promoting an event

`record_event` with `promote` writes the event and, in the same call, a
long-term memory holding the event's text:

```json
{"name": "record_event", "arguments": {
  "kind": "decision",
  "payload": {"content": "Ship the importer behind a flag"},
  "promote": {"type": "decision", "tags": ["importer"]}}}
```

The memory takes the payload's `content` (or `text`), the event's scope,
`promote.type` as its kind (default `fact`) and `promote.tags` (default:
the event's tags). It carries an `evidence_for` relation to the event, and
the reply reports its id in `_meta.promoted_memory_id`. Use it for an
event whose outcome should stay findable by meaning after the timeline
has moved on; events are only ever found by their words.

Nothing is promoted unless asked: the agent decides what is worth keeping.
If the same text is already a current memory in that scope, no second
memory is written and the existing one gets the link to the new event.

## Superseding and similarity advisories

Facts change: a balance moves, a decision is reversed, a port is
reassigned. Storing the new version next to the old one leaves both
competing in `recall`, so `remember` and `update` take `supersedes`:

```json
{"name": "remember", "arguments": {"content": "D account balance is 4,800 EUR (2026-10-04)",
                                   "supersedes": "01K8Z…"}}
```

The superseded memory is not deleted. It stays readable by id (`forget`
and exports still see it), but `recall` and `mneme://context` skip it
unless `include_superseded: true`. Forgetting the replacement brings the
original back, so a mistaken supersede is undone by forgetting the new
memory.

A `supersedes` argument is the same thing as a `supersedes` entry in
`relations` without a reason; either way the link also appears in the
rows' `relations`.

To help the agent notice when it should supersede, every `remember`
reply names the closest current memory once similarity reaches 0.80:

```text
stored memory 01K90…
note: close to memory 01K8Z… (similarity 0.95): "D account balance is 5,200 EUR (2026-10-01)".
If this replaces it, call `update` on the new id with `supersedes: "01K8Z…"`; if it only
restates it, `forget` the new id.
```

The same information is in `_meta.related_memory`, or
`_meta.duplicate_advisory` from 0.95 up (the pre-1.5 name), with
`existing_id`, `existing_content`, `existing_scope`, `similarity` and
`message`. It's in the text as well because some MCP hosts never show
`_meta` to the model.

Why 0.80, and why one piece of advice for both cases: measured with
BGE-M3 on realistic agent memories, a paraphrased restatement scores
0.82–0.96, the same fact with an updated value 0.83–0.95, a different
fact about the same subject 0.55–0.79, and unrelated memories 0.36–0.48.
Restatements and updates overlap completely, so mneme can't tell them
apart; it points at the candidate and lets the agent decide.

## Retries and exact duplicates

A `remember` that times out or whose reply is lost tends to be retried.
Instead of storing a second copy, `remember` looks at the 16 nearest
existing memories (the dedup probe, `DEDUP_PROBE`; wider than the single
closest hit the similarity advisory uses, so an exact twin is found even
when other near-identical memories rank above it). If one of them is
**current** (not superseded), in the **same scope**, and has identical
content after trimming and collapsing runs of whitespace, nothing is
written:

```text
already stored as 01K90…
```

with `_meta = {"deduplicated": true, "id": "01K90…"}`. The first memory's
`type` and `tags` stand; the retry's are ignored. When the retry passed a
`type`, or non-empty `tags`, that differ from the stored memory's, the
reply says so:

```text
already stored as 01K90…; existing type/tags kept
```

The suffix appears only in that case. A plain retry, or one that repeats the
same `type` and `tags` (or passes neither), gets the bare
`already stored as <id>`. Different content,
a different scope, or a superseded twin stores normally, and dedup is
skipped when `supersedes` is passed. Two simultaneous identical calls can
both land (the probe and the write are not one atomic step); a sequential
retry cannot. There is no `idempotency_key` argument: this covers the
retry case without one.

## Write guardrails

`remember`, `update` (when `content` is present), `pin` and `record_event`
(every string inside `payload`) scan what they are given for credentials
before writing. The reject list is deliberately narrow, fixed prefixes with
long random bodies, so false positives stay rare:

| Pattern name | Matches |
|---|---|
| `aws_access_key_id` | An AWS access key id. |
| `pem_private_key` | A `-----BEGIN … PRIVATE KEY-----` header. |
| `jwt` | A JSON Web Token (three base64url segments starting `eyJ`). |
| `github_token` | A GitHub personal, OAuth, user, server or refresh token (`gh?_…`). |
| `github_fine_grained_token` | A `github_pat_…` token. |
| `slack_token` | A Slack `xox?-…` token. |
| `openai_or_anthropic_key` | An OpenAI or Anthropic `sk-…` key. |
| `google_api_key` | A Google `AIza…` API key. |
| `stripe_key` | A Stripe secret or restricted key (`sk_`/`rk_` live or test). |
| `age_secret_key` | An `AGE-SECRET-KEY-1…` key. |
| `generic_credential_assignment` | `password`, `passwd`, `secret`, `token` or `api_key` followed by `:` or `=` and a value. **Warn-only: never rejects.** |

The exact shapes are tuned over time; the names above are what the error
and advisory report.

`[guardrails] secrets` picks the behaviour (default `reject`; see
[Configuration](./configuration.md#guardrails--write-time-checks)):

- **`reject`** returns a soft error shaped like the size-tier rejection:
  `isError: true`, a human-readable text, and

  ```json
  {"error": {"code": "secret_detected", "pattern": "github_token", "offset": 42}}
  ```

  `offset` counts characters from the start of the scanned string; for
  `record_event` an extra `path` (for example `/content`) says which string.
  The matched text is never echoed or logged, and the automatic
  `tool_call_failed` event records only the code.
- **`warn`** stores the write, adds `_meta.secret_advisory` with the
  same `pattern` and `offset`, plus `high_confidence` and `message`, and
  appends a one-line warning to the reply text naming the pattern, the
  character offset and the memory id to `forget`, because some hosts never
  show `_meta` to the model. For `record_event` the warning also names the
  payload `path` (for example `at char 12 of /content`), so the agent can
  tell which string matched.
- **`off`** skips the scan, the generic pattern included.

The automatic `tool_call` event normally copies the `content` of
`remember`, `pin` and `update` calls into the timeline. When the write
carried a `secret_advisory` (in `warn` mode, or the warn-only generic
pattern in any scanning mode) that copy omits `content` and `text`, so the
credential lives only in the memory it was written to and `forget` removes
it entirely. Under `off` nothing is scanned and the copy is made as usual.

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

## Consolidation status

`stats` and `mneme://stats` share one `consolidation` object, built by the
same function so they cannot drift. Besides the scheduler's run counters
(`runs_total`, `errors_total`, `last_consolidation_at`,
`last_promoted_to_warm`, `last_archived_to_cold`, `last_orphans_reclaimed`)
it answers "why is nothing promoted?":

| Field | Meaning |
|---|---|
| `thresholds` | `{"hot_to_warm_days": 28, "warm_to_cold_days": 180}` — the values in effect. |
| `hot_oldest_age_days` | Age of the oldest hot event in days (two decimals), measured on `last_accessed`, the field consolidation ages on. `null` when the hot tier is empty. |
| `hot_eligible_for_warm` | Hot events already at or past the threshold (`last_accessed <=` the cutoff, the comparison the pass uses). `0` when the tier is empty. |
| `next_promotion_in_days` | Days until the oldest hot event becomes eligible: `hot_to_warm_days` minus `hot_oldest_age_days`, floored at 0, and `0` whenever something is eligible. `null` when the hot tier is empty. |

With the defaults the first promotion cannot happen before an event is 28
days old: hundreds of runs with `last_promoted_to_warm: 0` during the first
four weeks are expected. The top level of both also carries `version`
(the mneme crate version). The hot-tier figures come from the same single
scan that produces the `hot` count.

## Scopes

A scope labels a memory, pin or event. Write tools without a `scope`
argument use the session's current scope; `recall`, `recall_recent`, `forget`
and `mneme://context` without one look across all scopes.

- The current scope is held **per MCP session** and starts from
  `MNEME_SCOPE`, then `[scopes] default` (or the client's roots with
  `derive_from_roots`). `switch_scope` changes it for that session only, and
  it resets on reconnect. `MNEME_SCOPE` is read by the process that serves
  (`mneme run`, `mneme serve`, `mneme daemon`), not by `mneme client`.
- Agents that span topics should pass `scope` explicitly on every write and
  filtered read.
- **Scopes are not a security boundary.** Any client holding the
  token can read every scope, and there is one token per instance. For
  confidential data, run a second instance with its own data directory and
  token.

## HTTP endpoints

Beyond `POST /mcp` (and `DELETE /mcp`), the Streamable HTTP transport serves:

| Endpoint | Auth | Answer |
|---|---|---|
| `GET /healthz` | none | `200 ok` — the process is up. |
| `GET /health` | none | Alias of `/healthz` (added after 1.4.1). |
| `GET /version` | bearer token | `{"version": "<crate version>"}`. |

The health probes are open so orchestrators can use them without a secret;
they disclose nothing but liveness. The version is behind the token.
