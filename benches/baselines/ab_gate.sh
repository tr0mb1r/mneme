#!/usr/bin/env bash
# Same-machine A/B benchmark of a base commit against the working tree.
#
#   benches/baselines/ab_gate.sh <base-ref> [out-dir]
#
# Builds the hot-path bench binaries once for <base-ref> (in a temporary
# git worktree) and once for the working tree, then runs them in
# interleaved rounds: base, head, base, head, ... Each side keeps the
# per-bench minimum across rounds. Writes <out-dir>/base.json and
# <out-dir>/head.json for compare.py; it does not judge them itself.
#
# Why interleaved minima: `remember` is one fdatasync plus a few thread
# wake-ups per call, so its time follows the disk and the scheduler far
# more than the code. One base pass followed minutes later by one head
# pass differed by more than 20 % on identical code. Alternating the two
# sides exposes both to the same drift, and noise only ever adds time,
# so the fastest round is the fairest estimate of each.
#
# Environment:
#   AB_ROUNDS       interleaved rounds (default 3)
#   AB_BENCHES      benches both sides run (default: the four hot paths)
#   AB_HEAD_ONLY    benches only the working tree runs, once, folded
#                   into head.json (default: none)
#   MNEME_BENCH_N   corpus size (default 1000)
set -euo pipefail

base_ref="${1:?usage: ab_gate.sh <base-ref> [out-dir]}"
out="$(mkdir -p "${2:-/tmp/perf}" && cd "${2:-/tmp/perf}" && pwd)"
rounds="${AB_ROUNDS:-3}"
benches="${AB_BENCHES:-remember recall cold_start auto_context}"
head_only="${AB_HEAD_ONLY:-}"
export MNEME_BENCH_N="${MNEME_BENCH_N:-1000}"

repo="$(git rev-parse --show-toplevel)"
here="$repo/benches/baselines"
# One target dir for both builds, so the base reuses every dependency
# the head (or the CI cache) already compiled.
target="$(cd "$repo" && cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
export CARGO_TARGET_DIR="$target"

base_tree="$out/base-tree"
rm -rf "$out/bin" "$out/criterion" "$out/rounds"
mkdir -p "$out/bin/base" "$out/bin/head" "$out/rounds"
cleanup() { git -C "$repo" worktree remove --force "$base_tree" 2>/dev/null || true; }
trap cleanup EXIT
cleanup
git -C "$repo" worktree add --quiet --detach "$base_tree" "$base_ref"

# Build `$3...` benches in checkout `$2` and copy the binaries to
# bin/<side>/<bench>. Copying matters: the two builds share a target
# dir, and the copies are what gets run, so neither side can end up
# measuring the other's binary.
build() {
  local side="$1" dir="$2"
  shift 2
  local args=()
  for b in "$@"; do args+=(--bench "$b"); done
  (cd "$dir" && cargo bench --no-run --message-format=json "${args[@]}") |
    python3 -c '
import json, shutil, sys
dest, wanted = sys.argv[1], set(sys.argv[2:])
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    t = m.get("target", {})
    if m.get("reason") == "compiler-artifact" and m.get("executable") and "bench" in t.get("kind", []):
        name = t["name"]
        if name in wanted:
            shutil.copy2(m["executable"], f"{dest}/{name}")
            wanted.discard(name)
if wanted:
    sys.exit(f"bench binaries not built: {sorted(wanted)}")
' "$out/bin/$side" "$@"
}

# shellcheck disable=SC2086
build base "$base_tree" $benches
# shellcheck disable=SC2086
build head "$repo" $benches $head_only

# Run `$3...` from bin/<side>, criterion output under criterion/<side>.
run() {
  local side="$1" dir="$2"
  shift 2
  for b in "$@"; do
    (cd "$dir" && CRITERION_HOME="$out/criterion/$side" "$out/bin/$side/$b" --bench --quick)
  done
}

extract() {
  python3 "$here/extract_baseline.py" "$3" --corpus "$MNEME_BENCH_N" \
    --criterion-root "$out/criterion/$1" --git-from "$2"
}

for r in $(seq 1 "$rounds"); do
  echo "::group::round $r/$rounds"
  # shellcheck disable=SC2086
  run base "$base_tree" $benches
  extract base "$base_tree" "$out/rounds/base-$r.json"
  # shellcheck disable=SC2086
  run head "$repo" $benches
  extract head "$repo" "$out/rounds/head-$r.json"
  echo "::endgroup::"
done

if [ -n "$head_only" ]; then
  # shellcheck disable=SC2086
  run head "$repo" $head_only
  # Re-reads the last round's hot-path numbers too; the minimum below
  # is unaffected by seeing them twice.
  extract head "$repo" "$out/rounds/head-extra.json"
fi

python3 "$here/merge_baselines.py" --strategy min "$out/base.json" "$out"/rounds/base-*.json
python3 "$here/merge_baselines.py" --strategy min "$out/head.json" "$out"/rounds/head-*.json
echo "wrote $out/base.json and $out/head.json (per-bench minimum of $rounds rounds)"
