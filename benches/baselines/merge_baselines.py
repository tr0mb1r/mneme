#!/usr/bin/env python3
"""Merge several extract_baseline runs into one median baseline.

A single criterion run on a shared CI runner is not a stable reference.
Measured here on a Linux VM, `auto_context/no_query` came back at 233,
236 and 422 µs across three consecutive runs of *identical* code — an
80 % swing. Freezing any one of those as the baseline either makes the
gate trivially passable or guarantees false failures.

So capture N runs and take the per-bench median of each metric. The
median is robust to the occasional co-tenant stall in a way the mean is
not, and it needs no outlier-rejection heuristic to argue about.

Usage:
  python3 benches/baselines/merge_baselines.py OUT.json IN1.json IN2.json ...

Benches missing from some inputs are still emitted, with `runs`
recording how many contributed — a bench measured once is visible as
such rather than silently looking as solid as one measured five times.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path

# Metrics merged by median. Anything else in a bench entry is taken from
# the first input that has the bench (they are run-invariant labels).
NUMERIC_METRICS = (
    "min_ns",
    "max_ns",
    "p50_ns",
    "p95_ns",
    "p99_ns",
    "mean_ns",
    "median_ns",
    "stddev_ns",
    "samples",
)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("output", type=Path)
    ap.add_argument("inputs", type=Path, nargs="+")
    args = ap.parse_args()

    if len(args.inputs) < 2:
        print(
            "warning: merging fewer than 2 runs — the result is no more "
            "robust than the single input it came from",
            file=sys.stderr,
        )

    payloads = []
    for path in args.inputs:
        if not path.is_file():
            sys.exit(f"input not found: {path}")
        payloads.append(json.loads(path.read_text()))

    merged: dict[str, dict] = {}
    all_keys = sorted({k for p in payloads for k in p.get("benches", {})})
    for key in all_keys:
        entries = [p["benches"][key] for p in payloads if key in p.get("benches", {})]
        out: dict = {"runs": len(entries)}
        for metric in NUMERIC_METRICS:
            values = [
                e[metric]
                for e in entries
                if isinstance(e.get(metric), (int, float))
            ]
            if values:
                out[metric] = statistics.median(values)
        merged[key] = out

    first = payloads[0]
    payload = {
        "captured_at": first.get("captured_at"),
        "git": first.get("git"),
        "host": first.get("host"),
        "corpus_filter": first.get("corpus_filter"),
        "merged_from": [str(p) for p in args.inputs],
        "merge_strategy": "per-metric median across runs",
        "benches": merged,
    }
    args.output.write_text(json.dumps(payload, indent=2) + "\n")

    single = [k for k, v in merged.items() if v["runs"] < len(payloads)]
    print(
        f"merged {len(payloads)} run(s) → {len(merged)} bench summaries "
        f"→ {args.output}"
    )
    if single:
        print(f"note: measured in fewer than all runs: {', '.join(single)}")


if __name__ == "__main__":
    main()
