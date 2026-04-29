# Troubleshooting

The `claude mcp list` says `mneme: failed to start`, the first call
takes 30+ seconds, `recall` returns nothing — common rough edges and
their fixes.

## `claude mcp list` shows `mneme: failed to start`

Run the binary directly to see stderr:

```sh
mneme run </dev/null
```

Anything fatal will print before the process exits. Also tail
`~/.mneme/logs/mneme.log`.

## First tool call takes 30+ seconds, then succeeds

The embedding model is downloading (~1.5 GB for `bge-m3`, ~80 MB for
`minilm-l6`). Subsequent starts reuse `~/.mneme/models/`. Tail
`~/.mneme/logs/mneme.log` to confirm download progress.

## "Another mneme is running" / lockfile error

`~/.mneme/.lock` is held by a live process. Find it (`pgrep -f "mneme run"`)
and shut it down with `mneme stop`. Only delete `.lock` manually if
you've confirmed no process holds it — a stale lockfile is rare and
usually means a previous crash.

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

Fixed since v0.10. If you see this on a current build, the symlink
chain in `~/.mneme/models/` is unusual; report with the full path
listing (`ls -la ~/.mneme/models/`).

## Diagnosing scheduler health

`mneme://stats` (or `mneme stats`) surfaces:

- `consolidation.last_consolidation_at` — null until the first idle pass
- `consolidation.runs_total` — successful pass count since boot
- `working.session_id` — active session id; matches `mneme://session/{id}`
- `working.checkpoints_total` — successful flush count

If `consolidation.runs_total` is `0` after several hours, the system
has never been quiet long enough for the idle gate to close.
