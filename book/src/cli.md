# CLI surface

`mneme` ships a single binary with eight subcommands. This page is the
canonical inventory.

The MCP server runs under either `mneme daemon` (v1.1 long-lived
process; default after `mneme init claude-code`) or `mneme run`
(single-host fallback). Agents connect via `mneme client`, a thin
stdio↔socket bridge — that's the subcommand listed in
`mcpServers.<name>.args` for the v1.1 default flow. Every other
subcommand is operational glue — scaffolding, diagnostics, dump,
stop, archive. The agent-facing surface lives in
[MCP surface](./mcp-surface.md); nothing on this page is reachable
through MCP.

Default data directory is `~/.mneme/` (override with `MNEME_DATA_DIR` —
see [Configuration](./configuration.md#environment-overrides)).

## Two MCP server modes: `daemon` vs `run`

mneme ships **two interchangeable MCP server entry points**, plus a
thin per-agent bridge. Pick the one that matches your install shape;
both serve the same MCP surface against the same `~/.mneme/` data
directory.

### Mode A — daemon mode (v1.1 default)

```
Claude Code     ─spawns─▶  mneme client  ─unix sock─▶  mneme daemon  ─▶  ~/.mneme/
Claude Desktop  ─spawns─▶  mneme client  ─unix sock─▶       ▲
Cursor          ─spawns─▶  mneme client  ─unix sock─▶       │
                                                            │
                                          (one daemon, many clients)
```

- **Long-lived `mneme daemon`** binds `~/.mneme/run/mneme.sock` and
  serves many MCP clients concurrently. Holds `~/.mneme/.lock` for
  its lifetime. Pays the BGE-M3 + redb + HNSW cold-start cost
  exactly once. **Self-detaches by default** — invoke `mneme daemon`
  in a shell and the prompt returns immediately with the child's
  PID printed to stdout; Ctrl-C in the spawning shell does NOT kill
  the daemon. Pass `--foreground` for systemd / launchd unit files
  or when debugging. Stop with `mneme stop` or `kill <pid>`.
- **Per-agent `mneme client`** is what each MCP host actually spawns.
  It's a stdio↔socket bridge: reads `~/.mneme/run/auth.token`,
  connects to the daemon, writes the `MNEME-AUTH:` handshake, then
  byte-pipes stdin↔socket and socket↔stdout until either side closes.
  No lockfile, no MCP frame parsing, no lifecycle events.
- **`session_start` / `session_end` events** mark the **daemon's**
  boot/shutdown — NOT individual agent sessions. Agent sessions
  start and end with `mneme client` connect/disconnect, which emit
  no lifecycle events of their own.
- **`mneme init claude-code`** (and its sibling installers) writes
  `args: ["client"]` into the agent's MCP config, so this is the
  shape users land on by default.

**Pick this mode when:** you run multiple MCP hosts against the same
data dir (Claude Code + Claude Desktop + Cursor + …); you want one
warm process serving them all; you're on macOS/Linux/Windows with
working Unix-socket / named-pipe support.

### Mode B — single-host fallback (`mneme run`)

```
Claude Code  ─spawns─▶  mneme run  ─stdio─▶  (server in same process)  ─▶  ~/.mneme/
                              ▲
                              │
                  (also holds the exclusive lock; one host at a time)
```

- **Single-process `mneme run`** speaks JSON-RPC over stdin/stdout
  directly, no socket, no `client` bridge. Same boot path as
  `mneme daemon` (`execute_with_mode`); same `session_start` /
  `session_end` emit semantics; same `~/.mneme/.lock` acquisition.
  Differs only in the transport: stdio instead of accept-loop.
- The MCP host (e.g. Claude Code) spawns `mneme run` directly with
  no shared socket. When the host exits, stdin closes, the run loop
  unwinds, the lock drops.
- Manual install: `claude mcp add … mneme run` (instead of
  `… mneme client`).

**Pick this mode when:** you're running exactly one MCP host against
the data dir; you're in a restricted environment where Unix sockets
or daemonization are awkward (CI, sandbox, container shell);
debugging — `mneme run </dev/null` reproduces an MCP boot
end-to-end without needing a daemon to be up.

### Decision matrix

| Question | If yes, prefer |
|---|---|
| Multiple MCP hosts share one data dir? | daemon + client |
| Want to amortise the BGE-M3 cold-start cost? | daemon + client |
| Single host, want minimum moving parts? | run |
| Restricted shell / CI / sandbox without socket support? | run |
| Debugging mneme itself end-to-end on stdin? | run |
| Want `mneme init claude-code`'s default? | daemon + client |

### What `mneme client` is NOT

- It is **not** a different MCP server. It does no JSON-RPC parsing,
  no tool dispatch, no embedding. It's a transport adapter; the
  server lives in `mneme daemon` (or `mneme run` for the fallback).
- It does **not** hold `~/.mneme/.lock`. Many `mneme client`
  processes can run concurrently against one daemon.
- It does **not** emit `session_start` / `session_end` events. Those
  fire on the **server** process (daemon or run).
- It does **not** keep a copy of the auth token in memory beyond the
  spawn-time read. The token never lands in any agent config file —
  agent configs reference the path (`~/.mneme/run/auth.token`), not
  the value. Rotation via `mneme auth rotate` rewrites that one file
  and the next handshake picks up the new value.

### Where each subcommand lands in `mcpServers.<name>.args`

| Install path | `args` value | Notes |
|---|---|---|
| `mneme init claude-code` (v1.1 default) | `["client"]` | One daemon, many clients. Token reference by path. |
| `claude mcp add … mneme client` (manual, daemon mode) | `["client"]` | Same shape, hand-rolled. |
| `claude mcp add … mneme run` (manual, fallback) | `["run"]` | Single-host stdio. No daemon, no socket, no token file. |

## Subcommands (12)

### Lifecycle

| Subcommand | What it does |
|------------|--------------|
| `mneme init` | Scaffold `~/.mneme/` (config, schema_version, directory layout) and run the schema migration to the binary's `CURRENT_SCHEMA_VERSION`. Idempotent — safe to rerun; only fills in missing pieces. Writes `config.toml` if absent; leaves an existing file alone. |
| `mneme init <agent> [--upgrade\|--uninstall\|--show]` | v1.1 per-agent installer per ADR-0012 / release-planning §4. `<agent>` ∈ {`claude-code`, `claude-desktop`, `cursor`, `cline`, `codex`, `gemini-cli`}. Today `claude-code` and `claude-desktop` ship fully wired; the others return `NotYetImplemented` with a tracked task pointer until each integration is validated end-to-end on a real install. Atomic (tmpfile + fsync + rename per file), idempotent (re-run is byte-identical), reversible (`--uninstall` removes mneme-owned files + entries while preserving every other key in the user's config files). `--show` previews the plan without writing. |
| `mneme daemon [--foreground]` | **v1.1 default MCP server entry point** per ADR-0012. Binds `~/.mneme/run/mneme.sock` (Unix domain socket on macOS / Linux; Windows named pipe lands in M4), accepts multiple MCP clients concurrently, gates each connection on the auth handshake (`MNEME-AUTH: <token>\n` per ADR-0012 D3), serves them via `tokio::spawn` per connection. Acquires `~/.mneme/.lock` for the lifetime of the process. Storage writes serialise through the single-writer seam (D8). Auto-shuts-down after `[daemon].idle_timeout_minutes` (default 30; `0` disables) of no clients. SIGTERM drains in-flight clients up to 30 s before runtime tears them down. Emits `session_start` on boot and `session_end` on graceful exit (ADR-0009). **Self-detaches by default** (D9): the parent spawns a detached child, prints the child PID to stdout, and exits 0 — the shell prompt returns immediately and Ctrl-C in the spawning shell does not kill the daemon. Pass `--foreground` to skip self-detach (right for systemd / launchd unit files and for debugging). Stop a detached daemon with `mneme stop` or `kill <pid>`. |
| `mneme client` | **Per-agent stdio↔socket bridge** (v1.1). MCP hosts (Claude Code, Claude Desktop, Cursor, …) spawn this as their per-session subprocess. It reads `~/.mneme/run/auth.token`, opens the socket, writes the `MNEME-AUTH:` handshake, then byte-pipes stdin↔socket and socket↔stdout until either side closes. Holds no lockfile, parses no MCP frames, emits no lifecycle events — pure transport adapter. The token value never lands in any agent config file (Invariant 3). |
| `mneme run` | **Single-host MCP server fallback.** Speaks JSON-RPC over stdio against MCP `2025-06-18`. Acquires `~/.mneme/.lock` for the lifetime of the process; refuses to boot if another instance holds it. Right pick when the host spawns mneme directly with no shared daemon (debugging, restricted environments, CI/test). For multi-session sharing, use `mneme daemon` + `mneme client` instead. SIGTERM / Ctrl-C / SIGINT are treated as a clean exit. Same `session_start`/`session_end` emit semantics as `mneme daemon` — both share the boot path via `execute_with_mode`. |
| `mneme stop` | Find the running server via the lockfile, send SIGTERM, and wait up to 10 s for it to drop the lock. Stale lockfile (PID no longer running) is cleaned up automatically. Exits 0 on either path; non-zero only if the process is alive but won't exit within the timeout. Works for both `mneme daemon` and `mneme run`. |
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

`~/.mneme/.lock` is a PID file held exclusively by whichever MCP server
is serving the data dir — `mneme daemon` or `mneme run`. Four classes
of subcommand interact with it:

- **Acquires:** `mneme daemon` and `mneme run` (held for the lifetime
  of the process). Both share the same `execute_with_mode` boot path,
  so the lock semantics are identical; only the transport differs.
- **Bridges to a holder without taking the lock:** `mneme client`. The
  bridge does not touch the lockfile — it just opens the daemon's
  socket and pipes bytes. Multiple `mneme client` processes can run
  concurrently against one daemon.
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
