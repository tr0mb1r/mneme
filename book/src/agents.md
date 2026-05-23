# Supported agents

mneme is MCP-native — any agent that speaks the [Model Context Protocol]
can use it. The matrix below tracks where mneme ships a turnkey
installer (`mneme init <agent>`), what surfaces the installer touches,
and which v1.1 daemon features are available end-to-end.

[Model Context Protocol]: https://modelcontextprotocol.io/

## Tier 1 — turnkey installer + dogfooded

| Agent | Installer | MCP config file | Instruction file | Hooks API | Auto-spawn (D12) | Auto-reconnect | Notes |
|---|---|---|---|---|---|---|---|
| Claude Code | `mneme init claude-code` | `~/.claude.json` (`mcpServers.mneme`) | `~/.claude/CLAUDE.md` | SessionStart, PreCompact, Stop | ✅ | ✅ | Reference implementation. Hooks deliver memory-protocol nudges per-turn. |
| Claude Desktop | `mneme init claude-desktop` | `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) / `~/.config/Claude/claude_desktop_config.json` (Linux) / `%APPDATA%/Claude/claude_desktop_config.json` (Windows) | Manual paste into system prompt | ❌ | ✅ | ✅ | No hooks — agent calibration relies on system-prompt paste or first-use tool-description discovery. |
| Cursor | `mneme init cursor` | `~/.cursor/mcp.json` (uniform across macOS/Linux/Windows) | Per-repo `.cursor/rules/*.mdc` or legacy `.cursorrules` | ❌ | ✅ | ✅ | No global instruction-file install path — the installer drops the MCP entry and prints the rules-file content for the user to paste per-repo. |
| OpenCode | `mneme init opencode` | `~/.config/opencode/opencode.json` (`mcp.mneme`) | mneme-owned file referenced via `instructions[]` | ❌ (but `instructions[]` auto-loads) | ✅ | ✅ | `instructions[]` is the cleanest of the no-hooks agents — guidance file is loaded on every session without manual paste. |

**All four** use the `mneme client` bridge under the hood. That means
the daemon features below apply uniformly:

- **D12 auto-spawn** (commit `742003e`, wait-budget bumped 5s→30s at
  `8846268`): when the agent's MCP host launches `mneme client` and the
  daemon isn't running, the client transparently spawns the daemon and
  waits up to 30 s for the socket to appear. No `mneme daemon` step in
  any agent's setup docs.
- **Auto-reconnect on EOF** (same commit): if the daemon restarts
  mid-session (binary swap, crash, idle-timeout), `mneme client` detects
  the socket EOF and re-dials with backoff, replaying the MCP
  initialize handshake so the host doesn't see a `-32002` error.
- **Active-drain on SIGTERM** (commit `1664a8b`): `mneme stop` now
  exits cleanly in milliseconds even with multiple connected clients
  (was 30 + s prior). Visible in dev iteration; transparent to agents.

## Tier 2 — deferred

These agents are on the roadmap but not yet shipped. See the working-
tree pin in scope `mneme` (procedural memory) for current owner / ETA.

| Agent | Status | Blocker / planned milestone |
|---|---|---|
| cline (VS Code extension) | Deferred | JSON-config installer path needs the extension-settings convention (different from `mneme init claude-code`'s `~/.claude.json` shape). |
| codex | Deferred | Needs an `AGENTS.md` marker-block installer (similar to Claude Code's `CLAUDE.md` template, but with codex's instruction-file conventions). |
| gemini-cli | Deferred to B.M4 (mid-July) | Per release-planning v2.1 §4.4. |

If you're using one of these today via a manual MCP-config edit, the
runtime features (auto-spawn, auto-reconnect, drain broadcast) work the
same as the Tier-1 agents — the only thing the per-agent installer adds
is the one-command setup. The protocol surface itself is uniform.

## Known limitations (multi-agent)

These caveats apply when more than one MCP host connects to the same
mneme daemon concurrently — i.e. you're running Claude Code + Cursor +
OpenCode against the same `~/.mneme/`:

- **Scope leak across agents** (DX queue item #1). `ScopeState` is
  process-global in the daemon — one `Arc<ScopeState>` is shared across
  every connection task (`src/scope.rs:13` documents it as
  "process-lifetime only"). The `MNEME-AUTH` handshake passes no agent
  identity. **Result:** if Cursor calls `switch_scope("cursor")`, the
  next defaulted-scope `remember` / `pin` / `record_event` from a
  concurrently-connected Claude Code session will land in `cursor`, not
  `global`. The planned fix (per-connection `ScopeState` + an
  `MNEME-AGENT` handshake field + installer plumbing) is ADR-worthy
  and tracked as the first DX-queue item in the working-tree pin.
  Workaround for now: pass `scope=<name>` explicitly on every write
  when running multiple agents concurrently.

## Adding a new agent

Each Tier-1 installer lives at `src/init/agents/<agent>.rs`. The
contract is the same across all four:

1. Resolve the agent's MCP-config file path (per-OS if needed).
2. Merge the `mneme` entry into the agent's MCP-server registry using
   `crate::init::json_config` (idempotent — running `mneme init <agent>`
   twice is a no-op).
3. Drop the mneme instruction file alongside (`MNEME_MD_TEMPLATE` from
   `crate::init::assets`) — exact location varies per agent's
   instruction-file convention.
4. Stamp a marker block in the instruction file via
   `crate::init::marker` so `mneme init <agent> --upgrade` can rewrite
   without trashing user content around the block.
5. (Optional) Install agent-specific hook scripts via
   `crate::init::assets::write_executable`.

Use `src/init/agents/claude_code.rs` as the reference. The other three
(`claude_desktop.rs`, `cursor.rs`, `opencode.rs`) demonstrate variants
for agents without a hooks API.
