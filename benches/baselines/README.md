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
embedding nor storage paths).

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

```bash
# 1. Clean criterion's stale output so the JSON only contains the
#    canonical sample count for each bench id.
rm -rf target/criterion/

# 2. Run the four hot-path benches at the default corpus.
cargo bench --bench remember --bench recall \
            --bench cold_start --bench auto_context

# 3. Extract the n=1000 baseline JSON.
python3 benches/baselines/extract_baseline.py \
        benches/baselines/<release>.json --corpus 1000

# 4. (Optional pre-release) Capture larger corpora alongside.
MNEME_BENCH_N=10000 cargo bench --bench recall --bench auto_context
python3 benches/baselines/extract_baseline.py \
        benches/baselines/<release>_n10k.json --corpus 10000
```

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

The Q3 / task #18 implementation will replace this with a CI-runnable
comparator that fails the build on any p95 regression > 10% (per
release-planning §6.2 "regression alerts (>10% slowdown in any tier)
block merge unless explicitly justified").

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
