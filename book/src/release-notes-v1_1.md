# v1.1 release notes (draft)

> **Status: draft on `develop` branch.** This page tracks what's
> shipping in v1.1 as features land. The polished launch version
> ships with the v1.1.0 tag.

The v1.1 cycle's theme is **daily friction, fixed**: install + run
mneme without spelunking; serve multiple Claude Code sessions from
one warm process; keep memory writes from accidentally absorbing
4 KB of pasted Slack threads; preserve a full v1.0 → v1.1 → v1.0
rollback path so you can downgrade without losing data.

---

## Headline changes

### `mneme init <agent>` installs without manual `cp` + JSON edits

**Before (v1.0):** copy three hook scripts from `docs/examples/`,
edit `~/.claude/settings.json` by hand to add the `mcpServers.mneme`
entry + the `hooks.SessionStart/PreCompact/Stop` block, restart
Claude Code, hope you got the JSON shape right.

**After (v1.1):**

```sh
mneme init claude-code
```

That's it. Atomic write of every file (`tmpfile` + `fsync` +
`rename`); user content in `settings.json` and `CLAUDE.md`
preserved verbatim around the marker block; idempotent — re-run
produces a byte-identical state. `--upgrade` overwrites the
mneme-owned files with the new binary's content; `--uninstall`
reverses every change atomically; `--show` previews the plan
without writing.

Fully wired today: **Claude Code** (reference implementation),
**Claude Desktop** (no hooks API or instruction file — just the
MCP entry + a paste-able MNEME.md copy). Cursor, Cline, Codex,
and Gemini CLI return `NotYetImplemented` with a tracked task
pointer until each integration is validated end-to-end on a real
install.

The post-install message walks you through a 2-minute
verification: a Vim-keybindings remember/recall conversation that
demonstrates persistence across sessions.

### Daemon mode + Unix socket transport

**Before (v1.0):** every Claude Code session spawned a fresh
`mneme run` subprocess that paid the BGE-M3 cold-load + redb open
+ HNSW snapshot replay every time. Two simultaneous Claude Code
sessions tripped the exclusive lockfile and the second exited
with "lock held."

**After (v1.1):**

```sh
mneme daemon
```

One long-lived process accepts multiple MCP clients concurrently
over the Unix domain socket at `~/.mneme/run/mneme.sock`. Storage
writes serialise through the existing single-writer seam (no new
concurrency primitives — ADR-0012 D8). Per-platform listener: Unix
socket on macOS / Linux today; Windows named-pipe support
(`\\.\pipe\mneme-{user_sid}`) lands in v1.1 alongside auth M4.

Lifecycle:
- Boot binds the socket (with stale-cleanup probe — orphaned
  socket files from a crashed daemon are detected via
  `connect()` and unlinked automatically).
- Idle timeout configurable via `[daemon] idle_timeout_minutes`
  (default 30; `0` disables). Counted from "last client
  disconnected" — long HNSW snapshots that block requests but
  not connections don't trigger spurious shutdowns.
- SIGTERM / Ctrl-C drains in-flight client connections (up to
  30 seconds) before the runtime tears them down.

`mneme run` (without `--stdio`) is the v1.0-compatible stdio
shim — Claude Code's existing `mcpServers` entry keeps working;
`mneme init claude-code` writes that exact entry. The
spawn-and-connect wrapper that has `mneme run` transparently
reuse a running daemon ships in a follow-up commit.

### Per-installation auth token

**The token value lives in exactly one file:**
`~/.mneme/run/auth.token` (mode `0600`). Agent configs reference
the path, never the value — so the blast radius of a compromised
agent config doesn't include the token. `mneme auth rotate`
regenerates the token atomically; existing daemon connections
stay valid (the token check fires only at handshake), the next
new connection picks up the rotated value automatically.

Every connection presents the token as the first line of the
handshake (`MNEME-AUTH: <token>\n`); the daemon reads from disk
on every handshake (no in-memory caching that would defeat
rotation — caught + fixed during A.M5 testing).

### Content size guardrails

`remember` and `update` enforce three tiers:
- **< 500 chars** — stored silently.
- **500–2,000 chars** — stored, response includes a
  `length_advisory` field.
- **2,000–10,000 chars** — stored, response includes a stronger
  `length_warning` field; logged at `info` level.
- **> 10,000 chars** — rejected with a structured error
  (`memory_too_large`) + suggestion to extract the key insight
  or summarize. Embedding is never performed for rejected content.

The 10,000-char ceiling is configurable via
`[budgets] max_remember_chars`. Existing oversized memories
(written under v1.0 which had no limit) remain readable +
`recall`-able — the verbatim principle is preserved; only NEW
writes/updates are gated.

`mneme://stats.memories.large_memory_count` reports the per-tier
distribution of your existing corpus + the IDs of any memories
above the ceiling — useful for trimming after the upgrade.

### First-boot upgrade audit

On v1.1's first boot against an existing data directory, the
binary scans L4 once for memories above the new ceiling and
writes a passive summary to `~/.mneme/diagnostics.log`. **Existing
memories are never auto-modified** — you find what's there and
decide what to do.

Gated by `~/.mneme/run/upgrade-audit.done` so it runs at most
once per data directory; delete the marker to force a re-audit
(useful after `mneme.forget`-ing oversized entries).

### `tools/call` responses gain `_meta` annotations

The MCP `_meta` field on tools/call responses now carries
mneme-specific structured annotations:
- `length_advisory` / `length_warning` on `remember` and `update`
- `error.code = "memory_too_large"` on the rejection path

Existing MCP clients that don't read `_meta` keep working — the
text content channel still carries human-readable equivalents.

---

## Operational changes

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
anyone whose backup workflow doesn't include rolling back to
v1.0 against a v1.1-populated data dir.

### Migration test as release gate

`tests/v1_0_to_v1_1_migration.rs` pins three things that v1.1.0
cannot ship without:

1. A v1.0-shaped `config.toml` parses cleanly under the v1.1
   binary (serde defaults backfill `[daemon]` and
   `max_remember_chars`).
2. Memories written under a v1.0-shaped config persist + recall
   across reboot (catches any accidental schema_version bump).
3. Explicit `[mcp].transport = "stdio"` is preserved on boot.

The sibling rollback test (`tests/v1_1_to_v1_0_rollback.rs`)
ships once the v1.0.1 backup-run-exclusion patch lands in a
released v1.0.1 binary that the rollback CI can boot against.

---

## Upgrading from v1.0

For most users, no action required:

```sh
brew upgrade mneme
mneme --version            # confirm v1.1.0
```

Re-running `mneme run` against your existing `~/.mneme/` boots
clean — no schema migration (per ADR-0012 Invariant 1, v1.1 does
not bump `schema_version`). Your existing `mcpServers.mneme`
config in Claude Code's `settings.json` keeps working unchanged.

To take advantage of `mneme init claude-code`'s automatic hook
installation (you previously copied them by hand from
`docs/examples/`):

```sh
mneme init claude-code --upgrade
```

That writes the canonical hook scripts + `MNEME.md` + marker
block. `--show` first if you want to see what would change.

To surface any oversized memories from your v1.0 corpus:

```sh
cat ~/.mneme/diagnostics.log
```

The first-boot audit ran the size-tier scan automatically; the
log shows per-tier counts + the IDs of any memories above the
new ceiling.

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
`~/.mneme/run/` is silently ignored by v1.0.1 (sockets get
unlinked next time a v1.1.x runs; auth.token is untouched).

---

## What's NOT in v1.1

Per the v2.1 planning doc, deliberately out of scope:

- **SSE event-stream framing** for the daemon transport. v1.1
  uses newline-delimited JSON over the Unix socket (same wire
  format as stdio); SSE comment-frame keepalive (ADR-0012 D7) is
  deferred to v1.1.x or v1.2 alongside the framing switch.
- **Hybrid search (BM25 + dense)** and **encryption at rest** —
  Tier-2 features deferred to v1.2.
- **OpenCode integration** — Tier-2 conditional. Ships in v1.1
  only if its plugin API stabilises by mid-July; otherwise
  defers to v1.1.x.
- **Standalone `mneme demo` walkthrough command** — the
  post-install prompt covers the 5-minute first-run experience;
  the standalone command is the explicit cut candidate per the
  pre-committed cut order.

See ADR-0012 amendment A1 for the full keepalive deferral
rationale, and release-planning v2.1 §2.6 for the cut order.
