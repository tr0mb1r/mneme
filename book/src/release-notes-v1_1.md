# v1.1 release notes

> **Status: v1.1.1 candidate on `develop`.** This page covers
> everything in the v1.1 train; the launch tag will be `v1.1.1`
> (with `v1.0` tagged first per the release plan). The crate
> version on `develop` is `1.1.1` so dogfood binaries report
> the version they're being tested against.

The v1.1 cycle's theme is **daily friction, fixed**: install + run
mneme without spelunking; serve multiple MCP-host sessions from one
warm process; keep memory writes from accidentally absorbing 4 KB of
pasted Slack threads; preserve a full v1.0 → v1.1 → v1.0 rollback
path so you can downgrade without losing data.

---

## Headline changes

### `mneme init <agent>` installs without manual `cp` + JSON edits

**Before (v1.0):** copy three hook scripts from `docs/examples/`,
edit `~/.claude/settings.json` by hand to add the `mcpServers.mneme`
entry + the `hooks.SessionStart/PreCompact/Stop` block, restart
Claude Code, hope you got the JSON shape right.

**After (v1.1):**

```sh
mneme init claude-code     # or claude-desktop, cursor, opencode
```

That's it. Atomic write of every file (`tmpfile` + `fsync` +
`rename`); user content in the agent's settings file preserved
verbatim around the marker block; idempotent — re-run produces a
byte-identical state. `--upgrade` overwrites the mneme-owned files
with the new binary's content; `--uninstall` reverses every change
atomically; `--show` previews the plan without writing.

**Tier-1 agents wired today** (see [Supported agents] for the full
matrix and per-agent caveats):

[Supported agents]: ./agents.md

- **Claude Code** (reference implementation — hooks + `~/.claude.json`
  MCP entry + `~/.claude/CLAUDE.md`) — commit `6253226`.
- **Claude Desktop** (no hooks API; MCP entry only) — commit `6253226`.
- **Cursor** (`~/.cursor/mcp.json`) — commit `1afe24e`.
- **OpenCode** (`~/.config/opencode/opencode.json` + auto-loaded
  instruction file via `instructions[]`) — commit `1afe24e`.

The post-install message walks you through a 2-minute verification:
a Vim-keybindings remember/recall conversation that demonstrates
persistence across sessions.

`cline`, `codex`, and `gemini-cli` return `NotYetImplemented` with a
tracked task pointer; manual MCP-config edits still work for those.

### Daemon mode + Unix socket transport

**Before (v1.0):** every Claude Code session spawned a fresh
`mneme run` subprocess that paid the BGE-M3 cold-load + redb open +
HNSW snapshot replay every time. Two simultaneous Claude Code
sessions tripped the exclusive lockfile and the second exited with
"lock held."

**After (v1.1):**

```sh
mneme daemon
```

One long-lived process accepts multiple MCP clients concurrently
over the Unix domain socket at `~/.mneme/run/mneme.sock`. Storage
writes serialise through the existing single-writer seam (no new
concurrency primitives — ADR-0012 D8).

**Self-detach by default** (commit `1d3943f`, ADR-0012 D9): `mneme
daemon` spawns a detached child via `setsid(2)` (Unix) or
`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` (Windows), prints the
child PID, and the parent exits 0. The shell prompt returns
immediately and the daemon survives the spawning shell's exit. Pass
`--foreground` to keep it attached (the form a systemd / launchd
unit would use).

Lifecycle:

- Boot binds the socket (with stale-cleanup probe — orphaned socket
  files from a crashed daemon are detected via `connect()` and
  unlinked automatically).
- Idle timeout configurable via `[daemon] idle_timeout_minutes`
  (default 30; `0` disables). Counted from "last client
  disconnected" — long HNSW snapshots that block requests but not
  connections don't trigger spurious shutdowns.
- **Active-drain on SIGTERM** (commit `1664a8b`): per-connection
  tasks observe a `tokio::sync::watch<bool>` shutdown broadcast
  and exit promptly, dropping their socket halves so clients see
  EOF. Drain completes in milliseconds even with multiple
  connected clients. Pre-fix this always took the full
  `DRAIN_DEADLINE` (30 s) because tasks had no cancellation hook.

### Auto-spawn + auto-reconnect from `mneme client`

**Before:** the user had to remember to run `mneme daemon` before
any agent's MCP host would find a server. If the daemon restarted
mid-session (binary swap, idle-timeout, crash), the MCP host
returned `-32002` errors and the user had to `/exit` + reopen.

**After (commits `742003e` + `8846268`, ADR-0012 D12):** every Tier-1
installer wires the MCP host to launch `mneme client`. The client
bridge then handles two cases transparently:

- **Daemon not running.** Connect attempt sees `ENOENT` /
  `ECONNREFUSED`. Client spawns the daemon (via the shared detach
  primitive that `mneme daemon` uses), then polls the socket with
  exponential backoff (10 ms → 30 s budget; bumped from 5 s after
  cold-cache dogfood caught the model-loader startup time
  exceeding the original budget).
- **Daemon restart mid-session.** Client detects socket EOF, polls
  for a new daemon with the same backoff, replays the MCP
  `initialize` handshake, and resumes the session. A background
  stdin reader buffers inbound frames via an mpsc channel during
  the reconnect window so nothing is dropped.

Regression tests in `tests/daemon_e2e.rs`:
`daemon_is_spawned_by_detach_primitive`,
`client_reconnects_after_daemon_restart`,
`client_auto_spawns_daemon_on_first_connect`.

### Per-installation auth token

**The token value lives in exactly one file:**
`~/.mneme/run/auth.token` (mode `0600`). Agent configs reference
the path, never the value — so the blast radius of a compromised
agent config doesn't include the token. `mneme auth rotate`
regenerates the token atomically; existing daemon connections
stay valid (the token check fires only at handshake), the next
new connection picks up the rotated value automatically.

Every connection presents the token as the first line of the
handshake (`MNEME-AUTH: <token>\n`); the daemon reads from disk on
every handshake (no in-memory caching that would defeat rotation
— caught + fixed during A.M5 testing).

### Content size guardrails

`remember` and `update` enforce three tiers:

- **< 500 chars** — stored silently.
- **500–2,000 chars** — stored, response includes a
  `length_advisory` field.
- **2,000–10,000 chars** — stored, response includes a stronger
  `length_warning` field; logged at `info` level.
- **> 10,000 chars** — rejected with a structured error
  (`memory_too_large`) + suggestion to extract the key insight or
  summarize. Embedding is never performed for rejected content.

The 10,000-char ceiling is configurable via
`[budgets] max_remember_chars`. Existing oversized memories
(written under v1.0 which had no limit) remain readable +
`recall`-able — the verbatim principle is preserved; only NEW
writes/updates are gated.

**Observability for rejections** (commit `6529302`): rejected
writes emit a structured `tool_call_failed` L3 event so future
recall sees the rejection trail without re-recording the rejected
payload.

`mneme://stats.memories.large_memory_count` reports the per-tier
distribution of your existing corpus + the IDs of any memories
above the ceiling — useful for trimming after the upgrade.

### First-boot upgrade audit

On v1.1's first boot against an existing data directory, the binary
scans L4 once for memories above the new ceiling and writes a
passive summary to `~/.mneme/diagnostics.log`. **Existing memories
are never auto-modified** — you find what's there and decide what to
do.

Gated by `~/.mneme/run/upgrade-audit.done` so it runs at most once
per data directory; delete the marker to force a re-audit (useful
after `mneme.forget`-ing oversized entries).

### `tools/call` responses gain `_meta` annotations

The MCP `_meta` field on tools/call responses now carries
mneme-specific structured annotations:

- `length_advisory` / `length_warning` on `remember` and `update`
- `error.code = "memory_too_large"` on the rejection path

Existing MCP clients that don't read `_meta` keep working — the
text content channel still carries human-readable equivalents.

### Default scope = `global`

The pre-v1.1.x default scope was `personal`, an accidental holdover
from early scope sketches. v1.1 ships `global` as the new default
(commit `ba9739b`) for both fresh installs and the
missing-`config.toml` fallback path. Existing installs with
`default = "personal"` in `~/.mneme/config.toml` still work — the
agent-side convention (pinned globally) is "switch to a project
scope or `global` early in the session; never default to
`personal`."

---

## Operational changes

### File logging on by default

INFO logs are written to a rotating file at `~/.mneme/logs/mneme.log`
(commit `e4a6dd4`). Stderr stays at WARN+ for `mneme` and other
crates to avoid drowning the user; the file gets the full INFO
stream that's useful for debugging daemon behaviour (drain timings,
client accept/disconnect events, snapshot scheduler ticks). File
size + rotation count configurable via the `[logging]` section in
`config.toml`.

### Codebase refactor batch (test surface + module shape)

A coordinated refactor batch landed on 2026-05-16 to reduce
technical-debt surface before tackling the multi-agent scope-
isolation ADR:

- `0810116` — `MEM_KEY_PREFIX` const consolidated from 9 duplicate
  definitions into one `pub(crate)` in `src/storage/mod.rs`.
  Snapshot scheduler extracted from `src/memory/semantic.rs` (1517
  → 1385 lines) into a new `src/memory/snapshot_scheduler.rs`
  submodule.
- `2ebf91f` — failure-mode + unit test coverage added for MCP
  resources (`context.rs`, `procedural.rs`, `recent.rs`) and the
  `stats` aggregator + previously-untested tools
  (`summarize_session`, `recall_recent`, `unpin`). Promoted from
  P3 to high priority because every agent reads these on turn 1
  — silent breakage = silent onboarding regression.
- `318772d` — `tools/call` triage (99 lines: success / soft-error /
  hard-error dispatch + L3 emission) extracted from
  `src/mcp/server.rs` (1273 → 1003 lines) into
  `src/mcp/dispatch.rs`.
- `6dcbad5` — `ConnectionGuard` RAII for the daemon's
  `active_clients` counter, replacing three manual `fetch_sub` +
  `tracing::info!` sites. Includes a strengthened
  `daemon_drains_idle_clients_promptly_on_sigterm` regression test
  with stderr-log invariants that catch lifetime regressions in
  the guard (specifically, a class of bug where the guard would
  be created outside the spawned task scope and decrement the
  counter prematurely).

Net: 552 tests pass, clippy + fmt clean, no behavioural change for
end-users.

### Performance regression gate in CI

`.github/workflows/perf.yml` runs the four hot-path criterion
benches on every push / PR that touches `src/`, `benches/`, or
`Cargo.{toml,lock}`. Fails the build if any p95 regresses > 10 %
vs the frozen `benches/baselines/v0_2_6.json` baseline. Linux-only
for measurement stability. Justified regressions update the
baseline JSON in the same commit + explain in the PR description.

### `mneme backup` no longer leaks the auth token

Backup now skips `~/.mneme/run/` entirely (auth tokens, sockets,
lifecycle markers — none of which belong in a user-distributable
backup tarball). Privacy-only fix — no behavioural change for
anyone whose backup workflow doesn't include rolling back to v1.0
against a v1.1-populated data dir.

### Migration test as release gate

`tests/v1_0_to_v1_1_migration.rs` pins three things that v1.1.x
cannot ship without:

1. A v1.0-shaped `config.toml` parses cleanly under the v1.1
   binary (serde defaults backfill `[daemon]` and
   `max_remember_chars`).
2. Memories written under a v1.0-shaped config persist + recall
   across reboot (catches any accidental schema_version bump).
3. Explicit `[mcp].transport = "stdio"` is preserved on boot.

The sibling rollback test (`tests/v1_1_to_v1_0_rollback.rs`) ships
once the v1.0.1 backup-run-exclusion patch lands in a released
v1.0.1 binary that the rollback CI can boot against.

---

## Upgrading from v1.0

For most users, no action required:

```sh
brew upgrade mneme
mneme --version            # confirm v1.1.x
```

Re-running `mneme run` (or `mneme daemon`) against your existing
`~/.mneme/` boots clean — no schema migration (per ADR-0012
Invariant 1, v1.1 does not bump `schema_version`). Your existing
`mcpServers.mneme` config in Claude Code's `settings.json` keeps
working unchanged.

To take advantage of `mneme init claude-code`'s automatic hook
installation (you previously copied them by hand from
`docs/examples/`):

```sh
mneme init claude-code --upgrade
```

That writes the canonical hook scripts + `MNEME.md` + marker block
+ wires the agent to launch `mneme client` (so D12 auto-spawn +
auto-reconnect kick in). `--show` first if you want to see what
would change.

To surface any oversized memories from your v1.0 corpus:

```sh
cat ~/.mneme/diagnostics.log
```

The first-boot audit ran the size-tier scan automatically; the log
shows per-tier counts + the IDs of any memories above the new
ceiling.

---

## Rollback path

v1.1 → v1.0 rollback is a hard promise per ADR-0012 (release-
planning v2.1 §6.1):

- v1.1 does NOT bump on-disk `schema_version` (Invariant 1).
- v1.0.1 (a small patch released alongside v1.1) tolerates the
  v1.1-managed `~/.mneme/run/` directory cleanly — no warnings,
  no backup leaks of the auth token.

To roll back:

```sh
brew install mneme@1.0.1
mneme stop                                # if a daemon is running
brew unlink mneme && brew link mneme@1.0.1
mneme stats                                # confirm v1.0.1 boots clean
```

Memories carry forward identically. v1.1-only state under
`~/.mneme/run/` is silently ignored by v1.0.1 (sockets get unlinked
next time a v1.1.x runs; auth.token is untouched).

---

## What's NOT in v1.1

Per the v2.1 planning doc + post-cycle adjustments, deliberately out
of scope:

- **Per-connection scope isolation.** `ScopeState` is process-global
  in the daemon — `Cursor`'s `switch_scope("X")` clobbers a
  concurrently-connected Claude Code session's defaulted-scope
  writes. Fix is ADR-worthy (touches the wire protocol via a new
  `MNEME-AGENT` handshake field) and deferred to v1.1.x or v1.2.
  Workaround: pass `scope=<name>` explicitly on every write when
  running multiple agents concurrently.
- **SSE event-stream framing** for the daemon transport. v1.1 uses
  newline-delimited JSON over the Unix socket (same wire format as
  stdio); SSE comment-frame keepalive (ADR-0012 D7) is deferred to
  v1.1.x or v1.2 alongside the framing switch.
- **Hybrid search (BM25 + dense)** and **encryption at rest** —
  Tier-2 features deferred to v1.2.
- **cline / codex / gemini-cli installers** — Tier-2 deferred (see
  [Supported agents] for status).
- **Windows named-pipe daemon support** (`\\.\pipe\mneme-{user_sid}`)
  — code wired in `src/cli/daemon.rs` but not CI-tested; promotes
  from "code present" to "supported" once the M4 windows CI matrix
  lands.

See ADR-0012 amendment A1 for the full keepalive deferral
rationale, and release-planning v2.1 §2.6 for the cut order.
