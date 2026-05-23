#!/usr/bin/env python3
"""Compare a fresh criterion baseline against a frozen reference.

Reads two JSON files produced by `extract_baseline.py` and reports
per-bench p95 deltas. Exits 0 if every shared bench is within
tolerance (default 10 % slower); exits 1 with a summary if any
exceeds it.

Used by `.github/workflows/perf.yml` as the v1.1 release-gate
check (release-planning v2.1 §6.2). Also runnable locally:

  python3 benches/baselines/compare.py \\
      benches/baselines/v0_2_6.json \\
      benches/baselines/<your-fresh>.json

Flags:
  --tolerance-pct N   Slowdown threshold in percent (default 10.0).
  --include PATTERN   Only compare bench keys matching PATTERN
                      (substring; can be repeated).
  --exclude PATTERN   Skip bench keys matching PATTERN (substring;
                      can be repeated).
  --metric KEY        Metric to compare; defaults to `p95_ns`.
                      Useful values: `p50_ns`, `p99_ns`, `mean_ns`.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def fmt_ns(ns: float) -> str:
    """Render a nanosecond value with the most readable unit."""
    if ns >= 1_000_000:
        return f"{ns / 1_000_000:.2f} ms"
    if ns >= 1_000:
        return f"{ns / 1_000:.2f} µs"
    return f"{ns:.0f} ns"


def load(path: Path) -> dict:
    if not path.is_file():
        sys.exit(f"baseline file not found: {path}")
    try:
        return json.loads(path.read_text())
    except json.JSONDecodeError as e:
        sys.exit(f"baseline file {path} is not valid JSON: {e}")


def compare(
    base: dict,
    new: dict,
    tolerance_pct: float,
    include: list[str],
    exclude: list[str],
    metric: str,
) -> int:
    base_benches = base.get("benches", {})
    new_benches = new.get("benches", {})

    shared = sorted(set(base_benches) & set(new_benches))
    only_base = sorted(set(base_benches) - set(new_benches))
    only_new = sorted(set(new_benches) - set(base_benches))

    if include:
        shared = [k for k in shared if any(p in k for p in include)]
    if exclude:
        shared = [k for k in shared if not any(p in k for p in exclude)]

    if not shared:
        sys.exit("no overlapping bench keys between baseline and new run")

    print(
        f"compare: base={base.get('git', {}).get('describe', '?')} → "
        f"new={new.get('git', {}).get('describe', '?')} | "
        f"metric={metric} | tolerance=±{tolerance_pct:.1f}%"
    )
    print()

    regressions: list[tuple[str, float, float, float]] = []
    improvements: list[tuple[str, float, float, float]] = []
    held: list[tuple[str, float, float, float]] = []

    for key in shared:
        b = base_benches[key].get(metric)
        n = new_benches[key].get(metric)
        if b is None or n is None:
            print(f"  ?  {key}: missing {metric} in one side; skipping")
            continue
        delta_pct = (n - b) / b * 100.0
        row = (key, b, n, delta_pct)
        if delta_pct > tolerance_pct:
            regressions.append(row)
        elif delta_pct < -tolerance_pct:
            improvements.append(row)
        else:
            held.append(row)

    def render(rows: list[tuple[str, float, float, float]], flag: str) -> None:
        for key, b, n, delta_pct in rows:
            arrow = "↑" if delta_pct > 0 else ("↓" if delta_pct < 0 else "·")
            print(
                f"  {flag} {key:55s} "
                f"{fmt_ns(b):>10s} → {fmt_ns(n):>10s}  "
                f"{arrow} {delta_pct:+6.1f}%"
            )

    if regressions:
        print("REGRESSIONS (over threshold):")
        render(regressions, "🔴")
        print()
    if improvements:
        print("IMPROVEMENTS (over threshold):")
        render(improvements, "🟢")
        print()
    if held:
        print("HELD (within threshold):")
        render(held, "·")
        print()
    if only_base:
        print(f"benches removed since baseline: {', '.join(only_base)}")
    if only_new:
        print(f"benches added since baseline:   {', '.join(only_new)}")

    if regressions:
        print()
        print(
            f"FAIL: {len(regressions)} bench(es) regressed > {tolerance_pct:.1f}%. "
            "Investigate before merging or document the justification per "
            "release-planning v2.1 §6.2."
        )
        return 1
    print()
    print(
        f"OK: every shared bench within ±{tolerance_pct:.1f}% on {metric}."
    )
    return 0


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("baseline", type=Path, help="Reference baseline JSON.")
    ap.add_argument("new", type=Path, help="Fresh baseline JSON to compare.")
    ap.add_argument("--tolerance-pct", type=float, default=10.0)
    ap.add_argument("--include", action="append", default=[])
    ap.add_argument("--exclude", action="append", default=[])
    ap.add_argument("--metric", default="p95_ns")
    args = ap.parse_args()

    base = load(args.baseline)
    new = load(args.new)
    sys.exit(
        compare(
            base,
            new,
            tolerance_pct=args.tolerance_pct,
            include=args.include,
            exclude=args.exclude,
            metric=args.metric,
        )
    )


if __name__ == "__main__":
    main()
