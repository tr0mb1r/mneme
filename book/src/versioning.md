# Versioning

Mneme tracks three versions independently. Conflating them causes pain.

| Axis | Where it lives | Bump cadence |
|------|----------------|--------------|
| Crate / binary | `Cargo.toml` `package.version` + git tag `vX.Y.Z` | Per release |
| On-disk schema | `~/.mneme/schema_version` + `src/migrate/` | Only when format changes |
| MCP protocol | hardcoded in `src/mcp/server.rs` (`2025-06-18`) | Only when MCP spec advances |

## Current — `1.x` (post-1.0)

Strict SemVer with project-specific definitions:

- **MAJOR** — MCP tool removed / renamed / signature-changed,
  breaking `config.toml` change that won't auto-migrate, on-disk
  format requiring `mneme restore` from backup (not auto-migration).
- **MINOR** — new tool, new resource, new optional config field,
  schema bump that auto-migrates on first boot, new `Embedder` /
  `Storage` trait method with default impl.
- **PATCH** — bug fixes, perf, refactors, docs, dependency bumps
  that don't change behavior.

Every release is tagged: `git tag v1.X.Y && git push --tags`. The tag
triggers `.github/workflows/release.yml`, which cross-builds the five
release targets and attaches binaries + sha256 sums to a GitHub
Release.

## How 1.0 was reached

v1.0 shipped 2026-05-18 once all five gates were satisfied:

1. **Homebrew formula shipped** via `tr0mb1r/homebrew-mneme`.
2. **Release pipeline ships prebuilt binaries** for five targets on
   every tag (`release.yml`).
3. **30-day soak passed on real workloads** (clock from 2026-04-29
   `0.2.0` baseline).
4. **No `schema_version` bump for one full release cycle** — proved
   the on-disk format stable.
5. **MCP tool surface frozen** — the canonical inventory is
   [MCP surface](./mcp-surface.md); the contract semver tracks from
   1.0 onward.

1.0 means *"I will not break your install for 12+ months without a
major bump."* Pre-1.0 history (`0.x` line) ships under the same five-
target cross-build pipeline; see CHANGELOG.

## Invariants

- Schema bumps and migration code in `src/migrate/` ship in the
  **same PR** as the format change. Never later.
- Conventional Commits drive the bump level: `feat:` → MINOR, `fix:`
  → PATCH, `feat!:` / `BREAKING CHANGE:` → MAJOR.
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
