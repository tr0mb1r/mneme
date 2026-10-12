# v1.4 release notes

> **Status: v1.4.1 shipped 2026-10-05.** v1.4.0 shipped 2026-10-04.
> Previous: v1.3.1 (2026-07-25).

The v1.4 cycle's theme is **mneme as a network service**, with Hermes
Agent as the first host that uses it that way, and **recall that finds
exact names**.

Both releases are **minor** bumps per [Versioning](./versioning.md): the
MCP wire surface gains parameters and breaks nothing, and the on-disk
format is unchanged. `schema_version` stays 1. No migration.

---

## v1.4.0 — Streamable HTTP and Hermes Agent

**`mneme serve`** speaks MCP Streamable HTTP with bearer-token auth, so
an agent in another container or on another host can share one mneme
server. The daemon can expose the same endpoint alongside its Unix
socket with `[http] enabled = true`. Health probes (`/healthz`) are
unauthenticated; everything else needs the token. See
[CLI surface](./cli.md#mode-c--network-service-mneme-serve).

**`mneme init hermes`** wires [Hermes Agent](./hermes-setup.md), either
locally (Hermes spawns `mneme client`) or remotely (`--url`, with the
token referenced from Hermes' environment, never written into its
config). It installs the `mneme` skill and an optional memory block in
Hermes' persona file.

**Deployment.** The repo ships a `Dockerfile` and a ready-made
mneme + Hermes Docker Compose stack under `deploy/compose/hermes/`.

**Security.** rustls 0.23.45 (RUSTSEC-2026-0285) and async-trait 0.1.92.

## v1.4.1 — hybrid recall, supersedes, context reload

**Hybrid search.** An embedding smears an identifier like `live_bt.py`
into "something about a script", so a lookalike can outrank the real
match. `recall` now runs an in-memory BM25 keyword index beside the
vector search and fuses the two. A new `mode` argument picks `hybrid`
(default), `semantic` or `keyword`, and each result says whether it
matched by `semantic`, `keyword` or `both`. The keyword index is built
from the stored memories on first use and kept in step on every write,
so nothing new is written to disk. See
[MCP surface](./mcp-surface.md).

**`supersedes`.** `remember` and `update` take an optional `supersedes`
(one id or several). The older memories stay stored but drop out of
`recall` unless `include_superseded: true` is passed, so a changed fact
no longer competes with its replacement. The near-duplicate advisory on
`remember` now fires from similarity 0.80 and suggests `supersedes`.

**Context reload for Hermes.** `mneme init hermes` installs a
`pre_llm_call` hook that re-injects pinned rules, relevant memories and
recent events on a session's first turn and on the first turn after
Hermes compresses the conversation. It fails open, and `--no-hook`
skips or removes it.

## Upgrading

- Binary: `brew upgrade mneme` or `cargo install mneme-mcp`.
- Hermes installs: re-run `mneme init hermes --upgrade` to pick up the
  hook. Upgrading the binary does not update the files `init` copied.
- Rolling back to 1.4.0 or 1.3.x is safe: older binaries ignore the
  supersession links.
