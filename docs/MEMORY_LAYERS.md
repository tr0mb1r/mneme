# Memory layers and their lifecycles

This document explains exactly **what mneme stores, where it lives,
when it's embedded, and which schedules move data between tiers**.
Read this if you're trying to understand why a memory you stored
yesterday is or isn't surfacing today, or whether your machine is
about to do disk I/O while you're typing.

If you only want the one-page version, read [§1 Quick reference](#1-quick-reference).
The rest of the doc is layer-by-layer detail.

> **Honesty note on schedules.** As of v0.14 all three background
> schedulers are wired (HNSW snapshots, L3 consolidation on a
> 5-min idle tick, L1 session checkpoints on 30 s / 5 turns) AND
> the L1 read-side fold-in into `mneme://context` ships. Working
> turns now appear in auto-context with `WORKING_WEIGHT = 0.9`,
> sorted newest-first. The per-session resource
> `mneme://session/{id}` exposes the full session JSON for active
> or past sessions. v0.15 adds `switch_scope` so write tools
> (`remember`, `pin`) without an explicit `scope` arg land in the
> session's current scope; current value surfaces on
> `mneme://stats` as `working.current_scope`.

---

## 1. Quick reference

| Layer | What it holds | Embedded? | Where on disk | Auto-schedule (today) | Auto-schedule (v1 design) |
|---|---|---|---|---|---|
| **L0 Procedural** | Always-on pinned rules (`pin` / `unpin`) | No | `~/.mneme/procedural/pinned.jsonl` | Live: file watched every 500 ms; external edits picked up in <1 s | Same |
| **L1 Working session** | The current session's turns (tool, user, assistant), scratch state | No | `~/.mneme/sessions/<session_id>.snapshot` (atomic temp+rename per flush) | **Checkpoint scheduler wired (v0.13)**; **conversation mirror wired (v0.2.4)** — `record_event(kind="user_message"/"assistant_message")` pushes a matching turn to L1 | Same |
| **L3 Episodic** | Time-ordered events: tool calls, lifecycle events, conversation turns, curated semantic events | No (lexical only) | `~/.mneme/episodic/` (redb), three prefixes: `epi:` hot, `wepi:` warm, `cold/` zstd | **Auto-emit shipping (v0.2.3 + v0.2.4)** — `tool_call` (per-tool enriched payload), `tool_call_failed`, `session_start`, `session_end`. **Agent-driven (v0.2.4)** — `record_event` writes any kind. **Tier promotion wired (v0.12)** — `ConsolidationScheduler` fires every 5 min when idle | Idle-time pass: hot → warm at age ≥ 28 d, warm → cold at age ≥ 180 d |
| **L4 Semantic** | Long-term facts, decisions, preferences, conversations | **Yes** — every `remember` / `update` re-embeds | `~/.mneme/episodic/` (redb), prefix `mem:` + `~/.mneme/index/hnsw.idx` snapshot + `~/.mneme/wal/` deltas | Live writes; **HNSW snapshot scheduler runs**: every 1000 inserts OR every 60 min, whichever first | Same |
| **Auto-context resource** | Pinned + recent, packed to a token budget | Reads only | (assembled on demand) | On read of `mneme://context` | Same |
| **Cold archive** | Quarter-bundled JSON, zstd-compressed | No | `~/.mneme/cold/<YYYY-Q>.zst` | Written only when L3 consolidation runs | Same as L3 |

The defaults in the table come from `~/.mneme/config.toml`; every
threshold is user-overridable. See [§7 Configuration knobs](#7-configuration-knobs).

---

## 2. L0 Procedural — always-on rules

**What it is.** A small JSONL file of pinned items. Each call to
`pin` appends a line; `unpin` rewrites the file without it. Procedural
memory exists to surface high-priority rules ("always run `cargo fmt`
before commit", "we never push to main") on every recall context
without semantic search needing to find them first.

**Embedding.** None. Procedural is exact-match by id, with content
returned verbatim. No vectors are computed.

**Storage.** `~/.mneme/procedural/pinned.jsonl`. One JSON object per
line. Human-readable; you can edit it in a text editor with mneme
running.

**Hot-reload schedule.** A `notify::PollWatcher` polls the file every
**500 ms** and reloads the in-memory cache when the content hash
changes. This is faster and more uniform across platforms than
FSEvents (macOS) or `ReadDirectoryChangesW` (Windows), which
coalesce events for tens of seconds. External edits are reflected in
the running server in **under 1 second**.

**Surfaced via:**
- Tool: `pin` (write), `unpin` (write).
- Resource: `mneme://procedural` (read).
- Auto-context: pinned items get a guaranteed share of the token
  budget on every `mneme://context` read.

**What runs today:** All of the above. No deferred features.

---

## 3. L1 Working session — the current turn(s)

**What it is.** The active session's working state — turn history,
scratch notes, the agent's own self-reminder buffer. Designed to
survive crashes by checkpointing to disk on a schedule.

**Embedding.** None.

**Storage.** `~/.mneme/sessions/<session_id>.json` (atomic
temp+rename per checkpoint). Single file per session.

**Designed schedule.** Per spec §8.3 and `[checkpoints]` in
`config.toml`:
- `session_interval_secs = 30` — checkpoint at most every 30 s.
- `session_interval_turns = 5` — and at least every 5 turns.
- Whichever fires first.

**What runs today:**
> **Wired (v0.13).** `cli::run` spawns a `CheckpointScheduler` that
> writes the active session to `~/.mneme/sessions/<session_id>.snapshot`
> on the first of two triggers: every `session_interval_secs` (30 s
> default) when there are pending turns, OR every
> `session_interval_turns` (5 default) tools/call invocations,
> whichever first. A clean shutdown writes a final snapshot with
> `clean_shutdown = true`. Per-session counters land on
> `mneme://stats` and the `stats` tool under the `working` key:
> `session_id`, `started_at`, `last_checkpoint_at`, `turns_total`,
> `checkpoints_total`.

**Practical implication.** Working-session state now survives
restart; the most recent snapshot for any prior session can be
loaded via `Session::load(...)` (today exposed only in tests; a
`mneme://session/{id}` resource is still outstanding). The L3
episodic store remains the structured "what did we just do?"
surface — it ranks by recency and embeddings; L1 is a turn log,
not a search index.

---

## 4. L3 Episodic — recent events and turn checkpoints

**What it is.** Time-ordered events: tool calls, user messages,
agent-emitted checkpoints, session summaries. Fundamentally a log,
not a search corpus — `recall_recent` returns events ordered by
`(last_accessed, retrieval_weight, created_at)`, not by semantic
similarity.

**Embedding.** None (ADR-0007). Episodic recall is lexical/temporal —
`recall_recent` returns L3 hits only, never L4. If you want semantic
search over what happened, the close-of-loop is: `summarize_session`
(returns a prompt template populated with recent L3 events) → fill it
via the host LLM → `record_event(kind="summary", payload={text, covers})`
to land the digest as a single L3 event → `remember` durable facts into
L4 for similarity retrieval. Mneme itself never calls an LLM (cardinal
rule #3); the agent owns the host completion path.

**Storage.** All in `~/.mneme/episodic/` (redb), distinguished by
key prefix:
- `epi:<ulid>` — hot tier (default home for new events)
- `wepi:<ulid>` — warm tier (after promotion)
- Cold tier lives outside redb in `~/.mneme/cold/<YYYY-Q>.zst`
  (zstd-compressed JSON bundles, one per calendar quarter).

**Designed lifecycle.** From `memory/consolidation.rs`:

```
hot  (epi:)   ──┐
                │  age ≥ hot_to_warm_days  (default 28)
warm (wepi:)  ──┐
                │  age ≥ warm_to_cold_days (default 180)
cold (zstd)
```

The pass is idempotent and crash-safe: hot→warm writes the new key
before deleting the old; warm→cold writes the cold bundle (atomic
temp+rename) before deleting warm rows. A `kill -9` mid-pass leaves
duplicates that the next run cleans up.

**Designed schedule.** `[consolidation] schedule = "idle"` in the
default config — meant to run when the server is idle. v1.0 honours
the `"idle"` mode only: the scheduler wakes every 5 min; if no
`remember` / `forget` / `record` happened in the prior tick, it
fires a pass and refreshes its idle gate. Future modes
(`every_<n>m`, cron, `on_demand`) belong to v1.1.

**What runs today:**
> **Auto-emit shipped through v0.2.4** (ADR-0009). Server-side
> `Server::handle_tools_call` and `cli::run::async_main` emit:
>
> - `tool_call` after every successful dispatch — payload is enriched
>   per-tool with the value-bearing arg (e.g. `remember.content`,
>   `recall.query`); diagnostic tools keep just `{tool: <name>}`.
>   The full `arguments` JSON is **never** mirrored (privacy).
> - `tool_call_failed` when a dispatch errors (carries `error_kind`
>   and a truncated `message` string). Mirrors the success-path emit
>   but does NOT push a turn to L1.
> - `session_start` on `mneme run` boot.
> - `session_end` on graceful shutdown (`clean_shutdown=true`).
>
> **Agent-driven via `record_event` (v0.2.4, ADR-0008).** The agent
> calls `mneme.record_event(kind, payload, ...)` for any kind beyond
> the auto-emits. Canonical kinds: `user_message`, `assistant_message`
> (server also pushes a matching turn to L1), `decision`, `problem`,
> `resolution`, `milestone`, `preference`, `pivot`, `observation`,
> `summary`. Free-form: agents can invent new kinds without a schema
> migration.
>
> **Tier promotion is wired and runs in the background**
> (`memory::consolidation_scheduler::ConsolidationScheduler`,
> spawned by `cli::run`). Per-pass observability (`runs_total`,
> `errors_total`, `last_consolidation_at`, `last_promoted_to_warm`,
> `last_archived_to_cold`) lands on the `mneme://stats` resource and
> the `stats` tool under the `consolidation` key. A burst of
> writes still inside its idle window suppresses the next pass —
> consolidation only fires after the system goes quiet.

**Surfaced via:**
- Tools: `recall_recent`, `summarize_session`, `record_event` (v0.2.4+).
- Resource: `mneme://recent`.

---

## 5. L4 Semantic — long-term facts (the embedded layer)

**What it is.** The semantic layer. Every memory is embedded into a
vector and indexed with HNSW for sub-50 ms semantic search. This is
where `remember`, `recall`, `update`, and `forget` live.

**Embedding.** **Yes.** Every `remember` and every `update` that
changes `content` calls the embedder. `update`s that change only
`tags` / `scope` / `kind` skip the embedder.

The active embedder is set in `[embeddings] model` in `config.toml`:
- **`bge-m3`** (default) — ~1.5 GB, 1024-dim, multilingual, top-tier
  recall. Best quality.
- **`minilm-l6`** — ~80 MB, 384-dim, English. Faster startup, smaller
  footprint, lower recall on edge cases.
- **`stub`** (via `MNEME_EMBEDDER=stub` env var, not config) —
  deterministic 768-dim dummy. Tests + offline boots only; **stored
  vectors aren't portable to real models**.

**Model swap behavior.** Changing `[embeddings] model` in `config.toml`
between runs triggers an automatic re-embed of every stored memory on
the next `mneme run`. This is logged loudly. The migration is atomic
through the WAL — interrupted re-embeds resume on the next boot.

**Embedding cadence.** Synchronous on the write path. `remember`
returns only after the vector is written. Production p95 budget per
spec §13: **150 ms per `remember`** (cold cache), **50 ms per `recall`**
on a 100K-memory dataset.

**Storage.**
- Memories: `~/.mneme/episodic/` (redb), prefix `mem:`. (Yes, the
  redb file is shared with episodic; only the key prefix
  distinguishes layers.)
- HNSW index: `~/.mneme/index/hnsw.idx` (atomic snapshot, postcard
  with magic prefix, schema v2 carries the embedded `applied_lsn`).
- WAL: `~/.mneme/wal/<segment>.wal`. Append-only. Group-committed
  with `fdatasync`. Replayed on boot from `applied_lsn` forward.

**Snapshot schedule (this is the one running schedule today).**
The snapshot scheduler in `memory/semantic.rs::scheduler_loop` wakes
on **either**:
- **Insert count threshold** — default `[checkpoints]
  hnsw_snapshot_inserts = 1000` (set in `config.toml`).
- **Time interval** — default `[checkpoints] hnsw_snapshot_minutes
  = 60`.

When either fires, the scheduler:
1. Takes the write lock (so no `remember`/`forget` is mid-flight).
2. Rewrites `hnsw.idx` from the in-memory index, atomically.
3. Truncates WAL segments fully covered by the new snapshot.

**Time-based wakes without insert pressure are deliberate no-ops** —
the scheduler exists to bound the worst-case gap between snapshots,
not to write empty ones.

A final snapshot is forced on graceful shutdown (`mneme stop`,
SIGTERM, Ctrl-C) so the next boot skips WAL replay.

**Crash recovery.** On `mneme run`:
1. Load `hnsw.idx` if present (gives `applied_lsn`).
2. Replay WAL forward from `applied_lsn`.
3. Open MCP server.

A `kill -9` mid-snapshot leaves the old snapshot intact (atomic
temp+rename); WAL replay covers the gap. Verified by
`tests/crash_recovery_semantic.rs`.

**Surfaced via:**
- Tools: `remember`, `recall`, `update`, `forget`.
- No L4-specific resource; semantic results are available through
  `mneme://context` (auto-context).

**What runs today:** All of the above.

---

## 6. Auto-context resource (`mneme://context`)

**What it is.** A pre-assembled context blob the agent can read at
session start. Combines:
- All pinned items from L0 (every one — they're a small set).
- The most recent N events from L3 (recency-ordered).
- Optionally seeded by an L4 query if the agent passes one.

Packed to a token budget (`[budgets] auto_context_token_budget =
4000` by default), with a per-layer floor so no single layer can
crowd the others out.

**Embedding.** Reads-only — already-embedded L4 results are mixed in
when an L4 seed is provided.

**Schedule.** None. The resource is **assembled on demand** when the
agent reads `mneme://context`. Latency budget per spec §13:
**p95 < 200 ms, p99 < 400 ms**.

**Determinism.** Given the same DB state, the assembly is
deterministic. Verified by `orchestrator::tests::build_context_is_deterministic`.

**Surfaced via:** Resource `mneme://context` only.

**What runs today:** All of the above.

---

## 7. Configuration knobs

Every threshold above is overridable in `~/.mneme/config.toml`. The
defaults shown here come from `Config::default` in `src/config.rs`.

```toml
# Tier transitions (L3 episodic).
[consolidation]
hot_to_warm_days  = 28
warm_to_cold_days = 180
schedule          = "idle"   # only "idle" today; reserved for future modes

# Snapshot + checkpoint cadences (L4 + L1).
[checkpoints]
session_interval_secs   = 30
session_interval_turns  = 5
hnsw_snapshot_inserts   = 1000
hnsw_snapshot_minutes   = 60

# Active embedder (L4).
[embeddings]
model      = "bge-m3"        # or "minilm-l6"
device     = "auto"          # "cpu" / "metal" / "cuda" override
batch_size = 32

# Auto-context budget.
[budgets]
default_recall_limit       = 10
auto_context_token_budget  = 4000
```

Pick a smaller model (`minilm-l6`) and a tighter snapshot cadence
(`hnsw_snapshot_inserts = 200`) on resource-constrained machines.
Pick larger thresholds (e.g. `hnsw_snapshot_minutes = 240`) on heavy
write workloads where snapshots are starting to dominate I/O.

---

## 8. What gets embedded — at a glance

| Operation | Embedder called? | Why |
|---|---|---|
| `remember` | **Yes** | Every L4 memory needs a vector for `recall`. |
| `update` (changes `content`) | **Yes** | Re-embed via `WalOp::VectorReplace`. |
| `update` (only `tags`/`scope`/`kind`) | No | Metadata-only path skips the embedder. |
| `forget` | No | Tombstone the vector; no new embedding. |
| `pin` / `unpin` | No | L0 is exact-match. |
| `recall_recent` / `summarize_session` / `record_event` | No | L3 is lexical/temporal (ADR-0007). |
| `recall` | **Yes (query side)** | Embeds the query string before the HNSW search. |
| Reading `mneme://context` | Sometimes | Only if you pass a `query` parameter. |
| Reading `mneme://stats` / `mneme://procedural` / `mneme://recent` | No | Direct lookups. |
| Booting `mneme run` after model swap | **Yes — for every stored memory** | `embed::migrate::migrate_if_needed`. Logged loudly. |

The embedder is a synchronous, batched worker (default
`batch_size = 32`). On `bge-m3` cold-cache, expect ~70 ms per
embedding on Apple Silicon Metal; on `minilm-l6`, ~5 ms. Recall
adds the query-embed step + the HNSW search.

---

## 9. Where memory lives on disk

```
~/.mneme/
├── config.toml                     # all thresholds; user-editable
├── schema_version                  # plain integer; bumped by migrations
├── .lock                           # PID file, exclusive lock
│
├── procedural/
│   └── pinned.jsonl                # L0; watched every 500 ms
│
├── sessions/                       # L1; written when wired (see §3)
│   └── <session_id>.json
│
├── episodic/                       # redb file shared by L3 + L4
│                                   #   epi:<ulid>  → hot L3
│                                   #   wepi:<ulid> → warm L3
│                                   #   mem:<ulid>  → L4 memory rows
│
├── wal/
│   └── <segment>.wal               # L4 write-ahead log
│
├── index/
│   └── hnsw.idx                    # L4 snapshot (atomic temp+rename)
│
├── cold/
│   └── 2026-Q1.zst                 # L3 cold tier, one bundle per quarter
│
├── models/
│   └── <model-id>/                 # downloaded weights + tokenizer
│
└── logs/
    └── mneme.log                   # human-readable, rotated at 100 MB × 5
```

`mneme stats` reports per-layer counts plus the `applied_lsn` of the
most recent snapshot. `mneme inspect <ulid>` reads any single row.
`mneme export` dumps everything to JSON (`semantic` / `procedural` /
`episodic` keyed) for grepping or piping to `jq`.

---

## 10. FAQ

**Q: I `remember`'d something five minutes ago. Will I lose it if my
laptop crashes right now?**
No. `remember` is synchronous; it returns only after the WAL has
fsynced the vector + content. Worst case after a crash is up to
60 minutes of WAL to replay (or 1000 inserts since the last
snapshot, whichever came first). The data is durable, restart is
just slower without the snapshot.

**Q: Why isn't my old episodic event being archived to cold tier
even though it's two years old?**
Two reasons it can stall: (1) the system never went idle long
enough for the scheduler's idle gate to close — every `remember` /
`forget` / `record` resets the window, so a constantly-active mneme
delays consolidation; (2) the event hasn't yet aged past
`hot_to_warm_days` / `warm_to_cold_days`. Inspect `mneme://stats`
under the `consolidation` key: `last_consolidation_at` is the
wall-clock of the last pass, and `runs_total` confirms the
scheduler is actually firing.

**Q: Does mneme embed my pinned items so they show up in semantic
recall?**
No. Pinned items are L0 procedural — they're returned verbatim from
`mneme://procedural` and packed into `mneme://context` on every read.
If you want a pinned item *also* searchable semantically, call both
`pin` and `remember` with the same content (or pin it and let the
agent decide to also `remember` notable rules).

**Q: I changed `model = "bge-m3"` to `model = "minilm-l6"` in
`config.toml`. What happens on the next `mneme run`?**
On boot, `embed::migrate::migrate_if_needed` notices the embedder
identity changed, re-embeds every L4 memory under the new model
through the WAL, and writes a new HNSW snapshot. If you have 10K
memories under MiniLM, expect a few minutes of one-time work; the
log line is loud (`re-embedded memories under new embedder
identity`). After that, recall uses the new model.

**Q: What happens to vectors during `mneme backup` and `mneme
restore`?**
`mneme backup` tar+gzips the data directory. The model cache
(`models/`) is excluded by default — re-downloadable, big, not
worth the bytes. The HNSW snapshot, WAL, redb file, and procedural
JSONL are all included. `mneme restore` is atomic (temp+rename)
and refuses to overwrite a directory where another mneme is
running. After restore, the next `mneme run` boots from the
restored snapshot + WAL exactly as if the original process had
just stopped.

---

## 11. See also

- [`docs/CLAUDE_CODE_SETUP.md`](CLAUDE_CODE_SETUP.md) — getting mneme
  wired into Claude Code, plus the patterns that make it pay off.
- The crate-level rustdoc (`cargo doc --open`) — `MnemeError`, the
  three trait seams (`Storage` / `Embedder` / `VectorIndex`), and
  per-module documentation cover the implementation in detail.
