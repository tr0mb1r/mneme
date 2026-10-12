# Mneme

> A standalone, MCP-native memory tool for any LLM or agent.
> Single binary. Local-first. Rust. Built to last.

Mneme is a persistent memory tool for AI agents. It runs as a long-lived
process on your machine, exposes its functionality via the
[Model Context Protocol (MCP)][mcp], and lets any compatible agent —
Claude Desktop, Claude Code, Cursor, OpenCode, Hermes Agent — remember things across
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

**v1.5.0 shipped 2026-10-12** — hardening from day-to-day use as an
agent's long-term memory: credential-shaped writes are refused by
default (`[guardrails] secrets`), every tool rejects undeclared
arguments, `record_event` needs a real payload, exact duplicates are
short-circuited, `stats` explains what consolidation is waiting for,
and the Hermes hook injects pinned rules whole (see the
[v1.5 release notes](./release-notes-v1_5.md)).

**v1.4** (2026-10-04 / 2026-10-05) made mneme a network service:
`mneme serve` speaks MCP Streamable HTTP with bearer-token auth,
`mneme init hermes` wires [Hermes Agent](./hermes-setup.md) locally or
remotely with a context-reload hook, and `recall` became **hybrid**
(BM25 keyword matches fused with vectors) with `supersedes` to retire
outdated facts (see the [v1.4 release notes](./release-notes-v1_4.md)).

Previous: v1.3.0 (2026-07-25), v1.2.x (2026-06), v1.1.1 (2026-05-23),
v1.0 (2026-05-18). Latest release on
[`mneme-mcp`](https://crates.io/crates/mneme-mcp) (crates.io) and the
[Homebrew tap](https://github.com/tr0mb1r/homebrew-mneme). The MCP wire
surface is the semver-tracked contract from 1.0 onward (see [MCP
surface](./mcp-surface.md)); the Rust library API is private. The
on-disk format is stable behind a versioned schema with a migration
path — `schema_version` has been 1 since v1.0, so no release so far has
needed a migration. v1.2 added **encryption at rest** (opt-in,
XChaCha20-Poly1305 AEAD, OS keyring KEK + BIP39 recovery phrase). See
[Versioning](./versioning.md) for the release policy.

## What works today

| Layer | Tools | Resource | Storage |
|-------|-------|----------|---------|
| L0 procedural (always-on) | `pin`, `unpin` | `mneme://procedural` | JSONL on disk, hot-reloaded |
| L1 working session | (live state) | `mneme://session/{id}` | `~/.mneme/sessions/<id>.snapshot` |
| L3 episodic (recent events) | `recall_recent`, `summarize_session`, `record_event` | `mneme://recent` | redb hot tier + zstd cold quarters |
| L4 semantic (long-term facts) | `remember`, `recall`, `update`, `forget` | — | redb + WAL + HNSW vector index |
| Auto-context | — | `mneme://context` | All four layers, packed to a token budget |
| Diagnostics | `stats`, `list_scopes`, `export`, `switch_scope` | `mneme://stats` | — |
| Network service | `mneme serve`, or the daemon with `[http] enabled` | Streamable HTTP `/mcp`, `/healthz`, `/version` | Same store, bearer-token auth |

v1.1 ships **daemon mode** (`mneme daemon` + `mneme client` bridge —
one warm process, many MCP hosts) over a Unix domain socket, the
per-agent installer **`mneme init <agent>`** wired for `claude-code`,
`claude-desktop`, `cursor`, and `opencode` (v1.4 adds `hermes`), content **size guardrails**
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
