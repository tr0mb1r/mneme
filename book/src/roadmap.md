# Roadmap — what isn't built yet

This page is the single public list of things mneme deliberately does
*not* do yet. Before it existed, every deferred item lived in a source
comment, so the only way to answer "is this supported?" was to read the
tree. If you hit something missing, look here first.

Nothing on this page is a promise of a date. Items are grouped by why
they're outstanding, which is more useful than a version number.

## Platform gaps

**Linux and macOS only.** Windows support was dropped after 1.5: there
is no Windows binary, CI no longer builds or tests on Windows, the
Windows-specific code has been removed, and a Windows build stops with
a compile error. A Windows host can run
mneme inside WSL2 (it is Linux there), or reach a `mneme serve` running
elsewhere over Streamable HTTP.

**Streamable HTTP has no server-to-client stream.** `mneme serve` (and
the daemon with `[http] enabled`) answer every request with plain JSON
and return `405` for `GET /mcp`, because mneme never sends
notifications or requests to the client. Resumable streams
(`Last-Event-ID`), JSON-RPC batching (removed in MCP 2025-06-18), and
TLS are not implemented; put a reverse proxy in front for TLS. The
legacy HTTP+SSE transport (MCP 2024-11-05) isn't offered.

## Agent installers

`mneme init <agent>` is fully wired for `claude-code`,
`claude-desktop`, `cursor`, `opencode`, and `hermes`. Three targets are declared
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
| `[mcp] sse_port` | Superseded by `[http] bind`. |
| `[telemetry]` (whole section) | No telemetry subsystem exists. mneme makes no network calls on any code path. |
| `[consolidation] schedule` | Only `"idle"` is implemented. Any other value logs a warning and falls back to it. Future modes (`every_<n>m`, cron, `on_demand`) aren't built. |
| `[recall] stemming` | Declared in 1.6; `"en"` is accepted and does nothing yet. See below. |

## Tool surface

**`forget` can't do bulk deletes.** It takes `id` only. (Up to 1.5.1 the
schema also advertised `query` and `scope`, which the runtime always
rejected; they are gone from the schema, and passing one returns
`INVALID_PARAMS` with a pointer to "find the ids, forget each one".)
Cold-archive entries are out of reach by design — `forget` covers L4,
L0, and L3 hot+warm only. Since 1.6 the kinds in
`[consolidation] cold_exempt_kinds` never reach the cold archive, so
they stay forgettable.

**`remember` does not pin.** `pinned: true` is rejected with
`INVALID_PARAMS` and a pointer to `pin` (up to 1.5.1 it was logged and
ignored, so the caller believed a rule had been pinned). The argument is
still declared, marked deprecated, so `pinned: false` from an older
client keeps working. Call `pin` if you want a rule in L0.

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

**Keyword search has no stemming and no substring matching.** `recall`
is hybrid (vector + BM25, see
[MCP surface](./mcp-surface.md#hybrid-recall)), but the keyword half
matches whole tokens: `backtest` doesn't match `backtesting`, and a
fragment of an identifier only matches at `_ . - / :` boundaries
(`gateway` finds `IBKR_GATEWAY_PORT`; `gatew` doesn't). The same holds
for the event index `recall(layers=[…, "episodic"])` uses. Both indexes
are rebuilt in memory on the first recall after each start, one pass
over the stored rows. `[recall] stemming = "en"` is reserved for this
and not implemented.

**The cold archive is not searched.** `recall` covers hot and warm
events. Cold events can be read by time (`recall_recent` /
`export` with `include_cold`), not found by content.

**Events are found by their words only.** L3 has no embeddings
(ADR-0007), so `recall` over events is lexical: a paraphrase that shares
no word with the event misses it. A short memory per conclusion is still
what makes it findable by meaning.

**`recall` defaults to memories only.** Searching events is opt-in
(`layers`), until the quality checks show mixed results never push a
better memory out.

**L4 has no automatic lifecycle.** L3 has a full hot → warm → cold
pipeline with a scheduler; L4 grows monotonically unless the agent
curates it. What exists: `supersedes` on `remember` / `update` (retires
an outdated memory from recall without deleting it),
`update(status: "archived")` (1.6: takes a memory out of recall and
auto-context, reversibly), `updated_at` (1.6: auto-context decays a
memory from its last edit), a similarity advisory on every `remember`
that names the closest current memory (≥ 0.80), and an orphan-vector
sweep in the consolidation pass. What doesn't exist:

- **No `last_accessed` on semantic or procedural items.** Nothing
  records that a memory was read, so there is no "unused for a year"
  signal. `recall` ranking is not decayed at all.
- **No automatic compaction or archiving.** Nothing merges, retires or
  archives memories on its own; the advisory reports near-duplicates and
  `supersedes` / `status` are the agent's tools.
- **Relations are stated, never inferred.** `relations` on `remember` /
  `update` (1.6) record that one memory refines, contradicts or
  supersedes another, or what it cites as evidence. The server does not detect contradictions or
  propose links beyond the similarity advisory.
- **Only `recall` refreshes an event's `last_accessed`** (1.6, hot
  events, at most daily). Reading an event through `recall_recent` or
  auto-context does not.

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
procedural, and `kind` on procedural, are both absent. `updated_at` and
`status` were added in 1.6 as side tables rather than struct fields, so
`schema_version` is still 1; the same route is open for these.

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
