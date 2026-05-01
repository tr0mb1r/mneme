# Configuration

`mneme init` writes a `~/.mneme/config.toml` with every default made
explicit. Every section is optional — if you delete a section or a
field, mneme falls back to the value documented here. A missing
config file is also fine: mneme uses defaults for every field.

This page is the canonical reference for every parameter — what it
does, what values it accepts, when to change it, and what the trade-
offs are. Changes take effect on the next `mneme run`.

## Quick reference

```toml
[storage]
data_dir   = "~/.mneme"
max_size_gb = 10
encryption = false

[embeddings]
model      = "bge-m3"     # or "minilm-l6"
device     = "auto"       # "auto" / "cpu" / "cuda" / "metal"
batch_size = 32

[scopes]
default = "personal"

[checkpoints]
session_interval_secs   = 30
session_interval_turns  = 5
hnsw_snapshot_inserts   = 1000
hnsw_snapshot_minutes   = 60

[consolidation]
hot_to_warm_days  = 28
warm_to_cold_days = 180
schedule          = "idle"

[budgets]
default_recall_limit       = 10
auto_context_token_budget  = 4000

[mcp]
transport = "stdio"
sse_port  = 7878

[telemetry]
enabled  = false
endpoint = ""

[logging]
level       = "info"
file        = "~/.mneme/logs/mneme.log"
max_size_mb = 100
max_files   = 5
```

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
| **Affects** | (unused) |

Reserved for at-rest encryption. Setting it to `true` today is a
no-op; mneme will either implement it via redb's
upcoming encryption extension or via a wrapping layer. Don't depend
on this field yet.

---

## `[embeddings]` — model + device for the L4 vector index

### `model`

| | |
|---|---|
| **Type** | string |
| **Default** | `"bge-m3"` |
| **Affects** | every L4 `remember` / `recall` / `update`; HNSW dimensionality |

Canonical embedder identity. Two models are supported in v1.0:

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
  resolves to in v1.0; CUDA / Metal acceleration is a future
  optimization.
- `"cpu"` — explicitly CPU. Equivalent to `"auto"` today.
- `"cuda"` — request the CUDA backend. Currently treated like CPU
  with a warning; CUDA support depends on a future candle build flag
  flip.
- `"metal"` — Apple Silicon GPU. Same status as CUDA today.

For v1.0, leave at `"auto"`. The model loaders log the active
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
| **Default** | `"personal"` |
| **Affects** | scope arg fallback for `remember` / `pin` |

The session's starting "current scope" — used by write tools when
the caller omits the `scope` argument. The `switch_scope` tool
mutates the in-memory cell; this field is only consulted at boot.

The string is free-form. Common conventions:
- `"personal"` *(default)* — default home for individual use
- `"work"`, `"home"`, `"<projectname>"` — separate buckets
- `"client-x"`, `"client-y"` — confidentiality boundaries

`list_scopes` shows every distinct scope across the three layers.
Cross-process semantics: see [switch_scope](./mcp-surface.md#session-helpers).

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
successful `tools/call`. Whichever trigger fires first wins.

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
hot path — they're for forensics, not auto-context.

### `schedule`

| | |
|---|---|
| **Type** | string |
| **Default** | `"idle"` |
| **Valid (v1.0)** | `"idle"` |
| **Affects** | when the consolidation scheduler fires |

Drives `ConsolidationScheduler`'s decision policy. v1.0 supports
`"idle"` only:

- `"idle"` — wakes every 5 minutes; fires the consolidation pass
  iff no `remember` / `forget` / `update` / `record` happened in
  the prior tick. A burst of writes pushes the next pass back to
  the next quiet window.

Future modes (`"every_<n>m"`, cron expressions, `"on_demand"`)
land in v1.1. Anything other than `"idle"` today logs a warning
and falls back to `"idle"` cadence.

---

## `[budgets]` — recall + auto-context limits

### `default_recall_limit`

| | |
|---|---|
| **Type** | unsigned integer |
| **Default** | `10` |
| **Affects** | `recall` results when the agent omits `limit` |

Default `k` for the `recall` tool when the agent doesn't provide
one. `recall` callers can override per-call up to a hard cap of
500.

### `auto_context_token_budget`

| | |
|---|---|
| **Type** | unsigned integer (chars/4 token estimate) |
| **Default** | `4000` |
| **Affects** | `mneme://context` total payload size |

Total token budget for the auto-context resource. The orchestrator
greedy-packs the four memory layers (procedural / working / episodic
/ semantic) up to this ceiling, with per-layer minimums protecting
each layer from being starved.

The default 4000 is calibrated for a ~32K-token model context window
where you want to spend ~12% on agent memory. Bump for larger
windows (e.g. 16000 for a 128K-token Claude session); shrink if
you're running a smaller model.

The estimator is `chars / 4` per spec §0; replacing it with a real
tokenizer (`tokenizers`/`tiktoken-rs`) is a one-function swap
without a config change.

---

## `[mcp]` — protocol surface

### `transport`

| | |
|---|---|
| **Type** | string |
| **Default** | `"stdio"` |
| **Valid (v1.0)** | `"stdio"` |
| **Affects** | how the MCP server speaks |

v1.0 ships stdio only — JSON-RPC framed line-delimited JSON over
stdin/stdout, the standard MCP local-tool transport. SSE / HTTP
transports (with auth + TLS) are deferred to v1.1.

### `sse_port`

| | |
|---|---|
| **Type** | unsigned 16-bit integer |
| **Default** | `7878` |
| **Affects** | (unused in v1.0) |

Reserved for the future SSE transport. Setting it today is a no-op.

---

## `[telemetry]` — opt-in usage reporting

### `enabled`

| | |
|---|---|
| **Type** | bool |
| **Default** | `false` |
| **Affects** | nothing in v1.0 |

Reserved. Mneme is **off by default and never enabled
silently** — anonymous usage telemetry is opt-in only and ships in
a future release. Setting it today does nothing; the binary
contains no telemetry transport.

### `endpoint`

| | |
|---|---|
| **Type** | URL string |
| **Default** | `""` |

Same status as `enabled` — placeholder.

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

Two environment variables override config fields at boot. They're
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
