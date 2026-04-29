# Mneme

[![CI](https://github.com/tr0mb1r/mneme/actions/workflows/ci.yml/badge.svg)](https://github.com/tr0mb1r/mneme/actions/workflows/ci.yml)
[![cross-build](https://github.com/tr0mb1r/mneme/actions/workflows/cross-build.yml/badge.svg)](https://github.com/tr0mb1r/mneme/actions/workflows/cross-build.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

> A standalone, MCP-native memory tool for any LLM or agent.
> Single binary. Local-first. Rust. Built to last.

**Status:** Pre-1.0. Phases 0–5 complete; Phase 6 (portability + diagnostics)
substantially complete. The on-disk format is stable behind a versioned schema
with a migration path, but the project hasn't yet cleared its 30-day soak,
shipped a Homebrew formula, or published a release pipeline. Treat as
production-capable for personal use, not yet 1.0.

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
| L3 episodic (recent events) | `recall_recent`, `summarize_session` | `mneme://recent` | redb hot tier + zstd cold quarters |
| L4 semantic (long-term facts) | `remember`, `recall`, `update`, `forget` | — | redb + WAL + HNSW vector index |
| Auto-context | — | `mneme://context` | Pinned + recent, packed to a token budget |
| Diagnostics | `stats`, `list_scopes`, `export` | `mneme://stats` | — |

`mneme run` speaks JSON-RPC over stdio against MCP protocol `2025-06-18`,
advertises 12 tools and 5 resources, and survives malformed JSON, oversize
frames, and EOF cleanly. Real BGE-M3 / MiniLM embeddings via `candle`,
HNSW recall via `instant-distance`, atomic snapshots, WAL crash-recovery,
schema migration from v0, and `mneme backup` / `mneme restore` round-trips
are all in place. Optional Claude Code lifecycle hooks
(`SessionStart`/`PreCompact`/`Stop`) are documented in
[`docs/CLAUDE_CODE_SETUP.md`](docs/CLAUDE_CODE_SETUP.md) §7 with
ready-to-copy scripts in
[`docs/examples/claude-code-hooks/`](docs/examples/claude-code-hooks/).

## Roadmap to 1.0

- Homebrew formula and a release pipeline that ships prebuilt binaries
- mdBook-rendered user docs site
- 30-day soak on real workloads

Feature work for v1.0 is complete; the remaining items are release
infrastructure.

## Installing

There is no `brew install mneme` yet. Until the release pipeline lands, build
from source. The fastest path is the bundled installer:

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
scripts/install.sh             # build, install on $PATH, scaffold ~/.mneme
scripts/install.sh --minilm    # same, but default to MiniLM (~80 MB) instead of BGE-M3 (~1.5 GB)
```

`scripts/install.sh` picks `~/.local/bin` (or `/usr/local/bin` if writable),
runs `cargo build --release`, copies the binary, runs `mneme init`, and
prints the exact `claude mcp add` line for the next step. It's idempotent —
safe to re-run when you pull. Pass `--prefix <dir>` to install elsewhere or
`--no-init` to skip the data-directory scaffold.

If you'd rather drive each step yourself:

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
cargo build --release
cp target/release/mneme ~/.local/bin/   # or anywhere on $PATH
mneme init                               # scaffolds ~/.mneme and pulls the embedding model
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

Requires Rust stable, pinned via `rust-toolchain.toml`.

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

3. Restart Claude Desktop. The tools panel should show 12 tools and 5 MCP
   resources. The first call may take a few seconds while the embedding
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
server's capabilities, then a `tools/list` response with all 12 tools.

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
