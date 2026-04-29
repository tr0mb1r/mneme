# Versioning

Mneme tracks three versions independently. Conflating them causes pain.

| Axis | Where it lives | Bump cadence |
|------|----------------|--------------|
| Crate / binary | `Cargo.toml` `package.version` + git tag `vX.Y.Z` | Per release |
| On-disk schema | `~/.mneme/schema_version` + `src/migrate/` | Only when format changes |
| MCP protocol | hardcoded in `src/mcp/server.rs` (`2025-06-18`) | Only when MCP spec advances |

## Pre-1.0 (current — `0.x`)

Cargo treats `0.MINOR.x` as the breaking boundary, so SemVer applies
with one shift:

- `0.MINOR.0` — breaking change: MCP tool rename / remove / signature
  change, `config.toml` shape change, on-disk layout requiring
  migration, public Rust API change.
- `0.x.PATCH` — additive / fix only: new tool, new optional field,
  bug fix, perf, docs, dependency bump.

Every release is tagged: `git tag v0.X.Y && git push --tags`. The tag
triggers `.github/workflows/release.yml`, which cross-builds the five
release targets and attaches binaries + sha256 sums to a GitHub
Release.

## 1.0 trigger

Mneme flips to 1.0 only when **all** of the following are true. There
are five gates, not three.

1. **Homebrew formula shipped** (post-1.0 in this project's plan; see
   the [installation page](./installation.md)).
2. **Release pipeline ships prebuilt binaries** for the five targets
   on every tag — handled by `release.yml`.
3. **30-day soak passed on real workloads.** The clock started
   2026-04-29 with the `0.2.0` baseline. Any `schema_version`
   migration that ships before day 30 restarts the clock.
4. **No `schema_version` bump for one full release cycle.** Proves
   the on-disk format has stabilized.
5. **MCP tool surface frozen** with no pending renames, removals, or
   signature changes. Twelve tools as of `0.2.0`: `pin`, `unpin`,
   `remember`, `recall`, `recall_recent`, `update`, `forget`,
   `summarize_session`, `stats`, `list_scopes`, `export`,
   `switch_scope`.

1.0 means *"I will not break your install for 12+ months without a
major bump."* The promise has to be keepable.

## Post-1.0

Strict SemVer with project-specific definitions:

- **MAJOR** — MCP tool removed / renamed / signature-changed,
  breaking `config.toml` change that won't auto-migrate, on-disk
  format requiring `mneme restore` from backup (not auto-migration).
- **MINOR** — new tool, new resource, new optional config field,
  schema bump that auto-migrates on first boot, new `Embedder` /
  `Storage` trait method with default impl.
- **PATCH** — bug fixes, perf, refactors, docs, dependency bumps
  that don't change behavior.

## Invariants

- Schema bumps and migration code in `src/migrate/` ship in the
  **same PR** as the format change. Never later.
- Conventional Commits drive the bump level: `feat:` → MINOR, `fix:`
  → PATCH, `feat!:` / `BREAKING CHANGE:` → MAJOR (or pre-1.0 MINOR).
- Tag-driven release: `v*.*.*` tags trigger the cross-build matrix
  and GitHub Release. Post-1.0 the same tag also updates the
  Homebrew formula.

## Tooling

- **release-plz** — opens a release PR on every push to `main` that
  bumps `Cargo.toml` and updates `CHANGELOG.md` from Conventional
  Commits since the last release. Configured at `release-plz.toml`.
- **release.yml** — tag-triggered cross-build + GitHub Release.
- **publish.yml** — manual `workflow_dispatch` that publishes a tagged
  version to crates.io as [`mneme-mcp`](https://crates.io/crates/mneme-mcp)
  via OIDC trusted publishing. Trigger after merging a release PR and
  pushing the tag:
  `gh workflow run publish.yml --field tag=vX.Y.Z`
