# Setting up Mneme with Claude Code

This guide walks you from a stock Claude Code install to a session where
the agent can call `remember`, `recall`, and the rest of mneme's 12 tools
— including verification, project-vs-personal scoping, and troubleshooting.

If you only want the three lines: build the binary, run `mneme init`, then
`claude mcp add --scope user mneme /absolute/path/to/mneme run`. Restart
Claude Code, type `/mcp`, and you should see mneme listed.

---

## 1. Prerequisites

You need:

- **Claude Code** installed and authenticated. Check with `claude --version`
  and `claude mcp list`.
- **Rust toolchain** (stable). `cargo --version` should work; otherwise
  install via [rustup](https://rustup.rs).
- **Mneme built locally**:

  ```sh
  git clone https://github.com/vserkin/mneme && cd mneme
  cargo build --release
  ```

  The binary lands at `target/release/mneme`. Either copy it onto `$PATH`
  (e.g. `cp target/release/mneme ~/.local/bin/`) or remember the absolute
  path — you'll need it in step 3.

- **One-time scaffold**:

  ```sh
  mneme init
  ```

  This creates `~/.mneme/` with `config.toml`, the schema-version stamp,
  and the directory layout. It does **not** download the embedding model
  yet — that happens on the first `mneme run`.

  > **Embedding model size.** The default model is `bge-m3` (~1.5 GB,
  > 1024-dim, multilingual, top-tier recall). If you want a smaller model
  > with faster startup, edit `~/.mneme/config.toml` before the first run
  > and set `[embeddings] model = "minilm-l6"` (~80 MB, 384-dim, English).
  > You can switch later, but each switch reindexes from scratch.

- **`jq`** (only for the smoke test below): `brew install jq`.

Verify the binary is healthy:

```sh
mneme --help
mneme stats   # prints zeros; confirms the data dir is intact
```

---

## 2. Choose a configuration scope

Claude Code stores MCP server registrations at three scopes. Pick the one
that matches how you want mneme available.

| Scope | Where it lives | Visible in | Use when |
|-------|----------------|-----------|----------|
| **user** | `~/.claude.json` (per-user, all projects) | Every Claude Code session you start, anywhere on disk | You want one personal memory across every repo. **Recommended for most people.** |
| **project** | `<repo>/.mcp.json` (committed to the repo) | Anyone running Claude Code in this repo who has the binary | You want collaborators to pick mneme up automatically when they `cd` into the repo. The binary still has to be installed locally. |
| **local** | `~/.claude.json` (per-user, scoped to one project path) | Only Claude Code sessions started inside this project | Trying mneme out without committing anything. Default if you don't pass `--scope`. |

Most users want **user scope**. Skip to step 3a.

---

## 3a. User scope (recommended)

```sh
# Replace the path with the absolute location of your mneme binary.
# If `mneme` is on $PATH, the literal string "mneme" works too.
claude mcp add --scope user mneme "$(command -v mneme)" run
```

Verify:

```sh
$ claude mcp list
mneme: connected
```

The registration is stored under `~/.claude.json` and will be loaded by
every Claude Code session you start from now on.

## 3b. Project scope

From inside the repo where you want mneme available:

```sh
claude mcp add --scope project mneme mneme run
```

This writes (or updates) `.mcp.json` at the repo root. Commit it if you
want collaborators to pick it up. The file looks like:

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

> **Important:** project scope assumes the binary is on `$PATH` for everyone
> who opens the repo. If you'd rather pin a specific build, replace
> `"command": "mneme"` with an absolute path — but absolute paths don't
> port across machines, so you'd want a workspace-relative scheme (e.g. a
> `bin/mneme` symlink committed to the repo).

## 3c. Local scope (try before you commit)

```sh
claude mcp add mneme mneme run     # no --scope flag = local
```

Same registration mechanism as user scope, but only active when Claude
Code is launched from this project directory. Good for kicking the tires
without touching `~/.claude.json` globally.

---

## 4. Verify Claude Code can see mneme

Open Claude Code in a directory that should pick up the registration
(anywhere for user scope, the project root for project/local scope).

Inside the session, run the slash command:

```
/mcp
```

You should see mneme listed with **12 tools** and **5 resources**:

- Tools: `remember`, `recall`, `update`, `forget`, `pin`, `unpin`,
  `recall_recent`, `summarize_session`, `stats`, `list_scopes`, `export`,
  `switch_scope`.
- Resources: `mneme://stats`, `mneme://procedural`, `mneme://recent`,
  `mneme://context`, `mneme://session/{id}`.

Quick functional check inside the session:

```
Use the mneme remember tool to store: "claude code mneme integration
verified at <today's date>".
```

Claude should call the `remember` tool and get back a ULID. Then:

```
Use mneme recall to find: "claude code integration"
```

Claude should call `recall` and surface the memory you just stored.

If the first call hangs for 10–30 seconds, the embedding model is
downloading. Subsequent calls are sub-50 ms.

---

## 5. Project-scoped memories

Mneme's data directory (`~/.mneme/` by default) is a single palace —
every memory lives there regardless of which project the agent is
working in. That's usually what you want; recall is semantic, so the
right context surfaces wherever it's relevant.

When you do want hard isolation between projects, you have two patterns:

### Pattern A — separate data directories per project

Set `MNEME_DATA_DIR` per project (via `direnv`'s `.envrc`, your shell
rc, or a wrapper script). Each project gets its own `~/.mneme-<project>`
with its own embeddings, episodic log, and procedural pins. Backups are
also per-project.

```sh
# In <repo>/.envrc, with direnv installed and allowed:
export MNEME_DATA_DIR="$HOME/.mneme-myrepo"
```

You'll need to run `mneme init` once inside each project after setting
`MNEME_DATA_DIR`.

### Pattern B — one palace, multiple scopes

Keep the single `~/.mneme/` palace and pass `scope` in tool calls:

```
Use remember with content "<fact>", scope "myrepo".
```

`recall` and `forget` accept the same `scope` argument. Use
`list_scopes` to see what buckets exist. The default scope is set in
`~/.mneme/config.toml` under `[scopes] default = "personal"`.

Pattern B is simpler and lets the agent cross-reference memories
across scopes when useful. Pattern A is right when you have a hard
confidentiality boundary (e.g. work vs. personal vs. client work).

---

## 6. Patterns that pay off

These are the patterns that make mneme worth running. Add them to your
`CLAUDE.md` (project or `~/.claude/CLAUDE.md`) so the agent uses mneme
proactively rather than only when you ask:

```markdown
## Memory (mneme)

- On session start, read `mneme://context` to see pinned procedural
  rules and recent events. Do this before answering anything that
  references "earlier" or "last time".
- When the user shares a fact, decision, or preference that should
  outlive the session, call `remember` with an appropriate `type`
  (`fact`, `decision`, `preference`, `conversation`) and a `scope`
  if the project has its own.
- When the user states a hard rule ("always use uv", "we never
  commit to main"), call `pin` instead of `remember` — pins surface
  on every subsequent context assembly.
- At the end of a long working session, call `summarize_session`
  with the session id and feed the prompt template into your own
  completion path.
- Never invoke `forget` without confirming with the user first;
  deletion is permanent.
```

Concrete moments to use each tool:

- **`remember`** — user shares a fact, makes a decision, expresses a
  preference. Default for "remember this".
- **`pin`** — hard rule that should surface every session. Use sparingly;
  the procedural feed is small on purpose.
- **`recall`** — semantic search. The right tool when you need context
  the user previously shared but isn't in the current conversation.
- **`recall_recent`** — "what did we just do?" / "what was the last
  thing on this branch?" Episodic, time-ordered, not semantic.
- **`update`** — user revises a fact. Re-embeds if `content` changes.
- **`stats`** / **`list_scopes`** — diagnose / orient. Cheap.
- **`export`** — read-only dump for grepping or piping to `jq`.

---

## 7. Lifecycle hooks (recommended)

Claude Code can run shell hooks at well-defined moments — session
start, before context compaction, end of every turn. Mneme uses
these as a deterministic load-and-save loop:

- **`SessionStart`** nudges the agent to read `mneme://procedural`
  and `mneme://context` on turn 1, instead of relying on the agent
  to remember to consult mneme.
- **`PreCompact`** nudges the agent to consolidate via
  `summarize_session` / `remember` / `pin` *before* Claude Code
  compresses the conversation — anything not in mneme by then is
  gone.
- **`Stop`** nudges the agent to persist any fact / decision /
  rule established this turn. Permissive by design: most turns
  produce nothing worth saving.

The hooks themselves are tiny shell scripts that emit a nudge on
stdout; Claude Code surfaces that as additional context, and the
agent makes the actual mneme tool calls. The hooks don't talk to
the running `mneme run` process directly — that would conflict
with the exclusive lockfile. A direct hook-to-mneme control
channel is on the v1.1 roadmap.

### 7.1 Install the example scripts

```sh
# From the repo root, copy the three scripts to a stable location
# under your home directory:
mkdir -p ~/.claude/hooks/mneme
cp docs/examples/claude-code-hooks/session-start.sh \
   docs/examples/claude-code-hooks/precompact.sh \
   docs/examples/claude-code-hooks/stop.sh \
   ~/.claude/hooks/mneme/
chmod +x ~/.claude/hooks/mneme/*.sh
```

### 7.2 Wire them into Claude Code

Add the following block to your Claude Code settings (typically
`~/.claude/settings.json` for user scope, or `<repo>/.claude/
settings.json` for project scope). Merge with existing `hooks`
keys if you have any.

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "~/.claude/hooks/mneme/session-start.sh" }
        ]
      }
    ],
    "PreCompact": [
      {
        "hooks": [
          { "type": "command", "command": "~/.claude/hooks/mneme/precompact.sh" }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "~/.claude/hooks/mneme/stop.sh" }
        ]
      }
    ]
  }
}
```

Restart Claude Code. The next session should start with the
SessionStart nudge in the trace, and the agent should call
`mneme://procedural` and `mneme://context` before answering.

### 7.3 Per-hook opt-out

Each hook is independent. Common patterns:

- **Read-only setup.** Keep `SessionStart`, drop `PreCompact` and
  `Stop`. The agent reads but never writes through hooks.
  Useful when you populate mneme manually.
- **Save-only setup.** Keep `PreCompact` and `Stop`, drop
  `SessionStart`. The agent persists but doesn't auto-consult.
  Useful if you don't want the SessionStart read latency on
  every fresh shell.
- **All three.** Recommended — it's the full load-and-save loop.

To disable a hook, remove its entry from the `hooks` block (or
delete the script from `~/.claude/hooks/mneme/` and Claude Code
will warn but skip it).

### 7.4 Verifying

After adding the hooks:

1. **SessionStart smoke.** Pin a sentinel rule:

   ```
   Use mneme.pin to add: "TEST: every response must end with the
   word fnord".
   ```

   Quit Claude Code, reopen, and ask any question. The first
   response should end with "fnord". Unpin afterwards.

2. **PreCompact smoke.** Start a session, do enough work to
   trigger compaction (or run `/compact` explicitly). Confirm
   the agent calls `summarize_session` (and optionally
   `remember` / `pin`) before the compaction summary lands.
   Inspect with `mneme inspect --query <sentinel>`.

3. **Stop smoke.** End a turn with explicit "remember that we use
   `uv`, not `pip`". Confirm a `remember` call lands. End a
   different turn with a purely transient question. Confirm the
   agent does *not* call `remember`.

### 7.5 Decoupling

Removing the hooks from `settings.json` (and the scripts from
`~/.claude/hooks/mneme/`) leaves mneme behaving identically — no
hidden coupling. Mneme works without the hooks; the hooks just
make the load-and-save loop deterministic.

---

## 8. Troubleshooting

### `claude mcp list` shows `mneme: failed to start`

Run the binary directly to see stderr:

```sh
mneme run </dev/null
```

Anything fatal will print before the process exits. Also tail
`~/.mneme/logs/mneme.log`.

### First tool call takes 30+ seconds, then succeeds

The embedding model is downloading (~1.5 GB for `bge-m3`, ~80 MB for
`minilm-l6`). Subsequent starts reuse `~/.mneme/models/`. Tail
`~/.mneme/logs/mneme.log` to confirm download progress.

### "Another mneme is running" / lockfile error

`~/.mneme/.lock` is held by a live process. Find it (`pgrep -f "mneme run"`)
and shut it down with `mneme stop`. Only delete `.lock` manually if
you've confirmed no process holds it — a stale lockfile is rare and
usually means a previous crash.

### `/mcp` doesn't list mneme

MCP servers are loaded once per Claude Code session. Quit Claude Code
fully and relaunch. If it's still missing, run `claude mcp list` from
the same directory and the same shell — the user/project/local scope
distinction can bite if you registered at one scope and launched at
another.

### macOS Gatekeeper blocks the binary ("cannot be opened")

```sh
xattr -d com.apple.quarantine /path/to/mneme
```

This applies to any binary you didn't build yourself. If you built
locally with `cargo build --release`, this shouldn't trigger.

### Tool calls succeed but `recall` returns nothing

`recall` is semantic, not lexical — it returns nothing when the query
embedding is far from any stored memory. This is not an error. Try a
broader query, or `export` to confirm what's actually stored, or
`recall_recent` to see what's in the episodic log.

### Embedding model fails to download (HF 404)

The default `bge-m3` is the BAAI release; some mirrors don't ship
`model.safetensors`. Switch to `minilm-l6` in `~/.mneme/config.toml`,
delete `~/.mneme/models/`, and restart.

---

## 9. Backup and restore

Mneme writes a single tar.gz snapshot via:

```sh
mneme backup ~/Backups/mneme-$(date +%Y%m%d).tar.gz
```

The model cache is excluded by default (re-downloadable). Pass
`--include-models` if you want a self-contained archive; symlinks
are preserved as-is rather than followed, so a symlinked
`~/.mneme/models/` won't trip an `EISDIR` during the walk.

To restore:

```sh
mneme restore ~/Backups/mneme-20260415.tar.gz   # refuses to overwrite
mneme restore --force <archive>                  # explicit overwrite
```

Restore is atomic (writes to a temp dir, renames in). The lockfile is
checked first; restore refuses if a server is running.

---

## 10. Uninstall

```sh
# Remove the registration from Claude Code at the scope you used:
claude mcp remove mneme --scope user
claude mcp remove mneme --scope project   # or
claude mcp remove mneme                   # local

# Optional: wipe the data directory.
mneme stop                  # if a server is running
rm -rf ~/.mneme

# Optional: remove the binary.
rm -f ~/.local/bin/mneme
```

`~/.mneme/` is the only state on disk; once it's gone, mneme leaves no
trace.
