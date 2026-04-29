# Configuration

`mneme init` writes a `~/.mneme/config.toml` with every default made
explicit. Edit it before the first `mneme run` to override anything.

## Quick reference

```toml
data_dir = "~/.mneme"
max_size_gb = 10

[embeddings]
model  = "bge-m3"     # or "minilm-l6" — see installation chapter for trade-offs
device = "auto"       # "cpu" / "cuda" / "metal"
batch_size = 32

[scopes]
default = "personal"  # used by `remember` / `pin` when --scope is omitted

[checkpoints]
session_interval_secs = 30   # L1 working-session checkpoint cadence
session_interval_turns = 5   # whichever fires first
hnsw_snapshot_inserts = 1000 # rebuild HNSW snapshot every N inserts
hnsw_snapshot_minutes = 60   # ...or every N minutes, whichever first

[consolidation]
hot_to_warm_days = 28        # L3 hot → warm threshold
warm_to_cold_days = 180      # L3 warm → cold threshold
schedule = "idle"            # only "idle" supported in v1.0

[budgets]
default_recall_limit = 10
auto_context_token_budget = 4000

[mcp]
transport = "stdio"          # only "stdio" supported in v1.0

[telemetry]
enabled = false              # off by default; opt-in only
endpoint = ""

[logging]
level = "info"
file  = "~/.mneme/logs/mneme.log"
max_size_mb = 100
max_files = 5
```

## Environment overrides

| Variable | What it does |
|----------|--------------|
| `MNEME_DATA_DIR` | Override `data_dir`. Per-project isolation pattern — set in `.envrc` so each project gets its own palace. |
| `MNEME_EMBEDDER` | Set to `stub` to swap in the deterministic stub embedder. Used by tests + offline CI; logs a loud `WARN` so it's never accidentally left on in production. |

## Switching embedders later

Change `[embeddings] model = "..."` in `config.toml` and run
`mneme run`. On boot, `embed::migrate::migrate_if_needed` notices the
embedder identity changed, re-embeds every L4 memory under the new
model through the WAL, and writes a new HNSW snapshot. If you have
10K memories under MiniLM, expect a few minutes of one-time work; the
log line is loud (`re-embedded memories under new embedder identity`).
After that, recall uses the new model.
