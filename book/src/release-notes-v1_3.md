# v1.3 release notes

> **Status: v1.3.0 shipped 2026-07-25.**
> Previous: v1.2.2 (2026-06-07).

The v1.3 cycle's theme is **retrieval that actually retrieves**, plus a
documentation-truth pass. It came out of a full review of the tree
against its own docs, which turned up several places where
[MCP surface](./mcp-surface.md) promised behaviour the binary did not
have — most seriously that `mneme://context`, the resource the setup
docs tell agents to read at session start, never returned a single
semantic memory.

This is a **minor** bump per [Versioning](./versioning.md): the MCP wire
surface gains tools and parameters but breaks nothing, and the on-disk
format is unchanged. `schema_version` stays 1. No migration.

---

## The headline fix — `mneme://context` was missing two of its four layers

**Before (v1.0–v1.2):** the resource handler passed no query to the
orchestrator and emitted a hardcoded `"semantic": []`. Nothing an agent
ever stored with `remember` could reach auto-context. The scoring path
for L4 in `orchestrator::assembly` existed, was tested, and was dead
code in production. Separately, the `working` (L1) section was scored
and charged against the token budget but never written into the
response body — so auto-context under-delivered content relative to the
budget it reported. And `build_context` was called with `scope: None`,
so `switch_scope` had no effect on the resource at all.

If your agent read `mneme://context` and concluded mneme had forgotten
something, this is why.

**After (v1.3):** all four layers are assembled and all four are
returned. The resource takes query parameters:

```
mneme://context                              → L0 + L1 + L3
mneme://context?q=how%20do%20we%20deploy     → + L4 semantic hits
mneme://context?q=deploy&scope=work          → all four layers, work scope only
mneme://context?q=deploy&limit=20
```

Semantic hits require `q` — without a query there is nothing to be
similar *to*. Junk parameters are ignored rather than erroring, so a
host that appends its own query string still gets a context payload.

`mneme://context{?q,scope,limit}` is now advertised through
`resources/templates/list` (see below), so a spec-compliant client can
discover the parameters instead of reading this page.

---

## Filtered recall no longer silently underfills

`SemanticStore::recall` probed a fixed `limit × 4` candidates from the
HNSW index and *then* post-filtered by scope and kind. Ask for 10
`work`-scoped memories out of a corpus that is a few percent `work` and
you got one or two — or none — while the index held plenty. The failure
was silent: a short result set is indistinguishable from a sparse
corpus.

The probe now widens geometrically until it has `limit` survivors or
the index is exhausted. Unfiltered recall still issues exactly one
probe, so the common path pays nothing for this — the `recall`
benchmark moved *down* 17 % on unchanged hardware. A regression test
pins the contract.

---

## Retrieval controls on `recall`

Two new filters and one new result field:

- **`tags`** — a memory must carry **every** listed tag (AND, not OR).
- **`min_similarity`** — a cosine-similarity floor. A query with no
  good match can now return nothing instead of the nearest `limit`
  results however far away they are.
- **`similarity`** on each result row — `1 - score`, where `1.0` is
  identical. The pre-existing `score` is a cosine *distance* (lower is
  closer), which was easy to misread as a relevance number. Both are
  returned; `min_similarity` uses the `similarity` orientation.

Callers that omit the new parameters see unchanged behaviour.

---

## Per-project scoping without `switch_scope`

```toml
[scopes]
derive_from_roots = true   # default: false
```

With this on, each connection's default scope is derived from the MCP
client's declared workspace roots — a host opened on
`~/code/billing-api` writes into scope `billing-api` with no
`switch_scope` call. Because `initialize` is per-connection, this works
correctly in daemon mode where one process serves many hosts
concurrently.

Precedence: `MNEME_SCOPE` (per-process, applies to `mneme run`) beats
roots derivation, which beats `[scopes] default`. Explicit beats
inferred.

Off by default, because turning it on changes where new memories land.
See [Configuration](./configuration.md#derive_from_roots).

---

## Near-duplicate advisory on `remember`

When new content closely restates an existing memory (cosine ≥ 0.95),
the `remember` response carries `_meta.duplicate_advisory` naming the
existing id and its content. **The write still lands** — mneme stores
what it is told, verbatim — but the agent can now choose to `update`
the existing memory instead of growing the corpus forever.

It costs one index probe and **no extra embedding**: the check reuses
the vector already computed for the write.

---

## `resources/templates/list`

Previously `method not found`, which left `mneme://session/{id}`
undiscoverable to any spec-compliant client. It now advertises
`mneme://session/{id}` and `mneme://context{?q,scope,limit}`.

## Tool annotations and titles on all 13 tools

Every `tools/list` entry now carries a `title` plus all four behaviour
hints — `readOnlyHint`, `destructiveHint`, `idempotentHint`,
`openWorldHint` — stated explicitly, because the spec's defaults for
the last two are wrong for this surface. Before this, a host could not
tell `forget` from `stats`, so it had to prompt for everything or
nothing.

---

## Orphan-vector sweep

Each consolidation pass now tombstones indexed vectors whose metadata
row is gone — the residue a `forget` interrupted between its two writes
leaves behind. Such vectors were already invisible to `recall`, but
they still burned RAM and consumed a slot in every search's candidate
budget.

The count is reported as `consolidation.last_orphans_reclaimed` on
`mneme://stats`. A persistently non-zero value is worth investigating,
not just reclaiming.

---

## Fixes

### `mneme init` wrote an unparseable `config.toml` on Windows

The starter template interpolated paths into TOML *basic* strings,
where `\U` in `C:\Users\...` begins an 8-digit unicode escape — so
`mneme init` emitted a file the next `mneme run` refused to read
(`invalid unicode 8-digit hex code`). Every string value now goes
through `toml::Value`'s own writer, which picks a literal string for a
path containing backslashes, a multi-line literal when it also contains
an apostrophe, and a basic string otherwise.

Introduced and fixed within this release — **no published version is
affected.**

### `mneme init` no longer writes reserved settings

New configs no longer contain `[storage] encryption`, `[mcp] sse_port`,
or the `[telemetry]` section — three keys that are parsed and ignored.
The starter `config.toml` is now commented. All three still **load**
without error, so existing configs keep working untouched; a test fails
the build if the template ever drifts from `Config::default()`.

### Dependency advisories

Five transitive dependencies bumped past newly-published advisories:
`anyhow` 1.0.104 (UB in `Error::downcast_mut`), `crossbeam-epoch`
0.9.20 (RUSTSEC-2026-0204, invalid pointer dereference), `quinn-proto`
0.11.16 (RUSTSEC-2026-0185, remote memory exhaustion), `memmap2`
0.9.11 (RUSTSEC-2026-0186, unchecked pointer offset), and `spin` 0.9.9
(yanked). Lockfile-only — no manifest change, no new dependency,
nothing added to `deny.toml`'s ignore list.

---

## Documentation

- **[Roadmap — what isn't built yet](./roadmap.md)** is new: the single
  public list of deliberately-deferred work. Previously every deferred
  item lived in a source comment, so the only way to answer "is this
  supported?" was to read the tree.
- **Windows is `mneme run` only.** The README and
  [Installation](./installation.md) now say so plainly — `mneme daemon`,
  `mneme client`, and `mneme stop` need Unix domain sockets, while the
  release pipeline ships a Windows binary and the docs had presented
  daemon mode as the default. Configure your MCP host with
  `args: ["run"]` there.
- **Dead links removed.** Public links pointed into `proj_docs/`, which
  is gitignored and has never been committed, so every one of them
  404'd (the ADR-0013 link from the encryption chapter, the ADR
  cross-link convention in `CONTRIBUTING.md`, the "spec is canonical"
  pointer in `README.md`).
- **Stale module docs corrected** — several described shipped features
  as pending: the `tools` registry ("Phase 1 ships three stubs … that
  don't yet touch storage"), `resources` (`mneme://session/{id}` "still
  deferred"), `cli::daemon` (idle-timeout and graceful drain listed as
  pending; both landed in v1.1.1), `daemon` (serve loop "lands in a
  follow-up commit"), `index` (snapshot/load "will grow to satisfy
  it"), and `[daemon]` config ("isn't fully wired yet"). Also fixed
  `docs/MEMORY_LAYERS.md` calling session snapshots `<id>.json` when the
  code writes `<id>.snapshot`, and [CLI surface](./cli.md) omitting
  `opencode` from the installer's agent set.
- **Benchmark baselines** — `benches/baselines/README.md` now documents
  the platform mismatch the perf gate has always had: the frozen
  baseline was captured on Apple Silicon while `perf.yml` runs on
  x86_64 Linux, which leaves `auto_context/no_query` permanently near
  the failure threshold and gives `remember` ~4.8x of slack. Adds a
  median-of-N capture procedure (three consecutive runs of identical
  code varied 80 % on one VM), a platform-matched
  `v1_2_2_linux_x86_64.json` reported non-blocking, and the promotion
  procedure. The blocking reference is unchanged.

---

## Internal / CI

`benches/encryption_overhead.rs` now runs in `perf.yml`. It was written
for v1.2 and never wired in, so the AEAD layer had no CI signal at all.
It is gated by `check_crypto_overhead.py` as a *ratio* of encrypted to
plain within one run — unlike absolute nanoseconds, a ratio transfers
between the capture host and the runner.

**754 tests pass** (was 702), 0 failures. New coverage: the
scope-minority recall underfill regression, unmatchable-filter
termination, tag AND semantics, the similarity floor, context
query-parameter parsing (including lenient handling of junk), semantic
fold-in and scope filtering through the resource,
`resources/templates/list`, roots derivation plus its default-off and
no-roots paths, annotation correctness for all 13 tools, near-duplicate
detection including coexistence with the size advisory, and the orphan
sweep both directly and through the scheduler.

---

## Upgrading from v1.2

No action required. `schema_version` is 1 in v1.3 (unchanged since
v1.0); the v1.3 binary reads an existing `~/.mneme/` data directory
as-is, encrypted or not.

```sh
brew upgrade mneme
mneme --version   # confirm v1.3.0
mneme stop        # if a daemon is running; your MCP host restarts it
```

Two things worth doing after the upgrade:

1. **Seed `mneme://context` with a query.** Agents that read the bare
   `mneme://context` still get L0 + L1 + L3 only. To get semantic
   recall into auto-context, read `mneme://context?q=<what you're
   working on>`. If your agent instructions hardcode the resource URI,
   update them.
2. **Consider `derive_from_roots`** if you work across several repos
   with one daemon — it removes the per-session `switch_scope` call.
   It is off by default and changes where new memories land, so opt in
   deliberately.

---

## What's NOT in v1.3

- **SSE event-stream framing** — still deferred; `mcp.sse_port` remains
  reserved and unimplemented.
- **Hybrid search (BM25 + dense)** — no timeline.
- **cline / codex / gemini-cli installers** — still return
  `NotYetImplemented`; manual MCP-config edits work.
- **Windows daemon mode** — `mneme run` only, as above.
- **Automatic L3 → L4 contradiction detection** — invalidation stays
  agent-driven; mneme provides the primitives (`update`, `forget`), not
  a background job.

See [Roadmap](./roadmap.md) for the full list.
