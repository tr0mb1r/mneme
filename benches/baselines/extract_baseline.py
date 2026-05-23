#!/usr/bin/env python3
"""Extract a portable baseline JSON from criterion's per-bench output.

Reads target/criterion/<group>/<id>/new/{estimates,sample}.json for
every bench that was run on the latest invocation and emits one
flat JSON keyed by "<group>/<id>" with mean/median/p50/p95/p99 in
nanoseconds.

Run after `cargo bench --bench remember --bench recall
--bench cold_start --bench auto_context`.

Usage:
  python3 benches/baselines/extract_baseline.py <output_path> [--corpus N]

`--corpus N` filters to bench ids that contain `n=<N>` so a single
baseline file is one corpus size; omit to include everything criterion
wrote.

This script does NOT shell out to cargo. It only reads existing
target/criterion/ output. See benches/baselines/README.md for how
the baseline is meant to be captured + compared against.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
CRITERION_ROOT = REPO_ROOT / "target" / "criterion"


def percentile(values: list[float], p: float) -> float:
    """Inclusive percentile via linear interpolation. p in [0, 100]."""
    if not values:
        return float("nan")
    sorted_v = sorted(values)
    if len(sorted_v) == 1:
        return sorted_v[0]
    k = (len(sorted_v) - 1) * (p / 100.0)
    lo = int(k)
    hi = min(lo + 1, len(sorted_v) - 1)
    frac = k - lo
    return sorted_v[lo] + (sorted_v[hi] - sorted_v[lo]) * frac


def per_iter_ns(sample: dict) -> list[float]:
    """times[i] is total ns for iters[i] iterations; return per-iter ns."""
    iters = sample.get("iters", [])
    times = sample.get("times", [])
    return [t / i for t, i in zip(times, iters) if i > 0]


def find_new_dirs(corpus: int | None) -> list[Path]:
    """Walk target/criterion for `*/new/` dirs holding both jsons."""
    if not CRITERION_ROOT.is_dir():
        sys.exit(f"no criterion output at {CRITERION_ROOT} — run cargo bench first")
    found: list[Path] = []
    for sample in CRITERION_ROOT.rglob("new/sample.json"):
        new_dir = sample.parent
        if not (new_dir / "estimates.json").is_file():
            continue
        if corpus is not None:
            # Bench id is the directory two levels up from `new/`:
            #   target/criterion/<group>/<id>/new/
            bench_id = new_dir.parent.name
            if f"n={corpus}" not in bench_id:
                continue
        found.append(new_dir)
    return sorted(found)


def bench_label(new_dir: Path) -> str:
    """Map target/criterion/<group>/<id>/new → '<group>/<id>'."""
    bench_id = new_dir.parent.name
    group = new_dir.parent.parent.name
    return f"{group}/{bench_id}"


def summarize(new_dir: Path) -> dict:
    sample = json.loads((new_dir / "sample.json").read_text())
    estimates = json.loads((new_dir / "estimates.json").read_text())
    per_iter = per_iter_ns(sample)
    return {
        "samples": len(per_iter),
        "min_ns": min(per_iter) if per_iter else float("nan"),
        "max_ns": max(per_iter) if per_iter else float("nan"),
        "p50_ns": percentile(per_iter, 50),
        "p95_ns": percentile(per_iter, 95),
        "p99_ns": percentile(per_iter, 99),
        "mean_ns": estimates["mean"]["point_estimate"],
        "median_ns": estimates["median"]["point_estimate"],
        "stddev_ns": estimates["std_dev"]["point_estimate"],
    }


def git_describe() -> dict:
    def run(*args: str) -> str:
        return subprocess.check_output(args, cwd=REPO_ROOT, text=True).strip()

    return {
        "branch": run("git", "rev-parse", "--abbrev-ref", "HEAD"),
        "sha": run("git", "rev-parse", "HEAD"),
        "describe": run("git", "describe", "--tags", "--always", "--dirty"),
    }


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("output", help="Where to write the baseline JSON.")
    ap.add_argument("--corpus", type=int, default=None,
                    help="Filter to bench ids containing n=<corpus>.")
    args = ap.parse_args()

    new_dirs = find_new_dirs(args.corpus)
    if not new_dirs:
        filt = f" with corpus n={args.corpus}" if args.corpus else ""
        sys.exit(f"no criterion benches found{filt}")

    benches = {bench_label(d): summarize(d) for d in new_dirs}
    payload = {
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "git": git_describe(),
        "host": {
            "uname": os.uname().sysname + " " + os.uname().release,
            "arch": os.uname().machine,
        },
        "criterion_root": str(CRITERION_ROOT.relative_to(REPO_ROOT)),
        "corpus_filter": args.corpus,
        "benches": benches,
    }

    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"wrote {len(benches)} bench summaries → {out}", file=sys.stderr)


if __name__ == "__main__":
    main()
