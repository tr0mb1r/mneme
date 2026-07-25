#!/usr/bin/env python3
"""Gate the encryption-at-rest overhead as a *ratio*, not an absolute.

`benches/encryption_overhead.rs` runs each operation twice in the same
process — once over plain storage/WAL, once over the encrypted wrapper.
Comparing the two within one run makes the check machine-independent:
the ratio holds on an M-series laptop and on a shared CI runner, where
absolute nanoseconds do not. That matters because the frozen baseline in
`v0_2_6.json` was captured on Apple Silicon while `perf.yml` runs on
`ubuntu-latest`, so absolute comparisons across those two are already
approximate.

Ceilings are per bench group and generous, because the isolated AEAD
cost is a large *relative* multiple of an in-memory operation while
being a rounding error end-to-end. `encrypted_storage_get` goes from
~0.3 µs to ~2 µs — 7x, and completely invisible next to a 70 ms BGE-M3
forward pass. The end-to-end budget (spec §13: recall p95 < 50 ms,
remember p95 < 150 ms) is enforced by comparing the redb-backed benches
against the frozen baseline; this file exists to catch the crypto layer
itself getting dramatically slower, e.g. a nonce-generation regression
or an accidental per-record key derivation.

Usage:
  python3 benches/baselines/check_crypto_overhead.py [--baseline JSON]

With no arguments it reads the fresh `target/criterion` output via
`extract_baseline`. Exits 1 if any group exceeds its ceiling.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from extract_baseline import (  # noqa: E402
    CRITERION_ROOT,
    bench_label,
    find_new_dirs,
    summarize,
)

# group -> max acceptable encrypted/plain p95 ratio.
#
# Measured on a Linux CI-class VM with `--quick`: storage_put 5.1x,
# storage_get 7.4x, wal_append 1.02x, wal_replay 12.4x. Ceilings sit
# ~1.6x above those, wide enough to absorb runner noise on a
# sub-microsecond operation while still catching an order-of-magnitude
# regression. Tighten them once a full-sample baseline exists on the
# release hardware.
CEILINGS: dict[str, float] = {
    "encrypted_storage_put": 8.0,
    "encrypted_storage_get": 12.0,
    # I/O-dominated: fdatasync swamps the AEAD, so this must stay ~1x.
    # A jump here means encryption started costing a syscall.
    "encrypted_wal_append": 1.5,
    "encrypted_wal_replay": 20.0,
}

METRIC = "p95_ns"


def collect_from_criterion() -> dict[str, dict]:
    benches: dict[str, dict] = {}
    for new_dir in find_new_dirs(None):
        label = bench_label(new_dir)
        if label.split("/", 1)[0] in CEILINGS:
            benches[label] = summarize(new_dir)
    return benches


def collect_from_baseline(path: Path) -> dict[str, dict]:
    if not path.is_file():
        sys.exit(f"baseline file not found: {path}")
    data = json.loads(path.read_text())
    return {
        k: v
        for k, v in data.get("benches", {}).items()
        if k.split("/", 1)[0] in CEILINGS
    }


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--baseline",
        type=Path,
        default=None,
        help="Read an extract_baseline JSON instead of target/criterion.",
    )
    args = ap.parse_args()

    benches = (
        collect_from_baseline(args.baseline)
        if args.baseline
        else collect_from_criterion()
    )
    if not benches:
        src = args.baseline or CRITERION_ROOT
        sys.exit(
            f"no encryption_overhead benches found in {src} — "
            "run `cargo bench --bench encryption_overhead` first"
        )

    failures: list[str] = []
    missing: list[str] = []
    print(f"crypto overhead gate | metric={METRIC}")
    print()

    for group, ceiling in sorted(CEILINGS.items()):
        plain = benches.get(f"{group}/plain", {}).get(METRIC)
        enc = benches.get(f"{group}/encrypted", {}).get(METRIC)
        if plain is None or enc is None:
            missing.append(group)
            print(f"  ?  {group:28s} incomplete pair; skipped")
            continue
        if plain <= 0:
            missing.append(group)
            print(f"  ?  {group:28s} plain p95 was {plain}; skipped")
            continue
        ratio = enc / plain
        ok = ratio <= ceiling
        flag = "OK " if ok else "FAIL"
        print(
            f"  {flag} {group:28s} {plain / 1000:>9.2f} µs → "
            f"{enc / 1000:>9.2f} µs  ratio {ratio:5.2f}x (ceiling {ceiling:.2f}x)"
        )
        if not ok:
            failures.append(
                f"{group}: {ratio:.2f}x exceeds ceiling {ceiling:.2f}x"
            )

    print()
    if missing:
        # Loud, because a silently-absent pair would let a regression
        # through while the job still reported green.
        print(f"WARNING: no usable pair for: {', '.join(missing)}")
    if failures:
        print("FAIL: encryption overhead regressed:")
        for f in failures:
            print(f"  - {f}")
        print()
        print(
            "If the new cost is justified, update CEILINGS in this file in the "
            "same commit and say why in the PR body."
        )
        sys.exit(1)
    print("OK: encryption overhead within ceilings.")


if __name__ == "__main__":
    main()
