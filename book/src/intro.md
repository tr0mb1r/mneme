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

Pre-1.0 — `0.2.x` line, latest `0.2.3` published as
[`mneme-mcp`](https://crates.io/crates/mneme-mcp) on crates.io and via
[Homebrew](https://github.com/tr0mb1r/homebrew-mneme). Phases 0–5
complete; Phase 6 (portability + diagnostics + release infrastructure)
substantially complete. Code-side feature work for 1.0 is done; the
remaining gates are calendar-bound: 30-day soak (clock started
2026-04-29) and one full release cycle without a `schema_version` bump.
The on-disk format is stable behind a versioned schema with a migration
path. Treat as production-capable for personal use, not yet 1.0. See
[Versioning](./versioning.md) for the policy and 1.0 gates.

## What works today

| Layer | Tools | Resource | Storage |
|-------|-------|----------|---------|
| L0 procedural (always-on) | `pin`, `unpin` | `mneme://procedural` | JSONL on disk, hot-reloaded |
| L1 working session | (live state) | `mneme://session/{id}` | `~/.mneme/sessions/<id>.snapshot` |
| L3 episodic (recent events) | `recall_recent`, `summarize_session`, `record_event` | `mneme://recent` | redb hot tier + zstd cold quarters |
| L4 semantic (long-term facts) | `remember`, `recall`, `update`, `forget` | — | redb + WAL + HNSW vector index |
| Auto-context | — | `mneme://context` | All four layers, packed to a token budget |
| Diagnostics | `stats`, `list_scopes`, `export`, `switch_scope` | `mneme://stats` | — |

`mneme run` speaks JSON-RPC over stdio against MCP protocol `2025-06-18`,
advertises a focused MCP tool and resource surface (see
[MCP surface](./mcp-surface.md) for the authoritative inventory),
and survives malformed JSON, oversize frames, and EOF cleanly.

## Where to next

* **[Installation](./installation.md)** — get the binary on `$PATH`.
* **[Setting up with Claude Code](./claude-code-setup.md)** — wire mneme
  into your daily agent workflow.
* **[Memory layers](./memory-layers.md)** — what each tier holds, where
  it lives on disk, what's wired vs. deferred.

[mcp]: https://modelcontextprotocol.io
