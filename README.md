# Mneme

> A standalone, MCP-native memory tool for any LLM or agent.
> Single binary. Local-first. Rust. Built to last.

**Status:** Pre-alpha — Phase 1 (MCP foundation) in progress.
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

## What it looks like (target UX)

```
$ brew install mneme            # or curl install, or cargo install
$ mneme init
$ mneme run
```

Then in your MCP host config (e.g., `claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "mneme": {
      "command": "mneme",
      "args": ["run"]
    }
  }
}
```

Restart the host. Now the agent has memory.

## Status

Working today:

- `mneme --help` and subcommand stubs
- Scaffolded module layout per spec §6.4
- Trait seams (`Storage`, `Embedder`, `VectorIndex`) for v1.0 stability
- **`mneme run`**: stdio MCP server speaking protocol `2025-06-18`,
  with three Phase 1 stub tools (`remember`, `recall`, `forget`) and
  one resource (`mneme://stats`). Survives malformed JSON, oversize
  frames, and EOF cleanly. End-to-end and property-tested.

Not yet:

- Persistence (Phase 2), embeddings + semantic recall (Phase 3),
  consolidation (Phase 4), auto-context (Phase 5).
- See [`proj_docs/mneme-implementation-plan.md`](proj_docs/mneme-implementation-plan.md) for phase-by-phase progress.

## Smoke-testing the MCP server

Once built, you can drive `mneme run` with any MCP host. To verify
manually with Claude Desktop on macOS:

1. Build the release binary: `cargo build --release`
2. Note its absolute path: `$(pwd)/target/release/mneme`
3. Add it to `~/Library/Application Support/Claude/claude_desktop_config.json`:

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

4. Restart Claude Desktop. The tools panel should show three tools
   (`remember`, `recall`, `forget`). Tool calls will succeed but stop
   short of persisting anything until Phase 2 lands.

To smoke from the shell without an MCP host:

```sh
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"shell","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
} | ./target/release/mneme run 2>/dev/null
```

You should see two JSON lines: an `initialize` response advertising the
server's capabilities, then a `tools/list` response with three tools.

## Building

```sh
cargo build --release
./target/release/mneme --help
```

Requires Rust stable (pinned via `rust-toolchain.toml`).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Spec is canonical; if reality
diverges from the spec, update the spec in the same commit.

## License

Apache-2.0. See [LICENSE](LICENSE).
