# Contributing to Mneme

Thanks for your interest. This document is short on purpose.

## Before you start

1. Read [`proj_docs/mneme-project-specification-v2.md`](proj_docs/mneme-project-specification-v2.md). It is the canonical
   project context and resolves most ambiguity.
2. Read [`proj_docs/mneme-implementation-plan.md`](proj_docs/mneme-implementation-plan.md) to see the current phase and
   exit gates.
3. Skim the ADRs in `proj_docs/decisions/` — they explain non-obvious calls.

## Working on Mneme

- Match the design principles in spec §3. When two principles conflict,
  the higher-numbered one loses.
- **Respect the seams.** `src/storage/mod.rs`, `src/embed/mod.rs`, and
  `src/index/mod.rs` define traits that protect spec principle #10
  ("build for ten years"). Don't bypass them.
- **Layer boundaries are not advisory.** L1 never persists. L4 has one
  home. See spec §5.2.
- **Stay in Mneme's lane.** No code embedding, no knowledge graph, no
  SaaS. See spec §11.4.

## Style

- `cargo fmt` (rustfmt defaults).
- `cargo clippy --all-targets -- -D warnings` must pass.
- All public APIs return `Result<T, MnemeError>`.
- No `unwrap()` in non-test code.
- Tests are mandatory for non-trivial changes.

## Commits

Follow Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, `sec:`).

## When the spec is wrong

Update the spec in the same commit as the code change, with a short
explanation of why. The spec exists to be useful, not authoritative for
its own sake.

## What needs review

Anything that touches the durability path (WAL, redb integration, HNSW
snapshot+delta) needs a second pair of eyes. Crash tests are mandatory.
