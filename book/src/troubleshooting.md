# Troubleshooting

The `claude mcp list` says `mneme: failed to start`, the first call
takes 30+ seconds, `recall` returns nothing — common rough edges and
their fixes.

## `claude mcp list` shows `mneme: failed to start`

Run the binary directly to see stderr:

```sh
# v1.1 daemon-mode install (the default after `mneme init claude-code`):
mneme daemon                    # starts the long-lived MCP server; Ctrl-C to stop
mneme client </dev/null         # in another shell, exercises the bridge end-to-end

# Single-host fallback install (args=["run"] in mcpServers):
mneme run </dev/null
```

Anything fatal will print before the process exits. Also tail
`~/.mneme/logs/mneme.log`.

## `~/.mneme/config.toml` is missing — what does that mean?

`Config::load` (`src/config.rs::load`) silently returns
`Config::default()` when the file is absent. The MCP server boots
on all-default values without a warning — easy to miss after you
rename or delete the file. Fix:

```sh
mneme init                                  # writes a fresh defaults file
diff ~/.mneme/config.toml ~/.mneme/config.toml.bak  # if you kept a backup
mneme stop                                  # restart so the new file lands
```

(A `WARN` log line at load time when the file is missing is queued
for a future v1.1.x patch — see the related observation in mneme.)

## First tool call takes 30+ seconds, then succeeds

The embedding model is downloading (~1.5 GB for `bge-m3`, ~80 MB for
`minilm-l6`). Subsequent starts reuse `~/.mneme/models/`. Tail
`~/.mneme/logs/mneme.log` to confirm download progress.

## "Another mneme is running" / lockfile error

`~/.mneme/.lock` is held by a live process. Find it
(`pgrep -af "mneme (daemon|run)"`) and shut it down with `mneme stop`.
Only delete `.lock` manually if you've confirmed no process holds it
— a stale lockfile is rare and usually means a previous crash.

(`mneme client` does NOT take the lockfile — it's just a stdio↔socket
bridge. The lock is held by whichever MCP server is serving the data
dir: `mneme daemon` for the v1.1 default flow, `mneme run` for the
single-host fallback.)

## `/mcp` doesn't list mneme

MCP servers are loaded once per Claude Code session. Quit Claude Code
fully and relaunch. If it's still missing, run `claude mcp list` from
the same directory and the same shell — the user/project/local scope
distinction can bite if you registered at one scope and launched at
another.

## macOS Gatekeeper blocks the binary ("cannot be opened")

```sh
xattr -d com.apple.quarantine /path/to/mneme
```

This applies to any binary you didn't build yourself. If you built
locally with `cargo build --release`, this shouldn't trigger.

## Tool calls succeed but `recall` returns nothing

`recall` is semantic, not lexical — it returns nothing when the query
embedding is far from any stored memory. This is not an error. Try a
broader query, or `export` to confirm what's actually stored, or
`recall_recent` to see what's in the episodic log.

## Embedding model fails to download (HF 404)

The default `bge-m3` is the BAAI release; some mirrors don't ship
`model.safetensors`. Switch to `minilm-l6` in `~/.mneme/config.toml`,
delete `~/.mneme/models/`, and restart. The fallback path
(`pytorch_model.bin`) is also wired but can occasionally trip if HF
returns a partial response — a clean re-download usually resolves it.

## Backup says EISDIR with `--include-models`

Fixed in current builds (the walker now uses `symlink_metadata` so
symlink-to-dir entries land in the archive as symlinks). If you see
this on a current build, the symlink chain in `~/.mneme/models/` is
unusual; report with the full path listing
(`ls -la ~/.mneme/models/`).

## Where did `~/.mneme/diagnostics.log` come from?

v1.1's first boot against an existing data directory runs a one-shot
upgrade audit (release-planning v2.1 §5.3). It scans L4 once for
memories above `[budgets].max_remember_chars` (default 10,000) and
appends a passive summary to `~/.mneme/diagnostics.log`. Format:

```
2026-05-09T12:34:56+00:00 v1.1 upgrade audit | max_remember_chars=10000 | total=1234 normal=1180 advisory=45 warning=8 over_limit=1
  over-limit memory IDs (use `mneme inspect <id>` to view; `mneme.forget` to remove if desired):
    01HABC...
  Existing oversized memories remain readable + recall-able (verbatim principle preserved). Only NEW writes/updates above 10000 chars are rejected by the `remember`/`update` tools.
```

Gated by `~/.mneme/run/upgrade-audit.done` so it runs at most once
per data directory. **Existing oversized memories are NEVER
auto-modified** — they remain `recall`-able. Only new writes/updates
above the limit are rejected.

To re-run the audit (e.g. after `mneme.forget`-ing oversized
entries) delete the marker:

```sh
rm ~/.mneme/run/upgrade-audit.done
```

The next MCP server boot (`mneme daemon` or `mneme run`) will scan
again and append a fresh entry.

## Diagnosing scheduler health

`mneme://stats` (or `mneme stats`) surfaces:

- `consolidation.last_consolidation_at` — null until the first idle pass
- `consolidation.runs_total` — successful pass count since boot
- `working.session_id` — active session id; matches `mneme://session/{id}`
- `working.checkpoints_total` — successful flush count

If `consolidation.runs_total` is `0` after several hours, the system
has never been quiet long enough for the idle gate to close.

---

# v1.1 daemon mode

The next sections cover failure modes specific to v1.1's
[daemon mode](./release-notes-v1_1.md). v1.0 stdio installs
(`mcpServers.mneme = {"command": "mneme", "args": ["run"]}` —
which is what `mneme init claude-code` writes today) are
unaffected.

## `mneme daemon` exits immediately with "another mneme daemon is already serving"

A previous daemon is alive. The stale-cleanup probe in
`daemon::listener::bind_listener` connect()-tested the existing
socket, got a successful connect, and refused to stomp on it.

```sh
pgrep -af "mneme daemon"     # find the live PID
mneme stop                    # graceful SIGTERM via the lockfile
```

If `mneme stop` reports no live process but the bind still
fails, the lockfile is held by something else — see "Another
mneme is running" / lockfile error above.

## `mneme daemon` exits with "stale socket file detected"

The previous daemon crashed (kernel reaped its fd before the
RAII unlink ran). `bind_listener`'s stale-cleanup probe handles
this automatically — `connect()` returns `ECONNREFUSED`, the
file is unlinked, the new daemon binds. If you see this error
anyway, the file may be a non-socket entry (e.g. a regular file
left by a restored backup). Inspect:

```sh
ls -la ~/.mneme/run/mneme.sock
```

If it's not a socket (`s` first character), delete it
manually — the cleanup probe's "ENOTSOCK fast path" handles
this case automatically in v1.1.0+, but if you're on a pre-fix
build you may need:

```sh
rm ~/.mneme/run/mneme.sock
mneme daemon                  # re-bind cleanly
```

## Daemon idle-shutdown: "no clients for 30 minutes"

By design (ADR-0012 D6 / `[daemon] idle_timeout_minutes`).
Either:

- Reconnect — the next client connection respawns it via the
  spawn-and-connect flow (once D12 lands; today
  `mneme run` is the explicit re-spawn path).
- Disable: set `[daemon] idle_timeout_minutes = 0` in
  `~/.mneme/config.toml`. Daemon then only stops via
  `mneme stop` / SIGTERM.
- Extend: set the value to a higher number of minutes.

The shutdown is logged at `info` level so you can see it in
`~/.mneme/logs/mneme.log`:

```
INFO transport=daemon-serve-many: idle timeout reached; shutting down
```

## Daemon shutdown takes 30 seconds on SIGTERM

In-flight client connections are draining. `wait_for_drain`
gives them up to 30 seconds to finish their MCP exchanges
before the runtime aborts the spawned tasks. To skip the
drain (force immediate exit), send SIGKILL instead — but
expect any in-flight `remember`/`update` calls to lose their
write.

## Auth handshake errors

Every connection to a v1.1 daemon must present
`MNEME-AUTH: <token>\n` as its first line, where `<token>` is
the contents of `~/.mneme/run/auth.token`. The daemon emits
distinct rejection types — find them in `~/.mneme/logs/mneme.log`
or on stderr:

| Daemon log line | What it means |
|---|---|
| `auth handshake rejected: client disconnected before completing auth handshake` | Client closed without sending anything. Often port-scan / health-probe traffic. |
| `auth handshake rejected: auth line exceeded 256 bytes without a newline` | Malformed protocol or attempted resource exhaustion. |
| `auth handshake rejected: auth handshake timed out after 5s` | Slow / silent peer; daemon dropped the connection. |
| `auth handshake rejected: auth line missing 'MNEME-AUTH: ' prefix` | Client sent something but not the auth header. Likely an MCP client that doesn't know the daemon protocol. |
| `auth handshake rejected: auth token does not match the daemon's expected value` | Client presented a token that doesn't match `~/.mneme/run/auth.token`. Probably a stale token from before `mneme auth rotate`. |

For the `TokenMismatch` case specifically, the fix is to make
sure the client is reading the current token from the file
(not a cached value from before the rotation). Existing
in-flight connections stay valid through rotation — the check
only runs on new connections.

## `mneme auth rotate` semantics

```sh
mneme auth rotate             # generates a fresh token at ~/.mneme/run/auth.token
mneme auth show-path          # prints the path agents reference
```

The token value lives in **exactly one file**. Agents that
reference it by path (the canonical pattern — `mneme init`
writes the right shape) keep working without reconfiguration.

After rotation:

- Existing daemon connections stay valid (the check only fires
  at handshake; in-progress sessions aren't dropped).
- New connections must read the rotated token from disk on
  their next handshake. If a client cached the old value, it
  will fail with `TokenMismatch` until it re-reads.

## Removing or recreating the auth token

The daemon auto-generates `~/.mneme/run/auth.token` on first
boot if it doesn't exist (mode 0600). To force a fresh token:

```sh
rm ~/.mneme/run/auth.token
mneme stop                    # graceful exit + free the socket
mneme daemon                  # next boot writes a new token
```

Equivalent to `mneme auth rotate` on a stopped daemon.

---

# v1.1 `mneme init` issues

## "config already present at ~/.mneme/config.toml" — but I want a fresh start

`mneme init` (no agent, the v1.0 scaffold-only mode) refuses
to overwrite an existing `config.toml`. Either edit it
manually, or:

```sh
mv ~/.mneme/config.toml ~/.mneme/config.toml.backup
mneme init                    # writes a fresh default config
```

Per-agent installers (`mneme init claude-code`,
`mneme init claude-desktop`) are different — they DO overwrite
the mneme-owned files (`MNEME.md`, marker block, hook scripts).
Use `--upgrade` for the explicit semantics:

```sh
mneme init claude-code --upgrade
```

## "Claude Code doesn't see the new MCP server after install"

Claude Code reads `mcpServers` only at startup. **Quit and
relaunch** Claude Code after `mneme init claude-code`.

Verify the entry landed:

```sh
jq .mcpServers.mneme ~/.claude/settings.json
# expect: {"command": "mneme", "args": ["run"]}
```

Verify the binary is on Claude Code's PATH (matters when
Claude Code is launched from the Dock / Spotlight rather than
your shell):

```sh
which mneme                   # in the same shell that launches Claude Code
```

If `which` finds it but Claude Code doesn't, the binary
isn't on Claude Code's launch-time PATH. Either install to
`/usr/local/bin/` (which Claude Code does pick up), or use
the absolute path in `settings.json`:

```sh
mneme init claude-code --uninstall
# edit settings.json to use absolute path:
#   "command": "/Users/<you>/.local/bin/mneme"
```

(`mneme init claude-code --upgrade` will preserve a
hand-edited `command` field if it's not exactly `"mneme"`,
but the safest path is to put `mneme` on the launch PATH.)

## SessionStart hook doesn't fire after install

Claude Code reads the `hooks` block at startup too. Restart
Claude Code first. If the nudge still doesn't appear:

```sh
ls -la ~/.claude/hooks/mneme/
# expect: 3 *.sh files, mode 0755
bash ~/.claude/hooks/mneme/session-start.sh
# expect: the SessionStart nudge text on stdout
```

If the script doesn't exist or isn't executable, re-run the
install with `--upgrade`:

```sh
mneme init claude-code --upgrade
```

If the script runs but Claude Code doesn't surface its
output, check `~/.claude/settings.json` for the `hooks`
block — `mneme init` writes the canonical Claude Code format
(`{"hooks": [{"type": "command", "command": "..."}]}`).

## `mneme init claude-code --uninstall` left files behind

`--uninstall` removes mneme-owned files but preserves user
content. Specifically NOT removed:
- Other `mcpServers` entries in `settings.json` (preserved)
- User content in `CLAUDE.md` outside the marker block
  (preserved — the file stays unless the marker block was
  the only content)
- Other hook events in `settings.json` (preserved)

If you see leftovers, they're either user-owned (preserved by
design) or they're Claude Code's auto-generated fallback (e.g.
empty `{}` settings.json — Claude Code writes that on its own).

---

# v1.1 → v1.0 rollback

The hard promise: rolling back from v1.1 to v1.0 doesn't lose
data. Per ADR-0012:

- v1.1 doesn't bump on-disk `schema_version` (Invariant 1).
- v1.0.1 (a small patch released alongside v1.1) tolerates the
  v1.1-managed `~/.mneme/run/` directory cleanly.

If you're on a pre-v1.0.1 v1.0 binary and you tried to
`mneme backup` against a v1.1-populated data dir, you'd see
this warning:

```
WARN skipping unsupported file type during backup walk path=~/.mneme/run/mneme.sock
```

The `auth.token` would also leak into the backup tarball.
Both are fixed in v1.0.1+. To roll back safely:

```sh
brew install mneme@1.0.1      # pinning the patch release
mneme stop                     # if a v1.1 daemon was running
brew unlink mneme && brew link mneme@1.0.1
mneme stats                    # confirm v1.0.1 boots clean
```

Memories carry forward identically. Subsequent v1.0.1 runs
see only the data v1.0 understands; the v1.1 `~/.mneme/run/`
state is silently ignored.
