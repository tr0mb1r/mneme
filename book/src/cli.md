# CLI surface

`mneme` ships a single binary with eight subcommands. This page is the
canonical inventory.

The MCP server runs under `mneme run`; every other subcommand is
operational glue — scaffolding, diagnostics, dump, stop, archive.
The agent-facing surface lives in [MCP surface](./mcp-surface.md);
nothing on this page is reachable through MCP.

Default data directory is `~/.mneme/` (override with `MNEME_DATA_DIR` —
see [Configuration](./configuration.md#environment-overrides)).

## Subcommands (11)

### Lifecycle

| Subcommand | What it does |
|------------|--------------|
| `mneme init` | Scaffold `~/.mneme/` (config, schema_version, directory layout) and run the schema migration to the binary's `CURRENT_SCHEMA_VERSION`. Idempotent — safe to rerun; only fills in missing pieces. Writes `config.toml` if absent; leaves an existing file alone. |
| `mneme init <agent> [--upgrade\|--uninstall\|--show]` | v1.1 per-agent installer per ADR-0012 / release-planning §4. `<agent>` ∈ {`claude-code`, `claude-desktop`, `cursor`, `cline`, `codex`, `gemini-cli`}. Today `claude-code` and `claude-desktop` ship fully wired; the others return `NotYetImplemented` with a tracked task pointer until each integration is validated end-to-end on a real install. Atomic (tmpfile + fsync + rename per file), idempotent (re-run is byte-identical), reversible (`--uninstall` removes mneme-owned files + entries while preserving every other key in the user's config files). `--show` previews the plan without writing. |
| `mneme run` | Start the MCP server. Speaks JSON-RPC over stdio against MCP `2025-06-18`. Acquires `~/.mneme/.lock` for the lifetime of the process; refuses to boot if another instance holds it. SIGTERM / Ctrl-C / SIGINT are treated as a clean exit. |
| `mneme daemon` | v1.1 daemon entry point per ADR-0012. Binds `~/.mneme/run/mneme.sock` (Unix domain socket on macOS / Linux; Windows named pipe lands in M4), accepts multiple MCP clients concurrently, gates each connection on the auth handshake (`MNEME-AUTH: <token>\n` per ADR-0012 D3), serves them via `tokio::spawn` per connection. Storage writes serialise through the existing single-writer seam (D8). Auto-shuts-down after `[daemon].idle_timeout_minutes` (default 30; `0` disables) of no clients. SIGTERM drains in-flight clients up to 30 s before runtime tears them down. |
| `mneme stop` | Find the running server via the lockfile, send SIGTERM, and wait up to 10 s for it to drop the lock. Stale lockfile (PID no longer running) is cleaned up automatically. Exits 0 on either path; non-zero only if the process is alive but won't exit within the timeout. |
| `mneme demo` | Print a 4-pattern walkthrough of the v1.1 memory surface (cross-session recall, `record_event`, `pin`, `mneme://context`). Pure text — pair with a real Claude Code / Claude Desktop session to see the patterns work end-to-end. Complements `mneme init <agent>`'s post-install prompt for users who want to come back to the patterns later. |

### Diagnostics

| Subcommand | What it does |
|------------|--------------|
| `mneme stats` | Print the same JSON `mneme://stats` returns: per-layer counts, schema version, applied LSN, scheduler health (consolidation + working blocks), current scope. Reads `~/.mneme/` directly; refuses while the lockfile is held (live writers would race). |
| `mneme inspect <ULID>` | Load a single memory by id from redb and pretty-print as JSON. |
| `mneme inspect --query <text>` | Boot the live embedder, run a `recall` against the on-disk HNSW (snapshot + WAL replay), and print the top-N hits as JSON. Both inspect modes refuse while the lockfile is held — stop the server first. Exactly one of `<ULID>` / `--query` is required. |
| `mneme export [--scope <s>] [--format json\|ndjson]` | Dump every memory across all three layers. `--format json` (default) is a single pretty-printed object with `procedural` / `episodic` / `semantic` keys. `--format ndjson` emits one row per line, each tagged with a `layer` key. `--scope <s>` filters all three layers by scope. Reads disk directly — no server needed; refuses while the lockfile is held. |

### Archive

| Subcommand | What it does |
|------------|--------------|
| `mneme backup <output> [--include-models]` | Tar+gzip the data directory to `<output>`. Excludes `~/.mneme/models/` (re-downloadable) and `~/.mneme/logs/` by default. Pass `--include-models` to ship a self-contained archive (~1–2 GB depending on the model). Symlinks are preserved as symlinks rather than followed. Refuses while the lockfile is held — a snapshot of in-flight WAL state would capture a torn write. |
| `mneme restore <input> [--force]` | Extract a `mneme backup`-produced archive back into the data directory. Atomic (temp+rename); refuses to overwrite an already-populated directory unless `--force` is given. Refuses while the lockfile is held. |

## Lockfile contract

`~/.mneme/.lock` is a PID file held exclusively by `mneme run`. Three
classes of subcommand interact with it:

- **Acquires:** `mneme run` (held for the lifetime of the server).
- **Refuses while held:** `mneme stats`, `mneme inspect`, `mneme export`,
  `mneme backup`, `mneme restore`. These all read or rewrite live state;
  racing the WAL/HNSW writer would risk a torn read or an inconsistent
  archive.
- **Reads to find the running server:** `mneme stop`.

`mneme stop` and the refuse-while-held subcommands tolerate stale
lockfiles where the recorded PID is no longer running — the lockfile
is cleaned up rather than treated as a fatal error.

## Verifying an install

```sh
mneme --help            # subcommand list and flags
mneme stats             # zeros on a fresh install; confirms data dir intact
mneme --version         # crate version (matches Cargo.toml)
```

A comprehensive end-to-end check covering every subcommand plus
backup / restore round-trips lives at `scripts/manual_test.sh`
(source clone only):

```sh
scripts/manual_test.sh --stub   # offline, ~10 s, no model download
scripts/manual_test.sh          # real MiniLM, exercises the embedder
```

## See also

- [MCP surface](./mcp-surface.md) — the agent-facing tools and resources.
- [Configuration](./configuration.md) — every `~/.mneme/config.toml`
  knob and the two environment overrides (`MNEME_DATA_DIR`,
  `MNEME_EMBEDDER`).
- [Troubleshooting](./troubleshooting.md) — common rough edges around
  lockfiles, model downloads, and `claude mcp list` failures.
