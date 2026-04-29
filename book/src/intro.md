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

Pre-1.0. Phases 0–5 complete; Phase 6 (portability + diagnostics)
substantially complete. The on-disk format is stable behind a versioned
schema with a migration path, but the project hasn't yet cleared its
30-day soak, shipped a Homebrew formula, or published a release pipeline.
Treat as production-capable for personal use, not yet 1.0.

## What works today

| Layer | Tools | Resource | Storage |
|-------|-------|----------|---------|
| L0 procedural (always-on) | `pin`, `unpin` | `mneme://procedural` | JSONL on disk, hot-reloaded |
| L1 working session | (live state) | `mneme://session/{id}` | `~/.mneme/sessions/<id>.snapshot` |
| L3 episodic (recent events) | `recall_recent`, `summarize_session` | `mneme://recent` | redb hot tier + zstd cold quarters |
| L4 semantic (long-term facts) | `remember`, `recall`, `update`, `forget` | — | redb + WAL + HNSW vector index |
| Auto-context | — | `mneme://context` | All four layers, packed to a token budget |
| Diagnostics | `stats`, `list_scopes`, `export`, `switch_scope` | `mneme://stats` | — |

`mneme run` speaks JSON-RPC over stdio against MCP protocol `2025-06-18`,
advertises **12 tools** and **5 resources**, and survives malformed JSON,
oversize frames, and EOF cleanly.

## Where to next

* **[Installation](./installation.md)** — get the binary on `$PATH`.
* **[Setting up with Claude Code](./claude-code-setup.md)** — wire mneme
  into your daily agent workflow.
* **[Memory layers](./memory-layers.md)** — what each tier holds, where
  it lives on disk, what's wired vs. deferred.

[mcp]: https://modelcontextprotocol.io
