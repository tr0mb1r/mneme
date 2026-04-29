# Contributing to Mneme

Thanks for your interest. This document is short on purpose.

## Before you start

1. Read [`docs/MEMORY_LAYERS.md`](docs/MEMORY_LAYERS.md) for a per-layer
   walkthrough — what each layer holds, where it lives on disk, and what
   runs vs. what's deferred.
2. Skim [`docs/CLAUDE_CODE_SETUP.md`](docs/CLAUDE_CODE_SETUP.md) for the
   end-user setup flow; understanding the install/configure path keeps
   reviewer-focused changes from breaking it.

## Working on Mneme

- **Respect the seams.** `src/storage/mod.rs`, `src/embed/mod.rs`, and
  `src/index/mod.rs` define traits that protect long-term portability —
  swapping redb / candle / instant-distance stays local to those modules.
  Don't bypass the seams from elsewhere in the tree.
- **Layer boundaries are not advisory.** L1 (working session) never
  outlives a process beyond its checkpoint. L4 (semantic) has exactly one
  home (`mem:` prefix in redb + HNSW). Don't blur layers.
- **Stay in Mneme's lane.** No code embedding, no knowledge graph, no
  SaaS. The README's "What it isn't" section is the source of truth.

## Style

- `cargo fmt` (rustfmt defaults).
- `cargo clippy --all-targets -- -D warnings` must pass.
- All public APIs return `Result<T, MnemeError>`.
- No `unwrap()` in non-test code.
- Tests are mandatory for non-trivial changes.
- Changes that touch the durability path (WAL, redb, HNSW snapshot/delta,
  backup/restore) must pass `scripts/manual_test.sh --stub` end-to-end
  before review.

## Commits

Follow Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, `sec:`).

## What needs review

Anything that touches the durability path (WAL, redb integration, HNSW
snapshot+delta, backup/restore, schema migration) needs a second pair
of eyes. Crash tests are mandatory; the `scripts/manual_test.sh`
end-to-end run is the floor, not the ceiling.
