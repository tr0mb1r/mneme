# Recommended Claude Code config

Drop-in templates for an mneme-aware Claude Code setup. Copy any or
all into your own config; tune from there.

The walkthrough in [Setting up with Claude Code](./claude-code-setup.md)
is the prose explanation of *how* and *why*. This page is the
copy-paste kit. Both stay in sync — the hook scripts and settings
snippet referenced here are the same files that ship in the repo
under `docs/examples/`, included verbatim below.

## 1. `CLAUDE.md` template

Drop this into `~/.claude/CLAUDE.md` (user-scope, applies everywhere)
or `<repo>/CLAUDE.md` (project-scope, only when Claude Code runs in
that repo). Both work; pick the scope that matches how you want
mneme to participate.

```markdown
{{#include ../../docs/examples/claude-code-config/CLAUDE.md}}
```

What it does:

- Describes the four memory layers so the agent picks the right tool
  for the right kind of write.
- Documents the read-on-session-start protocol so the agent doesn't
  forget to consult mneme before answering.
- Locks down `forget` behind explicit user confirmation — deletion
  is permanent.

## 2. `settings.json` template

Drop this into `~/.claude/settings.json` (user-scope) or
`<repo>/.claude/settings.json` (project-scope). If you already have a
`settings.json`, merge each top-level key (`mcpServers`, `hooks`,
`permissions`) rather than overwriting the file.

```json
{{#include ../../docs/examples/claude-code-config/settings.json}}
```

Three blocks, each independent:

### `mcpServers`

Registers mneme as an MCP server. Equivalent to running
`claude mcp add --scope user mneme mneme run` — pick whichever feels
more natural. The CLI form is what the [setup guide](./claude-code-setup.md#3a-user-scope-recommended)
walks you through; this JSON form is what `claude mcp add` writes
under the hood.

### `hooks`

Wires the three lifecycle scripts (`SessionStart`, `PreCompact`,
`Stop`) into Claude Code so the load-and-save loop is deterministic
rather than depending on the agent to remember to consult mneme.

The script files themselves ship under
`docs/examples/claude-code-hooks/` in the upstream repo. Copy them
to `~/.claude/hooks/mneme/` and `chmod +x`:

```sh
mkdir -p ~/.claude/hooks/mneme
cp docs/examples/claude-code-hooks/session-start.sh \
   docs/examples/claude-code-hooks/precompact.sh \
   docs/examples/claude-code-hooks/stop.sh \
   ~/.claude/hooks/mneme/
chmod +x ~/.claude/hooks/mneme/*.sh
```

The hooks emit nudges on stdout; Claude Code surfaces them as
additional context, and the agent makes the actual `mneme.*` tool
calls. Each hook is independent — drop any of the three from the
`hooks` block to disable that beat. See
[the setup guide §7.3](./claude-code-setup.md#73-per-hook-opt-out)
for the per-hook opt-out patterns.

### `permissions`

A starting-point allow / deny list that cuts permission prompts
during normal mneme + Rust development without opening the door to
destructive operations.

**Allowed** without prompting:

- `mcp__mneme__*` — every mneme MCP tool. The `forget` confirmation
  rule lives in `CLAUDE.md` (see template above), not here.
- `Bash(mneme:*)` — the `mneme` CLI subcommands.
- Read-only `cargo` (`build`, `check`, `test`, `fmt`, `clippy`, `doc`).
- Read-only `git` (`status`, `diff`, `log`, `show`, `branch`, `fetch`).
- Read-only `gh` (`pr view/checks/list`, `run view/list`,
  `issue view/list`, `repo view`).
- Read-only Unix (`ls`, `cat`, `head`, `tail`, `grep`, `rg`, `find`,
  `wc`, `which`).

**Denied outright** — these prompt even if a wider allow pattern
gets pasted in later:

- `Bash(git push:*)` — `git push` should always be a deliberate act.
- `Bash(git reset --hard:*)` — destroys uncommitted work silently.
- `Bash(rm -rf:*)` — destroys directory trees silently.
- `Bash(cargo publish:*)` / `Bash(cargo yank:*)` — public-registry
  side effects.

Anything not listed in `allow` or `deny` falls back to Claude Code's
default behaviour (prompt the user). That's the right shape for an
allowlist: tight on what's pre-approved, narrow on what's outright
forbidden, prompt on everything else.

## 3. Refining over time

The list above is a starting point, not a finished set. After a few
sessions, run the `/fewer-permission-prompts` skill — it scans your
recent transcripts for read-only Bash and MCP tool calls that
prompted, and proposes additions to the allow list. Iterate from
your real usage rather than guessing patterns up-front.

## 4. Verifying the setup

After dropping in the files and restarting Claude Code:

1. **MCP listing** — type `/mcp`. You should see `mneme` with 12
   tools and 5 resources.
2. **Hook smoke** — pin a sentinel rule and confirm the agent reads
   it on session start. Walkthrough in
   [the setup guide §7.4](./claude-code-setup.md#74-verifying).
3. **Permission smoke** — try a read-only `git status`. It should
   run without a permission prompt. Try a `git push`. It should
   prompt.

If `/mcp` doesn't list mneme, see
[the setup guide's troubleshooting section](./claude-code-setup.md#8-troubleshooting).
