# v1.6 release notes

> **Status: unreleased.** Previous: v1.5.0 (2026-10-12).

v1.6.0 is **the recall-quality release**. It is one release carrying
what was planned as three (the 1.5.2 fixes, searchable events and the
auto-context rework, then relations, provenance and `mneme eval`).

It came out of measuring mneme against a real store: 51
questions with known answers, replayed against six weeks of an agent's
memory. Three findings set the scope. Ten of the 51 answers were not in
any memory at all, only in recorded events that `recall` could not
search. The automatic context gave events 77% of its budget and showed
the same fifteen of them whatever the message was, leaving room for two
memories. And events older than 28 days had dropped out of every read
path. A fourth: a stale fact outranked its correction in 2 of 7 known
pairs, because the only way to link them hid the old one entirely and so
went unused. So this release makes **events searchable**, **reworks how
auto-context spends its budget**, adds **typed relations between
memories**, fixes the read paths, and ships the **evaluation harness**
that found all of this.

This is a **minor** bump per [Versioning](./versioning.md): new tool
arguments and config keys, and several defaults that changed (listed
first, below). The on-disk format gains side tables but `schema_version`
stays 1 and no migration runs. A 1.5 binary can still open a 1.6 store;
it ignores what it does not know. The full list is in the
[changelog](https://github.com/tr0mb1r/mneme/blob/main/CHANGELOG.md).

> **Linux and macOS only.** Windows support is dropped; see
> [Platforms](#platforms).

---

## Behaviour changes — check these before upgrading

- **Auto-context spends its budget differently.** Memories are
  guaranteed a share, events are capped at one, and events are ranked
  against the message. A `mneme://context` read returns different rows
  than it did in 1.5, and a read with no `?q=` uses less of the budget.
  See [Auto-context](#auto-context-reworked).
- **A scoped context read returns global pins too.** `?scope=project`
  used to drop every rule pinned under `global`. Set
  `[auto_context] pin_scopes = []` for the old behaviour.
- **Pinned items are never trimmed**, even when they alone exceed
  `auto_context_token_budget`.
- **Events of kind `archived`, and events with `retrieval_weight` ≤ 0.3,
  are left out of auto-context.** `?kinds=archived` brings the first
  group back.
- **Decisions, lessons, milestones and summaries are no longer moved to
  the cold archive.** They stay in the warm tier, where search reaches
  them (`[consolidation] cold_exempt_kinds`; `[]` restores archiving).
  The redb file grows with them.
- **Working memory (L1) is per connection** on the daemon and over HTTP.
  It was one shared session.
- **Read-only tool calls no longer add an L1 turn**, so `turns_total`
  grows more slowly (`[working] record_tool_turns`).
- **`export` includes warm-tier events.** Dumps get larger; that is the
  events 1.5 was leaving out.
- **`recall` hits on returned events refresh their `last_accessed`**
  (at most daily), which delays their move to the warm tier. Turn it off
  with `[episodic] touch_on_recall = false`.
- **`remember` with `pinned: true` is rejected** with a pointer to `pin`.
  It was silently ignored.

## Searching events

Events are not embedded, so `recall` used to search memories only. A
decision written with `record_event` and never copied into a memory
could be listed by time and not found by what it said.

- **`recall` with `layers: ["semantic", "episodic"]`** also runs a
  keyword search over events whose kind is in `[episodic] index_kinds`
  (`decision`, `milestone`, `lesson`, `summary`, `resolution`,
  `problem`, `pivot`, `observation`, `archived` by default; not
  `tool_call`). Rows from both layers carry `layer` and `rank` and come
  back in one list ordered by `rank`. An event containing every query
  word ranks like a memory at similarity 0.65. The default is still
  `["semantic"]`, with the same rows in the same order as 1.5.
- **`recall` takes `since` / `until`** (RFC3339 or ULID) and
  `time_field` (`created_at` or `updated_at`). `scope` takes several
  scopes separated by commas.
- **Warm and cold reads.** `recall_recent` reads the warm tier whenever
  the window reaches it, and `include_cold: true` adds the cold archive
  (only the quarters the window overlaps). `export` takes
  `include_cold` too, as does `mneme export --include-cold`. Rows carry
  `tier`.
- The index is in memory, built on first use from the hot and warm
  tiers. Nothing new is written to disk for it.

Details: [MCP surface → Searching events](./mcp-surface.md#searching-events).

## Auto-context, reworked

- **Budget by layer.** Pins are never trimmed. Memories are reserved
  `semantic_min_share` (30%) of the budget or `semantic_min_items` (5)
  rows, whichever is smaller, with no ceiling. Events take at most
  `episodic_max_share` (35%), working turns at most 5%.
- **Events are chosen for the message.** With `?q=`, candidates are the
  recent window plus the event index's matches for the query, so an
  on-topic event from two months ago can be offered. Exact and
  near-duplicates are dropped, at most six events of one kind are kept,
  and long payloads are clipped to 400 characters with
  `"truncated": true`.
- **Freshness of a memory is measured from its last edit**, with a
  90-day half-life and a floor, instead of 14 days from creation. A
  current-state memory that is edited in place no longer fades.
- **`?scope=a,b`** reads a set of scopes.
- The reply reports `layer_tokens`, and rows carry `rank`.

All of it is tunable under [`[auto_context]`](./configuration.md).

## Memories: `updated_at` and archiving

- **`updated_at`.** `update` records when a memory's content last
  changed. `recall`, `export` and auto-context carry it.
- **`update(id, status: "archived")`** takes a memory out of `recall`
  and auto-context without deleting it; `status: "active"` restores it.
  `recall(include_archived: true)` and `export` still show it.
- `recall` rows carry `rank` (similarity plus keyword evidence), on one
  scale in every mode.
- `export` emits `superseded_by`, so a dump can tell a current fact from
  a replaced one.

## Relations, provenance and `mneme eval`

<!-- TODO: verify against merged W3/W6/W7 -->
<!-- Drafted from the plan (§3 W3, W6, W7) before that work merged.
     Check every argument name, default and number below against the
     code, then delete these two comments. -->

### Relations between memories

`supersedes` was the only way to say that one memory replaces another,
and it hides the old one from recall and context. That is right for a
full replacement and wrong for a partial correction, where hiding the
old memory loses the facts that still hold.

- **`remember` and `update` take `relations`**: a list of
  `{type, id | ref, reason}`. Types are `supersedes`, `refines`,
  `contradicts` and `evidence_for`. `reason` is free text, up to 300
  characters. The target is another memory or event (`id`), or an
  external reference string (`ref`, e.g. `research:<slug>`,
  `path:/…`).
- **Demote, don't hide.** A memory that a newer, current memory
  `refines` or `contradicts` stays in `recall` but ranks lower: its
  keyword evidence is ignored and its rank drops by
  `[recall] demote_margin` (0.10). Auto-context leaves it out unless
  `[auto_context] include_demoted = true`. `supersedes` keeps its
  hide-the-old-one meaning, and `supersedes: "<id>"` still works as
  shorthand.
- **Rows carry `relations`** (`superseded_by`, `refined_by`,
  `contradicted_by`, `refines`, `evidence`), omitted when empty.
  `recall(expand: 1)` adds a stub of each linked memory.
  `recall(ref: "research:…")` finds memories by the external reference
  they cite.
- When `remember` reports a closely related existing memory, the reply
  now suggests the `relations` entry to add. The server still decides
  nothing by itself.
- If the memory a relation comes from is forgotten, the relation lapses
  and the target's rank returns.
- Relations are in `export` and survive backup and restore.

### Provenance

- `remember` and `update` take `provenance` (`user_stated`, `observed`,
  `inferred`, `imported`) and `source` (a free string such as
  `telegram:…` or `tool:…`). Rows carry both, `recall` can filter on
  `provenance`, and `stats` counts memories by it.
- It has no effect on ranking by default. It is there so that a hygiene
  pass can ask for, say, inferred facts older than 30 days that nobody
  confirmed.

### `mneme eval`

- **`mneme eval`** replays a question file against a store and reports
  retrieval quality: hit@1 / hit@5 and MRR for `recall` across modes,
  scopes and layers, how often a stale fact outranks its correction, and
  how much of the known-good material auto-context includes. Questions
  are JSON lines: `{id, question, type, scope, gold, gold_l3, stale}`.
  It writes a metrics file and a summary, and it checks that it wrote
  nothing to the store.
- A synthetic corpus ships with it (`examples/seed_storage.rs`), so the
  numbers can be reproduced without anyone's private data.
- The numbers that motivated this release, on the store it was measured
  against, with the targets it was built to: recall hit@5 over all 51
  questions 0.80 → ≥ 0.90; known-good memories present in auto-context
  2/17 → ≥ 7/17; stale-over-current 2/7 → 0/7; global pins kept on a
  scoped read 0–1/6 → 6/6. Measured results:
  _to be filled in from the release run._

## Fixes

- **`kind` is accepted** on `remember`, `update` and `recall`, with
  `type` kept as an alias. Replies always called the field `kind`, so
  agents passed it back and the call failed; in the store that prompted
  this, 9 of 116 `remember` calls.
- **`forget` no longer leaves the text behind.** `remember`, `pin` and
  `update` mirror their content into `tool_call` events, and `forget`
  deleted only the memory. It now redacts those events (hot and warm
  tiers) and reports how many. Cold bundles are not rewritten.
- **Warm-tier events are readable.** No read path looked at events older
  than `hot_to_warm_days` (28): a seven-week `recall_recent` window
  returned 7 rows with 344 present, and `export` was not a complete
  backup.
- **`forget` removes an event from both tiers** when a crash left it in
  two.
- **L1 isolation and turn counting**, as listed under behaviour changes.
  One client's turns used to appear in every other client's context, and
  a read-only run added 665 turns.
- `forget`'s schema declares only `id`. `query` and `scope` were
  advertised and always rejected.

## New configuration

| Section | Keys |
|---|---|
| `[auto_context]` | `semantic_min_share`, `semantic_min_items`, `episodic_max_share`, `working_max_share`, `pin_scopes`, `episodic_query_mix`, `max_events_per_kind`, `event_clip_chars`, `min_event_weight`, `semantic_half_life_days`, `semantic_recency_floor` |
| `[episodic]` | `index_kinds`, `touch_on_recall`, `log_tool_call_values` (`full` / `ids` / `off`: how much of a call's arguments a `tool_call` event keeps) |
| `[consolidation]` | `cold_exempt_kinds` |
| `[working]` | `record_tool_turns` (`writes` / `all` / `off`) |
| `[recall]` | `stemming` — accepted, **not implemented yet**; keyword search still matches whole tokens. `demote_margin` <!-- TODO: verify against merged W3/W6/W7 --> |

`mneme init` writes these into a new `config.toml` with comments. An
existing config needs no change: every key has a default. See
[Configuration](./configuration.md).

## Hermes Agent

- **The hook shows what the server selects.** Its own filtering (skip
  lists, weight floor, 200-character clip) is gone; the server does it
  before spending the budget. Use a 1.6 server with the 1.6 hook.
- **One scope per conversation.** `mneme init hermes --scope-map <file>`
  gives the hook a JSON map from thread ids to scopes. It then reads
  `mneme://context?q=…&scope=<thread scope>,global` instead of
  everything. See
  [Setting up with Hermes Agent](./hermes-setup.md#one-scope-per-conversation).
- **The `mneme` skill is 1.2.0.** It tells the agent to use
  `layers: ["semantic", "episodic"]` for "what did we conclude about X",
  `since` / `until` for time questions, and `status: "archived"` to
  retire a fact.

## Platforms

**Windows support is dropped.** mneme supports Linux and macOS. There
is no Windows binary, CI does not build or test on Windows, the
Windows-specific code is removed, and a non-Unix build stops with a
compile error. On a Windows machine, run mneme inside WSL2, or reach a
`mneme serve` running elsewhere over HTTP.

## Security

Dependencies were refreshed for the RustSec advisories reported against
the 1.5.0 lockfile: `zstd`, `rustls`, `rustls-webpki`, `webpki-roots`,
`quinn-proto` and `quinn-udp` by lockfile update, and `zip` through
`candle-core` 0.10 → 0.11. `cargo deny check` is clean. With
`log_tool_call_values = "ids"` the timeline keeps a hash and a length
instead of the content of every write.

## For contributors

- **`mneme eval`** (above) is the quality check for retrieval changes.
- **The perf gate is a same-runner A/B.** `perf.yml` builds the base
  commit and the change, runs them in three interleaved rounds and
  compares each side's best median at 15%. The frozen baseline files are
  informational. Locally: `benches/baselines/ab_gate.sh main`.
- **Structural quality checks** run on every PR against a synthetic
  corpus (`tests/quality_structural.rs`): what `recall` and auto-context
  must and must not return.
- Importable GitHub rulesets for `main`, release tags and dev branches
  are in the repository.

## Upgrading

- Binary: `brew upgrade mneme` or `cargo install mneme-mcp`. Restart
  running servers. No migration runs.
- Take a `mneme backup` first if you want a way back: a 1.5 binary opens
  a 1.6 store, but it does not know archived memories or `updated_at`.
- Hermes installs: re-run `mneme init hermes --upgrade` with the same
  `--hermes-home` / `--url` as the first install (and `--scope-map` if
  you want scoped reads). The hook and skill are copies; upgrading the
  binary does not update them. Upgrade the server before the hook.
- If you relied on events reaching the cold archive to keep redb small,
  set `[consolidation] cold_exempt_kinds = []`.
