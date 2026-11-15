# Configuration

`mneme init` writes a `~/.mneme/config.toml` with every default made
explicit. Every section is optional — if you delete a section or a
field, mneme falls back to the value documented here. A missing
config file is also fine: mneme uses defaults for every field.

This page is the canonical reference for every parameter — what it
does, what values it accepts, when to change it, and what the trade-
offs are. Changes take effect on the next `mneme run`.

## Quick reference

`mneme init` writes exactly the keys below. Three more
(`[storage] encryption`, `[mcp] sse_port`, and the whole `[telemetry]`
section) still *load* for backwards compatibility but have no effect —
see [Reserved settings](#reserved-settings).

```toml
[storage]
data_dir   = "~/.mneme"
max_size_gb = 10

[embeddings]
model      = "bge-m3"     # or "minilm-l6"
device     = "auto"       # "auto" / "cpu" / "cuda" / "metal"
batch_size = 32

[scopes]
default           = "global"
derive_from_roots = false

[checkpoints]
session_interval_secs   = 30
session_interval_turns  = 5
hnsw_snapshot_inserts   = 1000
hnsw_snapshot_minutes   = 60

[episodic]
auto_log_tool_calls  = "writes"  # or "all" / "off"
log_tool_call_values = "full"    # or "ids" / "off"
index_kinds          = ["decision", "milestone", "lesson", "summary", "resolution",
                        "problem", "pivot", "observation", "archived"]
touch_on_recall      = true

[working]
record_tool_turns = "writes"     # or "all" / "off"

[auto_context]
semantic_half_life_days = 90.0
semantic_recency_floor  = 0.75
episodic_max_share      = 0.35
semantic_min_share      = 0.3
semantic_min_items      = 5
working_max_share       = 0.05
episodic_query_mix      = 0.3
event_clip_chars        = 400
min_event_weight        = 0.3
max_events_per_kind     = 6
pin_scopes              = ["global"]
include_demoted         = false

[recall]
demote_margin = 0.1

[consolidation]
hot_to_warm_days  = 28
warm_to_cold_days = 180
schedule          = "idle"
cold_exempt_kinds = ["archived", "summary", "milestone", "lesson", "decision"]

[budgets]
default_recall_limit       = 10
auto_context_token_budget  = 4000
max_remember_chars         = 10000

[auto_context]
semantic_half_life_days = 90     # 0 = no decay for memories
semantic_recency_floor  = 0.75
episodic_max_share      = 0.35
semantic_min_share      = 0.30
semantic_min_items      = 5
working_max_share       = 0.05
episodic_query_mix      = 0.3
event_clip_chars        = 400    # 0 = do not clip
min_event_weight        = 0.3
max_events_per_kind     = 6      # 0 = no cap
pin_scopes              = ["global"]

[guardrails]
secrets = "reject"              # or "warn" / "off"

[mcp]
transport = "stdio"

[daemon]
idle_timeout_minutes = 30
log_level            = "default"

[http]
enabled              = false
bind                 = "127.0.0.1:7878"
token_file           = ""
allowed_origins      = []
max_sessions         = 256
session_idle_minutes = 1440

[logging]
level       = "info"
file        = "~/.mneme/logs/mneme.log"
max_size_mb = 100
max_files   = 5
```

---

## Reserved settings

Three settings are accepted on load but have **no effect**. They exist
so a `config.toml` written by an older release keeps working; since v1.3
`mneme init` no longer writes them into new configs, because handing
someone a knob that does nothing is worse than omitting it.

| Setting | What it actually does |
|---|---|
| `[storage] encryption` | Nothing. Encryption is gated by the presence of `~/.mneme/keystore.json` — run `mneme encrypt`. |
| `[mcp] sse_port` | Nothing. Superseded by [`[http] bind`](#http--streamable-http-listener); `[mcp] transport` isn't consulted either. |
| `[telemetry] enabled` / `endpoint` | Nothing. There is no telemetry subsystem, and no code path in the binary makes a network call. |

If you have these in your config, you can delete them or leave them —
either way behaviour is identical. See
[Roadmap](./roadmap.md#reserved-configuration).

`[consolidation] schedule` is a partial case: it *is* read, but only
`"idle"` is implemented. Any other value logs a warning and falls back.

---

## `[storage]` — disk layout and size limits

### `data_dir`

| | |
|---|---|
| **Type** | path |
| **Default** | `~/.mneme` |
| **Affects** | every on-disk artifact (config, models, redb, WAL, snapshots, sessions, cold archive, logs) |

The root for everything mneme writes. Override per project to keep
work and personal memories isolated:

```sh
# In <repo>/.envrc with direnv:
export MNEME_DATA_DIR="$HOME/.mneme-myrepo"
```

The environment variable `MNEME_DATA_DIR` overrides this field at
boot. See [environment overrides](#environment-overrides).

### `max_size_gb`

| | |
|---|---|
| **Type** | unsigned integer (GB) |
| **Default** | `10` |
| **Affects** | reserved disk-space hint; not yet enforced (advisory) |

Reserved for future quota enforcement. Today it's a documentation
field — the storage layer doesn't currently refuse writes when the
data dir crosses this number. Tracked under `Phase 7 polish`.

### `encryption`

| | |
|---|---|
| **Type** | bool |
| **Default** | `false` |
| **Affects** | nothing |

**Reserved — setting this does nothing**, in either direction. Whether
mneme is running encrypted is decided by the presence of
`~/.mneme/keystore.json`. To enable encryption, run `mneme encrypt`; to
disable it, run `mneme decrypt --yes-i-really-mean-it`. See the
[Encryption at rest](./encryption.md) chapter for the full workflow,
including how to recover on a new machine via the 12-word BIP39
mnemonic. `mneme init` no longer writes this key.

Earlier mneme docs said this flag would be wired to a future opt-in;
v1.2 ships the feature with `mneme encrypt` as the user-facing
trigger instead, because tying it to a config field would silently
re-encrypt or decrypt the data dir on the next daemon boot — a
foot-gun for users who edit `config.toml` without realising what it
implies.

---

## `[embeddings]` — model + device for the L4 vector index

### `model`

| | |
|---|---|
| **Type** | string |
| **Default** | `"bge-m3"` |
| **Affects** | every L4 `remember` / `recall` / `update`; HNSW dimensionality |

Canonical embedder identity. Two models are supported:

| Short name | Full repo | Dim | Approx size | Languages | Recall | Cold-start |
|------------|-----------|-----|-------------|-----------|--------|------------|
| `bge-m3` *(default)* | [`BAAI/bge-m3`](https://huggingface.co/BAAI/bge-m3) | 1024 | ~2.3 GB on disk, ~1.5 GB download | 100+ multilingual | top-tier (state-of-the-art on MTEB at the time of pinning) | several seconds first boot |
| `minilm-l6` | [`sentence-transformers/all-MiniLM-L6-v2`](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) | 384 | ~90 MB | English-only | very good for English; weaker on multilingual content | sub-second |

Pick `minilm-l6` when:
- You're trying mneme out and don't want to wait on a 1.5 GB download.
- All your memories are in English.
- You care about cold-start time more than top-tier recall.

Pick `bge-m3` (the default) when:
- You write or recall in multiple languages.
- You want best-in-class similarity scores.
- The first-boot wait is acceptable.

Unknown values produce a clear error before any I/O — typos won't
silently fall through to a default. Switching models later
re-embeds every stored memory automatically; see
[switching embedders later](#switching-embedders-later).

The `MNEME_EMBEDDER` environment variable can override this field
to the deterministic `stub` embedder for offline tests; see
[environment overrides](#environment-overrides).

### `device`

| | |
|---|---|
| **Type** | string |
| **Default** | `"auto"` |
| **Valid** | `"auto"`, `"cpu"`, `"cuda"`, `"metal"` |
| **Affects** | embedder forward-pass placement |

Where the embedder runs.

- `"auto"` *(default)* — pick CPU. Today this is what `auto`
  resolves to; CUDA / Metal acceleration is a future
  optimization.
- `"cpu"` — explicitly CPU. Equivalent to `"auto"` today.
- `"cuda"` — request the CUDA backend. Currently treated like CPU
  with a warning; CUDA support depends on a future candle build flag
  flip.
- `"metal"` — Apple Silicon GPU. Same status as CUDA today.

Leave at `"auto"` for now. The model loaders log the active
backend on boot so you'll see the resolution in
`~/.mneme/logs/mneme.log`.

### `batch_size`

| | |
|---|---|
| **Type** | unsigned integer |
| **Default** | `32` |
| **Affects** | embedding throughput when many texts are queued |

Maximum batch size the embedder coalesces from the bounded queue.
Higher = better throughput on multi-document writes (e.g.
`summarize_session` followed by `remember`); lower = lower memory
peak. The 32 default keeps RAM bounded on small machines.

---

## `[scopes]` — default scope routing

### `default`

| | |
|---|---|
| **Type** | string |
| **Default** | `"global"` |
| **Affects** | scope arg fallback for `remember` / `pin` |

The session's starting "current scope" — used by write tools when
the caller omits the `scope` argument. The `switch_scope` tool
mutates the in-memory cell; this field is only consulted at boot.

The string is free-form. Common conventions:
- `"global"` *(default)* — cross-project rules, conventions, and
  preferences that apply regardless of which repo you're in
- `"<projectname>"` — one scope per project; agents call
  `switch_scope("<name>")` early in the session
- `"client-x"`, `"client-y"` — confidentiality boundaries

Existing installs with `default = "personal"` in `~/.mneme/config.toml`
still work — the change is the default for fresh installs and the
missing-`config.toml` fallback path.

`list_scopes` shows every distinct scope across the three layers.
Cross-process semantics: see [switch_scope](./mcp-surface.md#session-helpers).

### `derive_from_roots`

| | |
|---|---|
| **Type** | boolean |
| **Default** | `false` |
| **Affects** | the per-connection default scope |

Derive each connection's default scope from the MCP client's declared
workspace roots, so a host opened on `~/code/billing-api` writes into
scope `billing-api` without the agent ever calling `switch_scope`.

The name comes from the first root's `name`, or the last path segment of
its `uri` if no name was given; it is lowercased with unsupported
characters collapsed to `-` (`My Project` → `my-project`). If the client
sends no roots, or nothing survives sanitisation, the configured
`default` stands.

**Off by default on purpose.** Turning it on changes where new memories
land, which means facts stored before the switch stop appearing in a
default-scoped `recall`. That is exactly what someone who wants project
isolation is asking for, and an unpleasant surprise for anyone else.

This is *per connection*, which is what makes it work in daemon mode:
`initialize` arrives once per client, so a Claude Code session in repo A
and a Cursor session in repo B land in different scopes off one shared
daemon.

### `MNEME_SCOPE` (environment)

Not a config key, but it belongs here. Setting `MNEME_SCOPE` in the
environment overrides `[scopes] default` for the process lifetime, and
takes precedence over `derive_from_roots` — explicit beats inferred.
It is sanitised the same way.

Useful for a repo-local MCP config:

```json
{
  "mcpServers": {
    "mneme": {
      "command": "mneme",
      "args": ["run"],
      "env": { "MNEME_SCOPE": "billing-api" }
    }
  }
}
```

**It is read by the process that serves, not by `mneme client`.** That
is `mneme run`, `mneme serve` (set it in the container's environment) or
`mneme daemon`. In daemon mode the host spawns `mneme client`, which is a
byte pipe that never builds a scope cell, so an `env` block on the host's
entry has no effect and the daemon's own environment decides. Use
`derive_from_roots` there. Either way the value is a default for *every*
connection to that instance; `switch_scope` is per connection and is
forgotten on reconnect, so an agent that spans topics should pass `scope`
on each call.

Resolution order, lowest precedence first:

1. `[scopes] default`
2. `MNEME_SCOPE`
3. roots derivation, if `derive_from_roots = true` and `MNEME_SCOPE` is unset
4. `switch_scope` during the session
5. an explicit `scope` argument on the individual tool call

---

## `[checkpoints]` — scheduler cadences

### `session_interval_secs`

| | |
|---|---|
| **Type** | unsigned integer (seconds) |
| **Default** | `30` |
| **Affects** | wall-clock cadence of L1 working-session checkpoints |

The L1 `CheckpointScheduler` flushes the active session to
`~/.mneme/sessions/<id>.snapshot` at this interval whenever there
are pending turns. `0` is not a recognised "off" value — set
`session_interval_turns` to a high number if you want to disable
the wall-clock trigger.

### `session_interval_turns`

| | |
|---|---|
| **Type** | unsigned integer (turn count) |
| **Default** | `5` |
| **Affects** | turn-count trigger of L1 working-session checkpoints |

The same scheduler also fires when `turns_since_last_checkpoint >=
interval_turns`, evaluated whenever the server pokes after a
successful `tools/call` that added a turn. Whichever trigger fires
first wins.

What counts as a turn: a `user_message` / `assistant_message` recorded
with `record_event`, and each successful call to a tool that changes
something (`remember`, `update`, `forget`, `pin`, `unpin`,
`record_event`, `switch_scope`). Read-only tools (`recall`,
`recall_recent`, `summarize_session`, `stats`, `list_scopes`, `export`)
add none since 1.5.2; before that every successful call did, so a
session that only looked things up filled the working window with tool
names and checkpointed for nothing. The rule is the tool's own
`readOnlyHint`, the same one `auto_log_tool_calls = "writes"` uses.

The two are designed together: pure wall-clock under-flushes a
busy session (5 turns of dense back-and-forth in 10 s would sit
unflushed for 30 s), pure turn-count under-flushes a slow session
(one turn followed by silence could sit indefinitely). Either is
the floor.

### `hnsw_snapshot_inserts`

| | |
|---|---|
| **Type** | unsigned integer (insert count) |
| **Default** | `1000` |
| **Affects** | HNSW snapshot frequency, WAL replay time on next boot |

The semantic-layer snapshot scheduler captures a fresh
`<root>/semantic/hnsw.idx` every N successful `remember` /
`forget` / `update` operations. Lower = shorter WAL replay on
restart but more disk I/O during operation; higher = the opposite.

Reasonable range: 100–5000. 1000 is calibrated for typical agent
workloads (~10 writes per active hour ⇒ snapshot every couple of
days).

### `hnsw_snapshot_minutes`

| | |
|---|---|
| **Type** | unsigned integer (minutes) |
| **Default** | `60` |
| **Affects** | HNSW snapshot wall-clock ceiling |

Same scheduler, wall-clock backstop. Whichever of the two triggers
fires first wins. Set to a large number (e.g. `1440` = 24 h) if
you only want insert-count-driven snapshots.

---

## `[episodic]` — what lands on the timeline

### `auto_log_tool_calls`

| | |
|---|---|
| **Type** | string |
| **Default** | `"writes"` |
| **Valid** | `"writes"`, `"all"`, `"off"` |
| **Affects** | which tool calls the server mirrors into L3 as `tool_call` events |

Every successful `tools/call` used to be mirrored into the timeline,
reads included, so verifying a clean-up (`recall`, `stats`) or deleting
test data (`forget`) kept producing fresh events and the timeline never
settled. Now:

- `"writes"`: mirror every tool except those that declare
  `readOnlyHint: true` (`recall`, `recall_recent`, `summarize_session`,
  `stats`, `list_scopes`, `export`). `remember`, `update`, `pin`,
  `unpin`, `forget`, `record_event` and `switch_scope` are still logged,
  so deletions stay auditable.
- `"all"`: mirror every successful call, as in 1.4.1 and earlier.
- `"off"`: mirror nothing, `tool_call_failed` included.

`tool_call_failed` is logged in `"writes"` and `"all"`. The `tool_call`
payload carries the value-bearing argument of write tools
(`remember.content`, for instance), so it is also the place a credential
would be copied to if `[guardrails] secrets` let one through.

### `log_tool_call_values`

| | |
|---|---|
| **Type** | string |
| **Default** | `"full"` |
| **Valid** | `"full"`, `"ids"`, `"off"` |
| **Affects** | how much of a call's arguments a `tool_call` event keeps |
| **Since** | 1.6.0 |

`auto_log_tool_calls` decides *which* calls are mirrored; this decides
*what* a mirrored event holds.

- `"full"`: the value-bearing arguments as passed: `content` for
  `remember` / `pin` / `update`, the query for `recall`, and so on. The
  timeline can answer "what exactly was written, and when". `forget`
  redacts the events that mirrored a forgotten memory (see
  [Forgetting and the timeline](./mcp-surface.md#forgetting-and-the-timeline)).
- `"ids"`: the tool name, the ids (`id`, `memory_id`) and the scope
  (`scope`, `new_scope`). A write's `content` is replaced by
  `content_len` (characters) and `content_sha256` (the first 8 hex digits
  of its SHA-256), enough to see that the same text was written twice
  without storing it. Queries, kinds and limits are dropped. A write the
  secret guard flagged gets neither field.
- `"off"`: `{"tool": "<name>"}` and nothing else.

```json
{"tool": "remember", "scope": "work", "memory_id": "01M7…", "content_sha256": "9f2c41aa", "content_len": 58}
```

Pick `"ids"` when the timeline should show activity but must never be a
second copy of the memories. `forget` works the same in every mode: in
`"ids"` the rows are found by id and lose the hash and length too; in
`"off"` they hold nothing to redact. The setting applies to events
written from then on; it does not rewrite existing rows.

### `index_kinds`

| | |
|---|---|
| **Type** | array of strings |
| **Default** | `["decision", "milestone", "lesson", "summary", "resolution", "problem", "pivot", "observation", "archived"]` |
| **Affects** | which event kinds `recall(layers=[…, "episodic"])` and query-conditioned auto-context can find |
| **Since** | 1.6.0 |

Events of these kinds are keyword-indexed (in memory, hot and warm
tiers). The text indexed is `payload.content`, else `payload.text`, plus
the tags. `tool_call` and the message kinds are left out by default:
they are many, and they repeat text that is already stored elsewhere.
Changing the list takes effect on restart, when the index is rebuilt.

### `touch_on_recall`

| | |
|---|---|
| **Type** | boolean |
| **Default** | `true` |
| **Affects** | whether `recall` refreshes `last_accessed` on the events it returns |
| **Since** | 1.6.0 |

Consolidation moves an event to warm once its `last_accessed` is
`hot_to_warm_days` old. With this on, a hot event returned by `recall`
has its `last_accessed` set to now, so events that keep being used stay
hot. The write happens after the response is sent and at most once per
24 hours per event; warm events are never rewritten. Set it to `false`
for runs that must not write on reads (evaluations, CI): `recall` then
changes nothing.

---

## `[working]` — the L1 working session

### `record_tool_turns`

| | |
|---|---|
| **Type** | string |
| **Default** | `"writes"` |
| **Valid** | `"writes"`, `"all"`, `"off"` |
| **Affects** | which successful tool calls add a turn to the working session |
| **Since** | 1.6.0 (the `"writes"` behaviour since 1.5.2) |

A tool turn is `role = "tool"` with the tool's name as content. It shows
up in the `working` section of `mneme://context`, counts towards
`turns_total`, and counts towards `[checkpoints] session_interval_turns`.

- `"writes"`: only tools that are not `readOnlyHint` (`remember`,
  `update`, `forget`, `pin`, `unpin`, `record_event`, `switch_scope`).
  Looking something up is not conversation.
- `"all"`: every successful call, as up to 1.5.1.
- `"off"`: none. The only turns are the messages an agent records with
  `record_event` (`user_message` / `assistant_message`).

Each MCP session has its own working session: one per stdio process, one
per daemon connection, one per HTTP `Mcp-Session-Id`. See
[L1 is session-local](./mcp-surface.md#l1-is-session-local).

---

## `[consolidation]` — L3 hot/warm/cold tiering

### `hot_to_warm_days`

| | |
|---|---|
| **Type** | unsigned integer (days) |
| **Default** | `28` |
| **Affects** | L3 promotion threshold |

Episodic events older than this (by `last_accessed`) get rekeyed
from the `epi:` prefix to `wepi:` on the next consolidation pass.
Lower = smaller hot-tier scans (faster `recall_recent` on busy
servers); higher = events stay in the agent-facing recent feed
longer.

With the default, nothing is promoted during the first 28 days, however
many times the scheduler runs. `stats` reports the threshold, the age of
the oldest hot event and `next_promotion_in_days` under `consolidation`;
see [Memory layers](./memory-layers.md).

### `warm_to_cold_days`

| | |
|---|---|
| **Type** | unsigned integer (days) |
| **Default** | `180` |
| **Affects** | L3 archival threshold |

Warm events older than this get archived to
`<root>/cold/<YYYY-Qn>.zst` (zstd-compressed JSON bundles) and
deleted from the warm tier. Cold lookups are still possible via
`storage::archive::ColdArchive::find_anywhere(id)` but are off the
hot path — they're for forensics, not auto-context. `recall_recent` and
`export` read them with `include_cold: true`; `recall` does not search
them.

### `cold_exempt_kinds`

| | |
|---|---|
| **Type** | array of strings |
| **Default** | `["archived", "summary", "milestone", "lesson", "decision"]` |
| **Affects** | which event kinds stay in the warm tier for good |
| **Since** | 1.6.0 |

Events of these kinds are never archived to cold, however old. They stay
in redb, so `recall` keeps finding them and `forget` can still delete
them (cold bundles are never rewritten). The cost is that redb keeps
growing with them; these are the low-volume kinds. `[]` restores the
pre-1.6 behaviour, where every kind is archived at `warm_to_cold_days`.
Events already in the cold archive are not brought back.

### `schedule`

| | |
|---|---|
| **Type** | string |
| **Default** | `"idle"` |
| **Valid** | `"idle"` |
| **Affects** | when the consolidation scheduler fires |

Drives `ConsolidationScheduler`'s decision policy. Current cadence
options:

- `"idle"` — wakes every 5 minutes; fires the consolidation pass
  iff no `remember` / `forget` / `update` / `record` happened in
  the prior tick. A burst of writes pushes the next pass back to
  the next quiet window.

Future modes (`"every_<n>m"`, cron expressions, `"on_demand"`)
are tracked for post-v1.1.x. Anything other than `"idle"` today
logs a warning and falls back to `"idle"` cadence.

---

## `[budgets]` — recall + auto-context limits

### `default_recall_limit`

| | |
|---|---|
| **Type** | unsigned integer |
| **Default** | `10` |
| **Affects** | `recall` results when the agent omits `limit` |

Default `limit` for the `recall` tool when the agent doesn't provide
one. `recall` callers can override per-call up to a hard cap of
100.

### `auto_context_token_budget`

| | |
|---|---|
| **Type** | unsigned integer (chars/4 token estimate) |
| **Default** | `4000` |
| **Affects** | `mneme://context` total payload size |

Total token budget for the auto-context resource. The orchestrator
packs the four memory layers (procedural / working / episodic /
semantic) up to this ceiling. How the budget is divided between them is
set in [`[auto_context]`](#auto_context--how-mnemecontext-is-scored-and-packed).

Pinned items are never trimmed. If they alone are larger than this
budget, the context exceeds it and holds nothing else.

The default 4000 is calibrated for a ~32K-token model context window
where you want to spend ~12% on agent memory. Bump for larger
windows (e.g. 16000 for a 128K-token Claude session); shrink if
you're running a smaller model.

The estimator is `chars / 4` per spec §0; replacing it with a real
tokenizer (`tokenizers`/`tiktoken-rs`) is a one-function swap
without a config change.

### `max_remember_chars`

| | |
|---|---|
| **Type** | unsigned integer (characters) |
| **Default** | `10000` |
| **Affects** | `remember`, `update` and `record_event` size ceiling |

Hard ceiling on `remember` / `update` content length (and on the
serialised `record_event` payload). Above this,
the tool returns a structured `memory_too_large` error
(`isError=true`, `_meta.error.code = "memory_too_large"`) suggesting
the agent extract a key insight or summarize first. The verbatim
principle is preserved: existing oversized memories remain readable
and `recall`-able — only new writes/updates above the ceiling are
rejected.

The 500-character advisory boundary and the 2,000-character warning
boundary are deliberately fixed in code (`src/mcp/tools/size_tier.rs`)
and do NOT vary with `max_remember_chars`. They represent the
steady-state size guidance and shouldn't drift per-installation. See
[`book/src/mcp-surface.md` §Size guardrails](mcp-surface.html#size-guardrails)
for the full tier semantics and `_meta` shape.

Raise this only when you have a specific need (research notes, long
runbooks). The advisory + warning meta will still surface above
500/2,000 chars; tightening the ceiling moves only the rejection
threshold.

---

## `[auto_context]` — how `mneme://context` is scored and packed

All keys are optional; a config file without the section gets the
defaults below. Scoring and packing are described in
[MCP surface §Scoring](./mcp-surface.md#scoring-auto-context).

### Layer shares

Fractions of `[budgets] auto_context_token_budget`. Values outside
`0.0`–`1.0` are clamped.

| Key | Default | Meaning |
|---|---|---|
| `episodic_max_share` | `0.35` | Most of the budget episodic events may take. |
| `working_max_share` | `0.05` | Most of the budget working-session turns may take. |
| `semantic_min_share` | `0.30` | Reserved for semantic rows before the other layers are packed. |
| `semantic_min_items` | `5` | The reserve is this many rows instead, when they are fewer tokens than `semantic_min_share`. |

Pinned items have no share: they are always kept. Semantic rows have no
ceiling: they take whatever events and turns leave. The shares need not
add up to 1.

To give events more room, raise `episodic_max_share`; at `1.0` they are
limited only by the semantic reserve and the pins, which is close to how
the budget was split before 1.6.

### Freshness of memories

| Key | Default | Meaning |
|---|---|---|
| `semantic_half_life_days` | `90` | Half-life of a memory's freshness, measured from its last content edit (`updated_at`). `0` turns decay off. |
| `semantic_recency_floor` | `0.75` | Lowest value the freshness factor can take. |

A memory's auto-context score is `rank × 0.7 × max(floor, 2^(−age/half_life))`.
With the defaults a memory never loses more than a quarter of its score to
age. These keys affect `mneme://context` only; `recall` does not decay.

### Event selection

| Key | Default | Meaning |
|---|---|---|
| `episodic_query_mix` | `0.3` | β. With a `?q=` seed an event scores `recency × (β + (1−β)·lex)`, where `lex` is how much of the query it contains. `1.0` ignores the query; `0.0` removes events that share nothing with it from contention. |
| `event_clip_chars` | `400` | Payloads longer than this are clipped and the row is marked `truncated`. `0` turns clipping off. |
| `min_event_weight` | `0.3` | Events with `retrieval_weight` at or below this are left out. |
| `max_events_per_kind` | `6` | At most this many events of one kind. `0` means no cap. |

Events of kind `archived` are always left out unless the read names them
with `?kinds=archived`.

### `pin_scopes`

| | |
|---|---|
| **Type** | array of strings |
| **Default** | `["global"]` |
| **Affects** | Which pins a scoped `mneme://context` read returns |

A read of `mneme://context?scope=a,b` returns the pins stored under `a`
and `b`, plus the pins stored under every scope listed here. A read with
no `scope` returns every pin and ignores this key.

Before 1.6 a scoped read returned only the pins of that scope, so naming
a scope dropped the global rules. Set `pin_scopes = []` to restore that.

### `include_demoted`

| | |
|---|---|
| **Type** | boolean |
| **Default** | `false` |
| **Affects** | Whether demoted memories appear in `mneme://context` |

A memory that a current, newer memory `refines` or `contradicts` is
demoted (see [MCP surface §Relations](./mcp-surface.md#demotion)).
`recall` returns it lower down; auto-context leaves it out. Set this to
`true` to keep such memories in the context block, ranked below the
memory that refines them.

---

## `[recall]` — ranking knobs

### `demote_margin`

| | |
|---|---|
| **Type** | number |
| **Default** | `0.10` |
| **Affects** | How far a refined or contradicted memory drops in `recall` |

Subtracted from the `rank` of a demoted memory, on the cosine-similarity
scale; its keyword evidence is dropped as well. The default is about the
gap between a right answer and a lookalike, so the older memory lands
just below the one that refines it rather than at the bottom of the
list. `0` keeps the rank and only drops the keyword evidence. Negative
values are treated as `0`.

### `stemming`

Reserved. Has no effect yet.

---

## `[guardrails]` — write-time checks

### `secrets`

| | |
|---|---|
| **Type** | string |
| **Default** | `"reject"` |
| **Valid** | `"reject"`, `"warn"`, `"off"` |
| **Affects** | `remember`, `update` (when `content` is given), `pin`, `record_event` |

What the write tools do when the content holds a credential in a
high-confidence format: an AWS access key id, a PEM private key header, a
JWT, a GitHub token (`ghp_…`, `github_pat_…`), a Slack token, an OpenAI or
Anthropic key (`sk-…`), a Google API key (`AIza…`), a Stripe key or an age
secret key. Each pattern needs a fixed prefix and a long random body, so
ordinary prose and hyphenated words don't match.

- `"reject"`: nothing is written. The reply has `isError: true` and
  `_meta.error = {"code": "secret_detected", "pattern": "<name>",
  "offset": <character offset>}`. The matched text is never echoed or
  logged. For `record_event`, `path` locates the string inside the
  payload.
- `"warn"`: the write is stored and the reply carries
  `_meta.secret_advisory` (`pattern`, `offset`, `high_confidence`,
  `message`) plus a one-line warning in the reply text (for `record_event`
  it names the payload `path` too). The automatic
  `tool_call` event for that write omits the content.
- `"off"`: no scan.

A looser `password`/`secret`/`token`/`api_key` followed by `:` or `=` and
eight or more characters is **warn-only** in `"reject"` and `"warn"`
modes: it never blocks a write, it only adds the advisory. The scan covers
the text of `content` and the string values of a `record_event` payload,
not tags or scope. See
[MCP surface](./mcp-surface.md#write-guardrails).

---

## `[mcp]` — protocol surface

### `transport`

| | |
|---|---|
| **Type** | string |
| **Default** | `"stdio"` |
| **Valid** | `"stdio"` |
| **Affects** | nothing (informational) |

The transport is chosen by the command, not by this field: `mneme run`
speaks line-delimited JSON over stdin/stdout; `mneme daemon` speaks the
same over a Unix domain socket at `~/.mneme/run/mneme.sock` (plus
Streamable HTTP when [`[http] enabled`](#http--streamable-http-listener));
`mneme serve` speaks Streamable HTTP only.

### `sse_port`

| | |
|---|---|
| **Type** | unsigned 16-bit integer |
| **Default** | `7878` |
| **Affects** | nothing |

Superseded by [`[http] bind`](#http--streamable-http-listener), which
configures the Streamable HTTP transport. Setting it is a no-op, and
`mneme init` no longer writes it. See
[Reserved settings](#reserved-settings).

---

## `[daemon]` — v1.1 daemon-mode tuning

The `[daemon]` section is committed early so the config surface is
stable from the first M2 commit. The daemon itself ships
incrementally across A.M2–A.M5 (release-planning v2.1 §3.9); these
knobs are honored as each piece lands. ADR-0012 documents the full
design.

### `idle_timeout_minutes`

| | |
|---|---|
| **Type** | unsigned integer (minutes) |
| **Default** | `30` |
| **Affects** | daemon shutdown after no clients connected |

Idle-timeout shutdown threshold (ADR-0012 D6). The daemon stops
after no clients have been connected for this long. `0` disables
the timeout — the daemon then only stops via `mneme stop` or
SIGTERM. Counted from "last client disconnected", not "last request
seen", so a long HNSW snapshot that blocks requests but not
connections doesn't accidentally trigger shutdown.

The auth-token file is always `~/.mneme/run/auth.token` (mode `0600`);
agent configs reference that path, never the token value. NEVER embed the
token value in `settings.json`, `claude_desktop_config.json`,
`.cursorrules`, or any other agent config file.

### `log_level`

| | |
|---|---|
| **Type** | string (`"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`, or `"default"`) |
| **Default** | `"default"` (inherits `[logging] level`) |
| **Affects** | daemon-only verbosity |

Daemon-only log level override. Falls back to the global
`[logging] level` when set to `"default"`. Lets you turn the daemon
up to `debug` while leaving CLI tools (`mneme stats`,
`mneme inspect`, etc.) at the global level.

---

## `[http]` — Streamable HTTP listener

MCP over HTTP, for agents that connect over the network instead of
spawning `mneme client` — typically an agent in another container.
`mneme serve` always runs this listener; `mneme daemon` runs it next
to the Unix socket only when `enabled = true`. Every key except
`enabled`, `max_sessions` and `session_idle_minutes` has an
environment override, so a container can run with no config file.
Endpoints and the security model are described in
[Setting up with Hermes Agent](./hermes-setup.md#the-http-transport).

### `enabled`

| | |
|---|---|
| **Type** | bool |
| **Default** | `false` |
| **Affects** | `mneme daemon` only |

Also open the HTTP listener when running `mneme daemon`, so one process
serves bare-metal agents over the socket and remote ones over HTTP.
While it's on, `[daemon] idle_timeout_minutes` is ignored: HTTP clients
aren't counted as connected, so the daemon would otherwise stop under
an active HTTP agent. `mneme serve` ignores this key.

### `bind`

| | |
|---|---|
| **Type** | string, `ip:port` |
| **Default** | `"127.0.0.1:7878"` |
| **Override** | `MNEME_HTTP_BIND`, or `mneme serve --bind` |

Address to listen on. Must be an IP address, not a hostname. Loopback
by default; inside a container use `0.0.0.0:7878` (the image sets
this). mneme serves plain HTTP: only listen beyond loopback on a
private network, or behind a TLS-terminating proxy.

### `token_file`

| | |
|---|---|
| **Type** | string (path) |
| **Default** | `""` (= `~/.mneme/run/auth.token`) |
| **Override** | `MNEME_HTTP_TOKEN` (the value), `MNEME_HTTP_TOKEN_FILE` (a path) |

Where the bearer token comes from. Precedence: `MNEME_HTTP_TOKEN`, then
`MNEME_HTTP_TOKEN_FILE`, then this key, then the daemon's own
`~/.mneme/run/auth.token` (created if missing). Files are re-read on
every request, so rotating the token (`mneme auth rotate` for the
default file) needs no restart. Surrounding whitespace is ignored.

### `allowed_origins`

| | |
|---|---|
| **Type** | list of strings |
| **Default** | `[]` |
| **Override** | `MNEME_HTTP_ALLOWED_ORIGINS` (comma-separated) |

Browser `Origin` values to accept, matched exactly; `"*"` accepts any.
A request that carries an `Origin` header not on this list gets `403`
(DNS-rebinding protection). Agents don't send `Origin`, so leave it
empty unless a browser-based client needs access.

### `max_sessions`

| | |
|---|---|
| **Type** | unsigned integer |
| **Default** | `256` |

Cap on concurrently open MCP sessions. An `initialize` beyond it gets
`503`.

### `session_idle_minutes`

| | |
|---|---|
| **Type** | unsigned integer (minutes) |
| **Default** | `1440` (24 h) |

Sessions with no traffic for this long are dropped. A client that comes
back gets `404` and re-initializes, as the MCP spec prescribes.

---

## `[telemetry]` — reserved, no effect

Accepted on load for backwards compatibility; `mneme init` no longer
writes the section. There is no telemetry subsystem and **no code path
in the binary makes a network call** — not opt-in-and-disabled, simply
absent. See [Reserved settings](#reserved-settings).

### `enabled`

| | |
|---|---|
| **Type** | bool |
| **Default** | `false` |
| **Affects** | nothing |

### `endpoint`

| | |
|---|---|
| **Type** | URL string |
| **Default** | `""` |
| **Affects** | nothing |

---

## `[logging]` — tracing output

### `level`

| | |
|---|---|
| **Type** | string |
| **Default** | `"info"` |
| **Valid** | `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"` |
| **Affects** | verbosity of the file + stderr log streams |

Standard `tracing-subscriber` levels. The `RUST_LOG` env var still
overrides this if set (per-module filters work via
`RUST_LOG=mneme::storage=debug,mneme=info`).

### `file`

| | |
|---|---|
| **Type** | path |
| **Default** | `~/.mneme/logs/mneme.log` |
| **Affects** | where structured JSON tracing logs land |

The file destination receives JSON-formatted spans + events for
operational use; stderr receives a human-readable mirror at the
same level. Set to `""` to disable file logging entirely
(stderr-only mode).

### `max_size_mb`

| | |
|---|---|
| **Type** | unsigned integer (MB) |
| **Default** | `100` |
| **Affects** | per-file log size before rotation |

Soft cap; rotation lands when the active log crosses this size.

### `max_files`

| | |
|---|---|
| **Type** | unsigned integer |
| **Default** | `5` |
| **Affects** | how many rotated log files are retained |

Older logs beyond this count are deleted on each rotation.

---

## Environment overrides

These environment variables override config fields at boot. They're
intended for cross-cutting use (per-project isolation, offline CI),
not for everyday tuning — use the config file for that.

### `MNEME_DATA_DIR`

Overrides `[storage] data_dir`. Per-project isolation pattern: set
in `.envrc` so each project gets its own palace. Each project
needs its own `mneme init` after the variable is set.

```sh
# project-local .envrc:
export MNEME_DATA_DIR="$HOME/.mneme-projectname"
```

### `MNEME_EMBEDDER`

Set to `stub` to swap in `crate::embed::stub::StubEmbedder` —
deterministic 32-dim output, no model download, no network. Used
by tests and offline CI runs. The boot log emits a loud `WARN`
when active so this doesn't get accidentally left on in
production.

```sh
MNEME_EMBEDDER=stub mneme run     # offline, ~10 ms cold start
```

Stored vectors under `MNEME_EMBEDDER=stub` are **not portable** to
the real models — switching back will trigger a re-embed migration
on the next boot.

### `MNEME_HTTP_*`

`MNEME_HTTP_BIND`, `MNEME_HTTP_TOKEN`, `MNEME_HTTP_TOKEN_FILE`, and
`MNEME_HTTP_ALLOWED_ORIGINS` override the matching
[`[http]`](#http--streamable-http-listener) keys. Built for containers:

```sh
docker run -e MNEME_HTTP_TOKEN=... -v mneme-data:/data mneme   # binds 0.0.0.0:7878
```

---

## Switching embedders later

Change `[embeddings] model` in `~/.mneme/config.toml` and run
`mneme run`. On boot, `embed::migrate::migrate_if_needed`:

1. Reads the on-disk embedder identity sidecar at
   `<root>/semantic/embedder.json`.
2. Notices the active embedder differs from the on-disk one
   (different model name *or* different output dim).
3. Wipes the stale HNSW snapshot + WAL.
4. Iterates every `mem:`-prefixed row in storage, re-embeds it
   with the new model, writes a fresh snapshot at `applied_lsn = 0`.

For 10K memories under MiniLM, expect a few minutes of one-time
work on the next `mneme run`. The log line is loud:

```
INFO mneme::embed::migrate: re-embedded memories under new embedder identity count=10234 model="bge-m3" dim=1024
```

After the migration, `recall` uses the new model. There's no
`mneme reindex` subcommand because the boot-time check is the
canonical migration path — adding a CLI surface would just let
users skip the safety check.

## Reading the live config

The `mneme stats` tool / `mneme://stats` resource exposes derived
runtime state, not the raw config. To check what mneme is
actually using:

```sh
cat ~/.mneme/config.toml          # the file as written
mneme stats                        # runtime view: applied_lsn,
                                   # turns_total, current_scope, etc.
```

If a field doesn't appear in `~/.mneme/config.toml`, the value
documented on this page is what's in effect.
