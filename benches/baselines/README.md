# Performance baselines

Frozen snapshots of mneme's hot-path latency, captured per release
candidate. Used as the reference point for the v1.1 regression gate
(release-planning v2.1 §6.2). Future runs compare against the
matching baseline; >10% slowdown in any p95 tier blocks merge unless
explicitly justified.

## What's captured

`v0_2_6.json` — the v1.0 release-candidate baseline, captured on
2026-05-09 against `develop` HEAD (which is identical to `main`
HEAD on hot-path code; the two diverging commits — backup `run/`
exclusion and `remember` description revision — touch neither
embedding nor storage paths). Captured on **Apple Silicon**. This is
still the blocking reference in `perf.yml`.

`v1_2_2_linux_x86_64.json` — median of 3 `--quick` runs on **x86_64
Linux**, the platform `perf.yml` actually runs on. Reported but *not*
enforced; see [Platform mismatch](#platform-mismatch) below.

## Platform mismatch

`perf.yml` runs on `ubuntu-latest` and gates against a baseline
captured on Apple Silicon. That is not a cosmetic difference:

| Bench | Apple Silicon p95 | x86_64 Linux p95 |
|---|---|---|
| `auto_context/no_query_n=1000` | 216.61 µs | ~244 µs |
| `remember/after_prefill_n=1000` | 4108.62 µs | ~856 µs |
| `recall/k=10_n=1000_pending` | 167.32 µs | ~138 µs |

`auto_context/no_query` therefore sits permanently within a few points
of the 10 % failure threshold for reasons unrelated to any diff under
test, while `remember` has ~4.8x of slack and would hide a real
regression completely.

**Runner noise compounds it.** Three consecutive runs of *identical*
code on one Linux VM produced 233, 236 and 422 µs for
`auto_context/no_query` — an 80 % swing from a co-tenant stall. Any
single-run capture is an unsuitable reference, which is why
`merge_baselines.py` exists.

### Promotion procedure

To make the gate meaningful, the reference must come from the same
runner class that enforces it:

1. Let `perf.yml` run on `main` a few times and read the
   "informational compare" step's output. Confirm the deltas against
   `v1_2_2_linux_x86_64.json` stay inside ±10 % across runs.
2. If they do, make it the blocking reference: point the "compare
   against frozen baseline" step at `v1_2_2_linux_x86_64.json` and
   drop the informational step.
3. If they don't, re-capture on a GitHub runner (a `workflow_dispatch`
   job that runs the capture procedure below three times and uploads
   the merged JSON as an artifact) and commit that instead.

Keep `v0_2_6.json` either way — it is the historical record of the
v1.0 numbers and the reference for manual release-hardware sweeps.

Per bench, the JSON records:

- `samples` — criterion sample count (50 for hot benches, 20 for
  cold-start variants).
- `min_ns`, `max_ns`, `p50_ns`, `p95_ns`, `p99_ns` — percentiles
  over per-iteration latency, computed from `target/criterion/<group>/<id>/new/sample.json`.
- `mean_ns`, `median_ns`, `stddev_ns` — criterion's own estimates.

All times are in nanoseconds; divide by 1000 for µs, by 1e6 for ms.

## What's NOT captured

- **Real-embedder latency.** Every bench uses `StubEmbedder` so it
  measures storage + WAL + HNSW in isolation. BGE-M3 forward-pass
  latency is a model question, not an architecture question, and
  dominates real-world `remember`/`recall` timings (per spec §13,
  the embedder gets roughly half of each end-to-end budget).
  Real-model numbers belong in a separate manual pre-release sweep
  (§13 of the implementation plan); this baseline is for catching
  storage/HNSW/WAL regressions specifically.
- **Memory footprint.** Criterion measures wall-clock; RSS at
  various corpus sizes needs a separate harness. Deferred to Q3
  (release-planning §6.2 task #18) when the CI regression gate
  scaffolding lands.
- **Corpora beyond `MNEME_BENCH_N=1000`.** The default 1k-memory
  corpus is the canonical CI baseline. Larger corpora (10k, 100k)
  take 5–10 minutes per bench; capture them manually before each
  release per the procedure below.

## Capture procedure

Capture at least three runs and merge them — see
[Platform mismatch](#platform-mismatch) for why one run is not enough.

```bash
# 1-3. Three independent runs, each into its own JSON.
for i in 1 2 3; do
  # Clean criterion's stale output so the JSON only contains the
  # canonical sample count for each bench id.
  rm -rf target/criterion/
  cargo bench --bench remember --bench recall \
              --bench cold_start --bench auto_context \
              --bench encryption_overhead
  python3 benches/baselines/extract_baseline.py \
          /tmp/perf/run$i.json --corpus 1000
done

# 4. Median-merge into the committed baseline.
python3 benches/baselines/merge_baselines.py \
        benches/baselines/<release>.json \
        /tmp/perf/run1.json /tmp/perf/run2.json /tmp/perf/run3.json

# 5. (Optional pre-release) Capture larger corpora alongside.
MNEME_BENCH_N=10000 cargo bench --bench recall --bench auto_context
python3 benches/baselines/extract_baseline.py \
        benches/baselines/<release>_n10k.json --corpus 10000
```

Match the `--quick` flag to whatever `perf.yml` uses, or the sample
counts won't be comparable.

Include `--bench encryption_overhead`: its ids carry no `n=` marker, so
`--corpus` keeps them (corpus-independent benches are never filtered
out — that behaviour is why the crypto bench was invisible to CI
through all of v1.2).

The extractor records the git branch + sha + describe in the JSON's
`git` block so the baseline is traceable to a specific commit. Re-run
after every measurable change to the storage seam, the embedder
loader, or the orchestrator.

## Comparison

```bash
# Crude diff: percent slowdown vs baseline, per bench.
python3 - <<'PY'
import json
base = json.load(open("benches/baselines/v0_2_6.json"))["benches"]
new  = json.load(open("benches/baselines/<new>.json"))["benches"]
for k, b in base.items():
    n = new.get(k)
    if not n:
        continue
    d = (n["p95_ns"] - b["p95_ns"]) / b["p95_ns"] * 100
    flag = "🔴" if d > 10 else "🟢"
    print(f"{flag} {k:50s} p95 Δ {d:+6.1f}%")
PY
```

**`benches/baselines/compare.py` is the canonical comparator** —
walks two extract-baseline JSONs, prints per-bench p95 deltas,
exits 1 if any regresses beyond the tolerance (default 10 %).
Used by `.github/workflows/perf.yml` as the v1.1 release gate
(release-planning §6.2). Local invocation:

```sh
python3 benches/baselines/compare.py \
    benches/baselines/v0_2_6.json \
    benches/baselines/<your-fresh>.json
```

Flags:

- `--tolerance-pct N` — slowdown threshold in percent (default 10).
- `--metric KEY` — metric to compare; defaults to `p95_ns`. Useful
  values: `p50_ns`, `p99_ns`, `mean_ns`.
- `--include PATTERN` / `--exclude PATTERN` — substring filter on
  bench keys; can be repeated.

Exit codes: `0` on hold, `1` on regression, `1` on missing/invalid
input. The comparator is intentionally Python (not Rust) so it can
run in CI without rebuilding the workspace and so the threshold
logic stays trivial to audit.

Benches present on only one side are reported under "benches added
since baseline" / "benches removed since baseline" rather than silently
skipped, so a shrinking comparison set is visible.

## Encryption overhead

`encryption_overhead` is gated differently, by
**`check_crypto_overhead.py`**. The bench measures each operation twice
in one process — plain wrapper vs encrypted wrapper — so the *ratio*
between them is machine-independent even though the absolute numbers are
not. That makes it the one gate that transfers cleanly between the
capture host and CI.

```sh
# Against fresh target/criterion output:
python3 benches/baselines/check_crypto_overhead.py

# Or against an already-extracted baseline JSON:
python3 benches/baselines/check_crypto_overhead.py --baseline /tmp/perf/this-run.json
```

Per-group ceilings live in `CEILINGS` at the top of that script.
They are deliberately loose (5-20x) because the isolated AEAD cost is a
large *relative* multiple of a sub-microsecond in-memory operation while
being invisible end-to-end — `encrypted_storage_get` goes from ~0.30 µs
to ~2.16 µs next to a ~70 ms BGE-M3 forward pass. The ceilings exist to
catch the crypto layer itself becoming dramatically slower (a
per-record key derivation creeping in, a syscall appearing on the AEAD
path), not to police microseconds.

Note what the ratios say about where encryption actually costs
something: `encrypted_wal_append` is 1.02x because `fdatasync`
dominates, while `encrypted_wal_replay` is ~12x because replay is pure
CPU with no I/O to hide behind. Cold-start time on an encrypted dir is
the number to watch.

## v0.2.6 reference numbers

Captured 2026-05-09 on Apple Silicon (arm64, Darwin 25.4.0). All
values are p95 in microseconds; see `v0_2_6.json` for the raw data
including p50/p99/min/max.

| Bench | p50 µs | p95 µs | p99 µs |
|---|---|---|---|
| `recall/k=10_n=1000_pending` | 164.75 | 167.32 | 168.58 |
| `recall/k=10_n=1000_committed` | 165.32 | 166.99 | 167.44 |
| `auto_context/no_query_n=1000` | 215.51 | 216.61 | 218.15 |
| `auto_context/with_query_n=1000` | 398.21 | 399.41 | 399.64 |
| `cold_start/from_snapshot_n=1000` | 1449.27 | 1545.74 | 2141.79 |
| `cold_start/from_wal_replay_n=1000` | 1443.53 | 1514.76 | 1551.86 |
| `remember/after_prefill_n=1000` | 3922.90 | 4108.62 | 4455.63 |

All well under spec §13 budgets — the storage path has substantial
headroom. v1.1's daemon work (network hop + auth check + multi-client
coordination) consumes some of this headroom; the regression gate is
the budget on how much.

## v1.2.2 x86_64 Linux reference numbers

Median of 3 `--quick` runs, 2026-07, Linux 6.18.5 / x86_64. Same
platform family as `perf.yml`'s runner. p95, microseconds.

| Bench | p95 µs |
|---|---|
| `recall/k=10_n=1000_pending` | 138.12 |
| `recall/k=10_n=1000_committed` | 160.76 |
| `auto_context/no_query_n=1000` | 244.20 |
| `auto_context/with_query_n=1000` | 415.85 |
| `remember/after_prefill_n=1000` | 855.79 |
| `cold_start/from_wal_replay_n=1000` | 1562.31 |
| `cold_start/from_snapshot_n=1000` | 1628.20 |
| `encrypted_storage_get/plain` → `/encrypted` | 0.30 → 2.16 |
| `encrypted_storage_put/plain` → `/encrypted` | 0.57 → 2.71 |
| `encrypted_wal_append/plain` → `/encrypted` | 741.26 → 862.52 |
| `encrypted_wal_replay/plain` → `/encrypted` | 174.21 → 2160.51 |

`recall` here includes the v1.3 widening loop
(`SemanticStore::recall`), which replaced a fixed `k × 4` index probe
with a geometric widen. Unfiltered recall still issues exactly one
probe, so the numbers moved *down* relative to v0.2.6 on the same
hardware rather than up — the loop costs nothing when the first probe
fills `k`.
