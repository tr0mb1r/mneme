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
| `remember` | L4 semantic | The user shares a fact, decision, or preference that should persist across sessions. **Size guidance:** target under 500 chars; 500–2k accepted with a `length_advisory`, 2k–10k with a `length_warning`, over 10k rejected with a structured error. See [§Size guardrails](#size-guardrails) below. **Replacing a fact:** pass `supersedes` (an id or array of ids) and the old memories drop out of `recall` and auto-context while staying readable by id. When the nearest current memory has similarity ≥ 0.80 the reply names it, both in the text and in `_meta` (`related_memory`, or `duplicate_advisory` at ≥ 0.95). See [§Superseding and similarity advisories](#superseding-and-similarity-advisories). **Retries are safe:** content identical to a current memory in the same scope (ignoring leading/trailing and repeated whitespace) writes nothing and replies `already stored as <id>` with `_meta.deduplicated: true` — see [§Retries and exact duplicates](#retries-and-exact-duplicates). Content with a credential in a well-known format is rejected — see [§Write guardrails](#write-guardrails). |
| `update` | L4 semantic | The user revises an existing memory; re-embeds automatically when `content` changes. Also takes `supersedes`, to retire older memories after the fact (e.g. after `remember` reported a close match). New `content` goes through the same [secret guard](#write-guardrails) as `remember`. |
| `forget` | L4 semantic, L0 procedural, L3 episodic (hot+warm) | The user explicitly asks to remove a memory. `id=…` resolves the ULID across all three layers, first hit wins; cold-archive entries stay out of reach by design. Confirm before calling. |
| `pin` | L0 procedural | A *rule* should surface on every recall context (e.g. "always use `uv`, not `pip`"). Passes the [secret guard](#write-guardrails). To replace a rule, `unpin` the old one and `pin` the new. |
| `unpin` | L0 procedural | A previously-pinned rule no longer applies. |
| `record_event` | L3 episodic (+ L1 mirror for message kinds) | Capture a structured event with a free-form `kind`. Use `user_message`/`assistant_message` for conversation turns; `decision`/`milestone`/`pivot`/etc. for curated semantic events; `summary` after `summarize_session` to land a digest. **`payload` is required** and must carry content, e.g. `{"content": "<what happened>"}`; there is no top-level `text` or `content` argument, a bare boolean or number payload counts as empty, and (as for every tool) undeclared arguments are rejected with `INVALID_PARAMS` naming the keys rather than ignored. See [§Event kinds](#event-kinds) below. |

### Memory reads

| Tool | Layer | Use when |
|------|-------|----------|
| `recall` | L4 semantic | Hybrid search — meaning *and* exact words, fused, so both questions and exact names (`live_bt.py`, "D account", an env var, an error string) work. `mode` picks `hybrid` (default), `semantic`, or `keyword`; each row says which search found it in `match` (`semantic` / `keyword` / `both`). See [§Hybrid recall](#hybrid-recall). Filter with `scope`, `type`, `tags` (a memory must carry *every* listed tag), and `min_similarity` (cosine floor, `1.0` = identical; omit to take the nearest `limit` however far away they are). Each row carries both `score` (cosine *distance*, lower is closer) and `similarity` (`1 - score`, the orientation `min_similarity` uses). Superseded memories are left out unless `include_superseded: true`, in which case they come after every current match and carry `superseded_by`. |
| `recall_recent` | L3 episodic | "What did we just do?" — time-ordered events (tool calls, lifecycle events, conversation, decisions). Optional `since` / `until` bound the result to a `[since, until)` window against `created_at` (RFC3339 or 26-char ULID); when either bound is set, `limit` caps at 1000 instead of 200. The server does not parse natural language — convert phrases like "last Tuesday" to RFC3339 client-side before calling. |

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
| `summary` | `{"text": "<digest>", "covers": ["<event_id>", ...]}` | Output of the `summarize_session` workflow. The Hermes context hook also records each compaction summary this way, with `"source": "hermes-compaction"`. |

Server-emitted kinds (the agent never writes these — they appear in
`recall_recent` results because the server records them on its own):

| Kind | Trigger | Payload |
|---|---|---|
| `tool_call` | Successful `tools/call` dispatch, subject to `[episodic] auto_log_tool_calls` (default `writes`: read-only tools are not mirrored) | Per-tool value-bearing arg(s) only — no full args object (privacy). For `remember`, `pin` and `update` that includes `content`. |
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
| `mneme://context` | Pre-assembled prompt context: pinned rules + working-session turns + recent events, packed against a token budget. Append `?q=<text>` to fold in semantically-similar long-term memories and `?scope=<name>` to restrict to one scope. Auto-logged event kinds are left out unless asked for — see [§Auto-context parameters](#auto-context-parameters). | On session start — single-call replacement for reading procedural + recent + recall separately. |
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
| `scope` | Restricts L0 / L3 / L4 to one scope. L1 working turns are session-local and always included. |
| `limit` | How many semantic candidates to over-fetch before the token-budget pass trims. Clamped server-side. |
| `kinds` | Comma-separated event kinds. The `episodic` section then holds only these. Naming an auto-logged kind opts back in to it. |
| `exclude_kinds` | Comma-separated event kinds to hide from `episodic`, in addition to the default exclusions. |

```
mneme://context                             → L0 + L1 + L3
mneme://context?q=how%20do%20we%20deploy     → + L4 semantic hits
mneme://context?q=deploy&scope=work          → all four layers, work scope only
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

The response body has one array per layer (`procedural`, `working`,
`episodic`, `semantic`), a `request` block echoing how the URI was
interpreted (`query`, `scope`, `kinds`, `exclude_kinds`; so "no hits" is
distinguishable from "no query supplied"), and `total_tokens` / `max_tokens`. The reply's `uri` echoes exactly what
was requested, parameters included, so a client can correlate it.

Parsing is deliberately lenient — an unknown key, a malformed `limit`,
or a stray `&` yields a request that still assembles something useful.
Auto-context is read unattended; failing it closed would leave the
session with no memory at all.

> **Changed in v1.3.** Before v1.3 the resource passed no query and
> emitted a hardcoded empty `semantic` array, so nothing written with
> `remember` could reach auto-context. The `working` section was also
> missing from the body while its turns were still charged against the
> token budget. Both fixed; the query parameters are new.

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

L4's weight applies only when the read carries a `?q=` seed; without one
the layer contributes nothing to score against.

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
