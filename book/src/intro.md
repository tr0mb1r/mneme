# Mneme

> A standalone, MCP-native memory tool for any LLM or agent.
> Single binary. Local-first. Rust. Built to last.

Mneme is a persistent memory tool for AI agents. It runs as a long-lived
process on your machine, exposes its functionality via the
[Model Context Protocol (MCP)][mcp], and lets any compatible agent —
Claude Desktop, Claude Code, Cursor, Cline, Aider — remember things across
sessions.

The clearest one-line description: **Mneme remembers things about your
work that the agent would otherwise forget.**

## What it isn't

- A vector database (it uses one internally, but that's an implementation detail)
- A RAG framework
- A codebase indexer (modern agents read code with shell tools — that's not Mneme's job)
- A web service or SaaS
- A library to embed in another application
- An LLM

## Status

**v1.2.1 shipped 2026-06-06** — a same-day hotfix for the v1.2.0
encryption migration (upgrade and re-run `mneme encrypt` if v1.2.0
left your daemon unable to start; see the
[v1.2 release notes](./release-notes-v1_2.md#v121--encryption-migration-hotfix)).
Previous: v1.2.0 (2026-06-06), v1.1.1 (2026-05-23), v1.0
(2026-05-18). Latest release on
[`mneme-mcp`](https://crates.io/crates/mneme-mcp) (crates.io) and the
[Homebrew tap](https://github.com/tr0mb1r/homebrew-mneme). The MCP wire
surface is the semver-tracked contract from 1.0 onward (see [MCP
surface](./mcp-surface.md)); the Rust library API is private. The
on-disk format is stable behind a versioned schema with a migration
path. v1.2 adds **encryption at rest** (opt-in, XChaCha20-Poly1305
AEAD, OS keyring KEK + BIP39 recovery phrase) and **`recall_recent`
time-range bounds** (`since`/`until`). See
[v1.2 release notes](./release-notes-v1_2.md) and
[Versioning](./versioning.md) for details.

## What works today

| Layer | Tools | Resource | Storage |
|-------|-------|----------|---------|
| L0 procedural (always-on) | `pin`, `unpin` | `mneme://procedural` | JSONL on disk, hot-reloaded |
| L1 working session | (live state) | `mneme://session/{id}` | `~/.mneme/sessions/<id>.snapshot` |
| L3 episodic (recent events) | `recall_recent`, `summarize_session`, `record_event` | `mneme://recent` | redb hot tier + zstd cold quarters |
| L4 semantic (long-term facts) | `remember`, `recall`, `update`, `forget` | — | redb + WAL + HNSW vector index |
| Auto-context | — | `mneme://context` | All four layers, packed to a token budget |
| Diagnostics | `stats`, `list_scopes`, `export`, `switch_scope` | `mneme://stats` | — |

v1.1 ships **daemon mode** (`mneme daemon` + `mneme client` bridge —
one warm process, many MCP hosts) over a Unix domain socket, the
per-agent installer **`mneme init <agent>`** wired for `claude-code`,
`claude-desktop`, `cursor`, and `opencode`, content **size guardrails**
with a first-boot upgrade audit, and the v1.0 → v1.1 **migration test**
as a release gate. `mneme run` is still the single-host stdio fallback.
MCP protocol version `2025-06-18`.

## Where to next

* **[Installation](./installation.md)** — get the binary on `$PATH`.
* **[Setting up with Claude Code](./claude-code-setup.md)** — wire mneme
  into your daily agent workflow.
* **[Memory layers](./memory-layers.md)** — what each tier holds, where
  it lives on disk, what's wired vs. deferred.

[mcp]: https://modelcontextprotocol.io
