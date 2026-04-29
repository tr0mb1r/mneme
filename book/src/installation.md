# Installation

There is no `brew install mneme` yet. Until the release pipeline lands,
build from source. The fastest path is the bundled installer:

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
scripts/install.sh             # build, install on $PATH, scaffold ~/.mneme
scripts/install.sh --minilm    # same, but default to MiniLM (~80 MB)
                               # instead of BGE-M3 (~1.5 GB)
```

`scripts/install.sh` picks `~/.local/bin` (or `/usr/local/bin` if
writable), runs `cargo build --release`, copies the binary, runs
`mneme init`, and prints the exact `claude mcp add` line for the next
step. It's idempotent — safe to re-run when you pull. Pass
`--prefix <dir>` to install elsewhere or `--no-init` to skip the
data-directory scaffold.

## Manual build

If you'd rather drive each step yourself:

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
cargo build --release
cp target/release/mneme ~/.local/bin/   # or anywhere on $PATH
mneme init                               # scaffolds ~/.mneme and pulls
                                         # the embedding model
```

`mneme init` writes `~/.mneme/config.toml` with all defaults made
explicit; edit it before first run if you want to override the
embedding model, data directory, or storage budget. The first
`mneme run` downloads the embedding model:

| Model | Size | Speed | Recall | When to pick |
|-------|------|-------|--------|--------------|
| `bge-m3` (default) | ~1.5 GB | slower cold start | top-tier, multilingual | Best recall, multilingual sources, willing to wait on first boot. |
| `minilm-l6` | ~80 MB | sub-second cold start | good for English | Fast onboarding, English-only is fine, or testing before committing to BGE-M3. |

Switching models later re-embeds every stored memory automatically; no
manual reindex.

## Requirements

- Rust stable (pinned via `rust-toolchain.toml`)
- ~2 GB free disk (BGE-M3 model + Cargo build cache); ~150 MB for the
  MiniLM path

## Verifying the install

```sh
mneme --help
mneme stats   # prints zeros; confirms the data dir is intact
```

For a comprehensive end-to-end check exercising every tool plus
backup/restore round-trips:

```sh
scripts/manual_test.sh --stub   # offline, ~10 s, no model download
scripts/manual_test.sh          # real MiniLM, exercises the embedder
```
