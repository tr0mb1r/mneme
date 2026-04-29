# Mneme

> A standalone, MCP-native memory tool for any LLM or agent.
> Single binary. Local-first. Rust. Built to last.

**Status:** Pre-1.0. Phases 0–5 complete; Phase 6 (portability + diagnostics)
substantially complete. The on-disk format is stable behind a versioned schema
with a migration path, but the project hasn't yet cleared its 30-day soak,
shipped a Homebrew formula, or published a release pipeline. Treat as
production-capable for personal use, not yet 1.0.

See [`proj_docs/mneme-project-specification-v2.md`](proj_docs/mneme-project-specification-v2.md)
for the canonical spec and [`proj_docs/mneme-implementation-plan.md`](proj_docs/mneme-implementation-plan.md)
for the build plan.

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
advertises 11 tools and 4 resources, and survives malformed JSON, oversize
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
- `switch_scope` tool and `mneme://session/{id}` resource
- L1 working-session checkpoint scheduler wiring

See [`proj_docs/mneme-implementation-plan.md`](proj_docs/mneme-implementation-plan.md)
for the canonical roadmap.

## Installing

There is no `brew install mneme` yet. Until the release pipeline lands, build
from source:

```sh
git clone https://github.com/vserkin/mneme && cd mneme
cargo build --release
cp target/release/mneme ~/.local/bin/   # or anywhere on $PATH
mneme init                               # scaffolds ~/.mneme and pulls the embedding model
```

`mneme init` writes `~/.mneme/config.toml` with all defaults made explicit;
edit it before first run if you want to override the embedding model, data
directory, or storage budget. The first `mneme run` downloads the embedding
model (~1.5 GB for the default `bge-m3`; switch to `minilm-l6` in
`config.toml` for a ~80 MB / faster-startup option).

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

3. Restart Claude Desktop. The tools panel should show 11 tools and 4 MCP
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
server's capabilities, then a `tools/list` response with all 11 tools.

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
