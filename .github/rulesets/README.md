# Repository rulesets

GitHub rulesets for `tr0mb1r/mneme`, kept in the repo so they are reviewed
like code. Each file is in the format GitHub's **Import a ruleset** accepts
(the same body as `POST /repos/{owner}/{repo}/rulesets`).

To apply: **Settings → Rules → Rulesets → New ruleset → Import a ruleset**,
pick the file, review, **Create**. Re-import after editing a file (delete the
old ruleset first, or edit it in the UI to match).

| File | Applies to | What it enforces |
|---|---|---|
| `main.json` | default branch (`main`) | No deletion, no force-push, changes only through a pull request with resolved review threads, and these checks green: `test (ubuntu-latest)`, `test (macos-latest)`, `cargo-deny`, `cargo-audit`. Admins can bypass on a PR (merge despite a failing check) but cannot push directly. |
| `release-tags.json` | tags `v*` | Release tags cannot be deleted or moved once pushed. Admins can bypass (to repair a bad tag). |
| `integration-branches.json` | `dev`, `develop` | No deletion, no force-push. |

Notes:

- The required check names come from `.github/workflows/ci.yml` (`test` matrix
  over Linux and macOS, `deny`, `audit`). `ci-skipped.yml` reports the same
  names on docs-only PRs so those are not blocked. If a job is renamed or the
  matrix changes, update `main.json`, `ci.yml` and `ci-skipped.yml` together.
- `perf` (criterion p95 gate) is deliberately not required: it compares against
  a frozen baseline on shared runners and is too noisy to block merges.
- `integration_id: 15368` is the GitHub Actions app, so only Actions can
  satisfy the checks.
- No approving review is required (single maintainer); raise
  `required_approving_review_count` when a second maintainer joins.
