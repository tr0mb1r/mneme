# Mneme

[![CI](https://github.com/tr0mb1r/mneme/actions/workflows/ci.yml/badge.svg)](https://github.com/tr0mb1r/mneme/actions/workflows/ci.yml)
[![cross-build](https://github.com/tr0mb1r/mneme/actions/workflows/cross-build.yml/badge.svg)](https://github.com/tr0mb1r/mneme/actions/workflows/cross-build.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

> A standalone, MCP-native memory tool for any LLM or agent.
> Single binary. Local-first. Rust. Built to last.

**Status:** Pre-1.0 — `0.2.x` line, latest `0.2.4` published as
[`mneme-mcp`](https://crates.io/crates/mneme-mcp) on crates.io and as
[`tr0mb1r/mneme`](https://github.com/tr0mb1r/homebrew-mneme) on Homebrew.
Phases 0–5 complete; Phase 6 (portability + diagnostics + release
infrastructure) substantially complete. Code-side feature work and release
infrastructure are done; the remaining gates are calendar-bound: 30-day soak
on real workloads (Day 0 = 2026-04-29) and one full release cycle without
bumping `schema_version`. The on-disk format is stable behind a versioned
schema with a migration path. Treat as production-capable for personal use,
not yet 1.0. See [`book/src/versioning.md`](book/src/versioning.md) for the
versioning policy and the five 1.0 gates.

## What it is

Mneme is a persistent memory tool for AI agents. It runs as a long-lived
process on your machine, exposes its functionality via the Model Context
Protocol (MCP), and lets any compatible agent — Claude Desktop, Claude
Code, Cursor, Cline, Aider — remember things across sessions.

The clearest one-line description: **Mneme remembers things about your
work that the agent would otherwise forget.**

## What it isn't

- A vector database (it uses one internally, but that's an implementation detail)
- A RAG framework
- A codebase indexer (modern agents read code with shell tools — that's not Mneme's job)
- A web service or SaaS
- A library to embed in another application
- An LLM

## Setup

- **Claude Code** — see [`docs/CLAUDE_CODE_SETUP.md`](docs/CLAUDE_CODE_SETUP.md)
  for the full guide (recommended path for terminal users).
- **Claude Desktop / other MCP hosts** — see
  [Smoke-testing the MCP server](#smoke-testing-the-mcp-server) below.
- **Understanding what mneme actually stores** — see
  [`docs/MEMORY_LAYERS.md`](docs/MEMORY_LAYERS.md) for a per-layer
  walkthrough of hot/warm/cold tiers, embedding cadence, snapshot
  schedules, and what's wired vs. deferred today.

## What works today

| Layer | Tools | Resource | Storage |
|-------|-------|----------|---------|
| L0 procedural (always-on) | `pin`, `unpin` | `mneme://procedural` | JSONL on disk, hot-reloaded |
| L1 working session | (live state) | `mneme://session/{id}` | `~/.mneme/sessions/<id>.snapshot` |
| L3 episodic (recent events) | `recall_recent`, `summarize_session`, `record_event` | `mneme://recent` | redb hot tier + zstd cold quarters |
| L4 semantic (long-term facts) | `remember`, `recall`, `update`, `forget` | — | redb + WAL + HNSW vector index |
| Auto-context | — | `mneme://context` | All four layers, packed to a token budget |
| Diagnostics | `stats`, `list_scopes`, `export`, `switch_scope` | `mneme://stats` | — |

`mneme run` speaks JSON-RPC over stdio against MCP protocol `2025-06-18`,
advertises a focused MCP tool and resource surface (see
[`book/src/mcp-surface.md`](book/src/mcp-surface.md) for the
authoritative inventory), and survives malformed JSON, oversize
frames, and EOF cleanly. Real BGE-M3 / MiniLM embeddings via `candle`,
HNSW recall via `instant-distance`, atomic snapshots, WAL crash-recovery,
schema migration from v0, and `mneme backup` / `mneme restore` round-trips
are all in place. Optional Claude Code lifecycle hooks
(`SessionStart`/`PreCompact`/`Stop`) are documented in
[`docs/CLAUDE_CODE_SETUP.md`](docs/CLAUDE_CODE_SETUP.md) §7 with
ready-to-copy scripts in
[`docs/examples/claude-code-hooks/`](docs/examples/claude-code-hooks/).

## Roadmap to 1.0

- 30-day soak on real workloads (Day 0 = 2026-04-29)
- One full release cycle without bumping `schema_version`

Code-side feature work, release infrastructure (Homebrew tap,
crates.io publish, tag-driven cross-build pipeline, mdBook-rendered
user docs at <https://tr0mb1r.github.io/mneme/>), and the MCP-surface
freeze (see
[`book/src/mcp-surface.md`](book/src/mcp-surface.md) for the
authoritative inventory) are complete. The remaining items are
calendar-bound rather than work-bound.

## Installing

Three install paths, each producing the same `mneme` binary on your
`$PATH`. See the [installation page](https://tr0mb1r.github.io/mneme/installation.html)
in the user docs for the full walkthrough.

### Homebrew (macOS, Linux) — recommended

```sh
brew tap tr0mb1r/mneme
brew install mneme
```

Pre-built static binary; no Rust toolchain. Apple Silicon, Intel macOS, and
aarch64 / x86_64 Linux (musl-static).

### `cargo install`

```sh
cargo install mneme-mcp
```

The crate is `mneme-mcp` on crates.io (the bare `mneme` name is held by an
unrelated event-sourcing library); the installed binary is `mneme`. Requires
Rust stable.

### From source

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
scripts/install.sh             # build, install on $PATH, scaffold ~/.mneme
scripts/install.sh --minilm    # same, but default to MiniLM (~80 MB) instead of BGE-M3 (~1.5 GB)
```

`scripts/install.sh` picks `~/.local/bin` (or `/usr/local/bin` if writable),
runs `cargo build --release`, copies the binary, runs `mneme init`, and
prints the exact `claude mcp add` line for the next step. Idempotent — safe
to re-run when you pull. Pass `--prefix <dir>` to install elsewhere or
`--no-init` to skip the data-directory scaffold.

### After install

Both Homebrew and `cargo install` skip `mneme init` by design (a formula
and `cargo install` should not modify `$HOME`). Run it once manually:

```sh
mneme init
```

`mneme init` writes `~/.mneme/config.toml` with all defaults made explicit;
edit it before first run if you want to override the embedding model, data
directory, or storage budget. The first `mneme run` downloads the embedding
model:

| Model | Size | Speed | Recall | When to pick |
|-------|------|-------|--------|--------------|
| `bge-m3` (default) | ~1.5 GB | slower cold start | top-tier, multilingual | You want the best recall and don't mind the disk + first-boot wait. |
| `minilm-l6` | ~80 MB | sub-second cold start | good for English | You want fast onboarding, English-only is fine, or you're testing before committing to BGE-M3. |

Switching models later re-embeds every stored memory automatically; no
manual reindex.

Building from source requires Rust stable, pinned via `rust-toolchain.toml`.

## Smoke-testing the MCP server

Once built, you can drive `mneme run` with any MCP host. To verify
manually with Claude Desktop on macOS:

1. Note the binary's absolute path: `$(pwd)/target/release/mneme`
2. Add it to `~/Library/Application Support/Claude/claude_desktop_config.json`:

   ```json
   {
     "mcpServers": {
       "mneme": {
         "command": "/absolute/path/to/target/release/mneme",
         "args": ["run"]
       }
     }
   }
   ```

3. Restart Claude Desktop. The tools panel should list `mneme` with its
   full tool inventory (see
   [`book/src/mcp-surface.md`](book/src/mcp-surface.md)) and resource
   set. The first call may take a few seconds while the embedding
   model loads.

To smoke from the shell without an MCP host:

```sh
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"shell","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
} | ./target/release/mneme run 2>/dev/null
```

You should see two JSON lines: an `initialize` response advertising the
server's capabilities, then a `tools/list` response enumerating every
registered tool (see
[`book/src/mcp-surface.md`](book/src/mcp-surface.md) for the canonical
list).

For the comprehensive end-to-end check (every tool, backup/restore
round-trip, post-restore recall) run:

```sh
scripts/manual_test.sh --stub        # offline, ~10s, no model download
scripts/manual_test.sh               # real MiniLM, exercises the embedder
```

## Building

```sh
cargo build --release
./target/release/mneme --help
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Spec is canonical; if reality
diverges from the spec, update the spec in the same commit.

## License

Apache-2.0. See [LICENSE](LICENSE).
