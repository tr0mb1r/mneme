# Installation

Three install paths, each producing the same `mneme` binary on your
`$PATH`. Pick the one that matches your environment.

## 1. Homebrew (recommended for macOS / Linux)

```sh
brew tap tr0mb1r/mneme
brew install mneme
```

Pre-built static binary — no Rust toolchain, no compile time.
Available for Apple Silicon, Intel macOS, and aarch64 / x86_64 Linux
(musl-static, glibc-portable). Windows is not distributed via
Homebrew; use [`cargo install`](#2-cargo-install) or
[from source](#3-from-source) instead.

Upgrades:

```sh
brew update
brew upgrade mneme
```

## 2. `cargo install`

```sh
cargo install mneme-mcp
```

Builds and installs from source via crates.io. Requires Rust stable
(via [rustup](https://rustup.rs) or your distro's package manager).

The crate is `mneme-mcp` on crates.io — the bare `mneme` name is
held by an unrelated event-sourcing library. The installed binary
is `mneme` regardless: every CLI command (`mneme run`,
`mneme stats`, …) and every MCP integration works the same way no
matter which install path you took.

Cargo installs to `~/.cargo/bin/` by default; make sure that's on
your `$PATH` (`rustup` does this for you).

Upgrades:

```sh
cargo install mneme-mcp   # cargo-install upgrades in place
```

## 3. From source

For contributors, Windows users, or anyone who wants to pin a
specific revision. The bundled installer is the fastest path to a
working install:

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
scripts/install.sh             # build, install on $PATH, scaffold ~/.mneme
scripts/install.sh --minilm    # same, but default to MiniLM (~80 MB)
                               # instead of BGE-M3 (~1.5 GB)
```

`scripts/install.sh` picks `~/.local/bin` (or `/usr/local/bin` if
writable), runs `cargo build --release`, copies the binary, runs
`mneme init`, and prints the exact `claude mcp add` line for the
next step. Idempotent — safe to re-run when you pull. Pass
`--prefix <dir>` to install elsewhere or `--no-init` to skip the
data-directory scaffold.

If you'd rather drive each step yourself:

```sh
git clone https://github.com/tr0mb1r/mneme && cd mneme
cargo build --release
cp target/release/mneme ~/.local/bin/   # or anywhere on $PATH
mneme init                               # scaffolds ~/.mneme and pulls
                                         # the embedding model
```

## After installing — first run

`mneme init` writes `~/.mneme/config.toml` with all defaults made
explicit. Both Homebrew and `cargo install` skip this step by
design (formula and `cargo install` should not modify `$HOME`); run
it manually if you went either route:

```sh
mneme init
```

The `from-source` installer runs `init` for you.

The first `mneme run` downloads the embedding model:

| Model | Size | Speed | Recall | When to pick |
|-------|------|-------|--------|--------------|
| `bge-m3` (default) | ~1.5 GB | slower cold start | top-tier, multilingual | Best recall, multilingual sources, willing to wait on first boot. |
| `minilm-l6` | ~80 MB | sub-second cold start | good for English | Fast onboarding, English-only is fine, or testing before committing to BGE-M3. |

Edit `~/.mneme/config.toml` and set `[embeddings] model` before the
first `mneme run` if you want to override the default. Switching
models later re-embeds every stored memory automatically; no
manual reindex.

## Requirements

| Install path | Toolchain | Disk |
|---|---|---|
| Homebrew | none (binary release) | ~150 MB MiniLM / ~2 GB BGE-M3 (model cache) |
| `cargo install` | Rust stable | + Cargo build cache |
| From source | Rust stable + `git` | + Cargo build cache |

## Verifying the install

```sh
mneme --help
mneme stats   # prints zeros if the data dir is empty; confirms it's intact
```

For a comprehensive end-to-end check exercising every tool plus
backup / restore round-trips (requires the source clone — install
paths 1 and 2 don't ship `scripts/manual_test.sh`):

```sh
scripts/manual_test.sh --stub   # offline, ~10 s, no model download
scripts/manual_test.sh          # real MiniLM, exercises the embedder
```
