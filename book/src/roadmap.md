# Roadmap — what isn't built yet

This page is the single public list of things mneme deliberately does
*not* do yet. Before it existed, every deferred item lived in a source
comment, so the only way to answer "is this supported?" was to read the
tree. If you hit something missing, look here first.

Nothing on this page is a promise of a date. Items are grouped by why
they're outstanding, which is more useful than a version number.

## Platform gaps

**Windows runs `mneme run` only.** `mneme daemon` and `mneme client`
depend on Unix domain sockets; Windows named-pipe support (ADR-0012
D2/D9) isn't implemented, so the daemon-mode default doesn't apply
there. `mneme stop` also returns `not yet implemented` on Windows — use
Task Manager or `Stop-Process`. Every other subcommand works, and the
release pipeline ships a Windows binary. Configure your MCP host with
`args: ["run"]` rather than `["client"]`.

**No SSE / Streamable HTTP transport.** `[mcp] transport` accepts only
`stdio`, and the value isn't consulted. `[mcp] sse_port` is reserved and
has no effect. There is no remote or multi-machine story: mneme is a
local process serving local hosts. ADR-0012 D7 (keepalive frames) is
deferred with it.

## Agent installers

`mneme init <agent>` is fully wired for `claude-code`,
`claude-desktop`, `cursor`, and `opencode`. Three targets are declared
in `--help` but return `not yet implemented` with a tracked pointer
rather than silently doing nothing:

| Agent | Status |
|---|---|
| `cline` | Stub. VS Code extension; MCP config path needs validating on a real install. |
| `codex` | Stub. |
| `gemini-cli` | Stub. |

`mneme init <agent> --upgrade` is currently an alias for the plain
install (the install *is* the upgrade — every write overwrites). The flag
exists so a future behaviour split lands cleanly.

## Reserved configuration

These keys load without error so existing `config.toml` files keep
working, but nothing reads them. Since v1.3 `mneme init` no longer
writes them into new configs.

| Key | Reality |
|---|---|
| `[storage] encryption` | Encryption is gated by the presence of `keystore.json` (run `mneme encrypt`), not by this flag. |
| `[mcp] sse_port` | No SSE transport exists. |
| `[telemetry]` (whole section) | No telemetry subsystem exists. mneme makes no network calls on any code path. |
| `[consolidation] schedule` | Only `"idle"` is implemented. Any other value logs a warning and falls back to it. Future modes (`every_<n>m`, cron, `on_demand`) aren't built. |

## Tool surface

**`forget` can't do bulk deletes.** The JSON Schema advertises `query`
and `scope` parameters; the runtime rejects both with "not yet
supported". Pass `id`. Cold-archive entries are out of reach by design —
`forget` covers L4, L0, and L3 hot+warm only.

**`remember`'s `pinned` flag is ignored.** Recognised by the schema,
logged when set, not wired to the procedural layer. Call `pin`
separately if you want a rule in L0.

**No structured tool output.** MCP 2025-06-18 supports `outputSchema` +
`structuredContent`; mneme returns JSON inside a text block, so agents
re-parse it. The shapes are stable and documented in
[MCP surface](./mcp-surface.md), they're just not machine-declared.

**No `prompts`.** The server advertises a `prompts` capability and
returns an empty list. `summarize_session` is a prompt template wearing
tool clothing and is the natural first entry.

**No `resources/subscribe`.** Resources are poll-only; the capability is
advertised as `false`. No `logging/setLevel` either — log level comes
from `config.toml`.

## Retrieval and memory hygiene

**No lexical or exact-match search.** `recall` is pure vector search, so
it's weakest exactly where a coding agent often needs strength: an
environment variable name, a ULID, an error string, a function
identifier. There is no substring or keyword path anywhere — `mneme
inspect --query` also goes through the embedder. A hybrid index (BM25 or
a simple inverted index over `content`, fused with the vector hits)
would be the single biggest recall improvement available. Workaround
today: `mneme export | jq`.

**L4 has no lifecycle.** L3 has a full hot → warm → cold pipeline with a
scheduler; L4 grows monotonically. What exists as of v1.3 is a
`duplicate_advisory` on `remember` (tells the agent when new content
restates an existing memory — advisory only, the write still lands) and
an orphan-vector sweep in the consolidation pass. What doesn't exist:

- **No `last_accessed` on semantic or procedural items.** `MemoryItem`
  has no touch-on-read field, so L4 carries no recency signal at all and
  nothing can decay. Adding it is an on-disk schema change (postcard is
  not self-describing), so it needs a migration.
- **No compaction.** Nothing merges or retires near-duplicate facts;
  the advisory only reports them.
- **`EpisodicStore::touch` is never called from the tool surface**, so
  L3's `last_accessed` ordering is effectively creation order.

**No graph fields on events.** ADR-0011's structured cross-references
aren't on `EpisodicEvent`; relations travel inside the free-form JSON
payload (e.g. `"references": ["01ABC…"]`). `book/src/mcp-surface.md`
documents the convention.

**Token budget is an estimator.** `chars / 4`, not a real tokenizer, so
`auto_context_token_budget` is approximate. The `tokenizers` crate is
already a dependency; the swap is one function.

**`mneme://stats` rescans on every read.** `memories.large_memory_count`
walks the whole `mem:` prefix each time. Fine at current cardinalities,
and the obvious fix (invalidate from `remember`/`update`/`forget`) is
noted where it belongs.

## Schema polish

Per `src/lib.rs`'s schema overview: `last_accessed` on semantic and
procedural, and `kind` on procedural, are both absent. Neither breaks
the current schema to add later, but both need a migration.

## Not planned

Not "someday" — deliberately out of scope, per the README's
"What it isn't":

- Code embedding or codebase indexing. Agents read code with shell
  tools.
- A knowledge graph.
- A hosted or SaaS mode.
- Calling an LLM from the server. Mneme never completes text; the agent
  owns the host completion path. `summarize_session` returns a template
  for the *agent* to fill.
- A library API. The crate publishes a binary; the Rust API is private
  and not semver-tracked. The MCP wire surface is the contract.
