# Contributing to Mneme

Thanks for your interest. This document is short on purpose.

## Before you start

1. Read [`docs/MEMORY_LAYERS.md`](docs/MEMORY_LAYERS.md) for a per-layer
   walkthrough — what each layer holds, where it lives on disk, and what
   schedules move data between tiers.
2. Read [`book/src/roadmap.md`](book/src/roadmap.md) for what is
   deliberately *not* built yet. It's the canonical list; if you're
   picking up work, start there rather than grepping for `TODO`.
3. Skim [`docs/CLAUDE_CODE_SETUP.md`](docs/CLAUDE_CODE_SETUP.md) for the
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
- Changes that touch hot-path code (`src/memory/`, `src/index/`,
  `src/storage/`, `src/orchestrator/`, `src/mcp/`) trip the
  `.github/workflows/perf.yml` regression gate. It benchmarks the base
  commit and your change on the same runner and blocks merge if a
  hot-path median (p50) slows by more than 15 %. Run the same comparison
  locally:

  ```sh
  git stash; git checkout main
  cargo bench --bench remember --bench recall \
              --bench cold_start --bench auto_context -- --quick
  python3 benches/baselines/extract_baseline.py /tmp/base.json --corpus 1000
  rm -rf target/criterion; git checkout -; git stash pop
  cargo bench --bench remember --bench recall \
              --bench cold_start --bench auto_context -- --quick
  python3 benches/baselines/extract_baseline.py /tmp/this.json --corpus 1000
  python3 benches/baselines/compare.py /tmp/base.json /tmp/this.json \
          --metric p50_ns --tolerance-pct 15
  ```

  If the regression is justified (e.g. v1.1 daemon-mode network hop),
  explain the trade in the PR description; an admin merges it through
  the ruleset bypass.

## Commits

Follow Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, `sec:`).

Write a multi-paragraph commit body for substantive changes. The body
is what gives a reviewer (and a future archaeologist) the *why*; the
subject just says *what*.

## Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
`release-plz` auto-generates a thin entry from each commit's *subject*
line — that's a floor, not a target.

For substantive changes, **hand-edit `CHANGELOG.md` as part of the same
PR** that introduces the change. Use the v0.2.0 / v0.2.4 entries as the
shape:

- A short paragraph at the top of the version section that says what
  the release is *for* and what bump-level it is per
  [`book/src/versioning.md`](book/src/versioning.md).
- Sections grouped by Keep-a-Changelog convention (`Added`, `Changed`,
  `Deprecated`, `Removed`, `Fixed`, `Security`) plus optional
  `Architecture decisions`, `Documentation`, `Tests`.
- Multi-line bullets where the *what* + *why* + *how to verify* don't
  fit on one line.
- Cross-link to the relevant in-repo doc when the change is an
  architectural one — `book/src/mcp-surface.md` for wire-surface
  changes, `docs/MEMORY_LAYERS.md` for layer behaviour,
  `book/src/roadmap.md` for anything deferred. (ADRs are referenced by
  number in older entries; those live in the maintainer's working notes
  and are not published, so don't link to them.)

If the change is genuinely small (a typo fix, a CI tweak, a single
docs update), the auto-generated entry is fine — release-plz will
handle it. Use judgement; default to writing the entry by hand when
in doubt.

## What needs review

Anything that touches the durability path (WAL, redb integration, HNSW
snapshot+delta, backup/restore, schema migration) needs a second pair
of eyes. Crash tests are mandatory; the `scripts/manual_test.sh`
end-to-end run is the floor, not the ceiling.
