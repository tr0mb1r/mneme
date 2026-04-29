#!/usr/bin/env bash
# From-source installer for mneme.
#
# Builds the release binary, drops it on $PATH, scaffolds the data
# directory, and prints next steps for MCP host registration. Aimed
# at users who don't yet have a Homebrew formula or prebuilt binary
# (i.e., everyone, until the v1.0 release pipeline ships).
#
# Usage:
#   scripts/install.sh                       # default install
#   scripts/install.sh --prefix ~/.local/bin # override install dir
#   scripts/install.sh --no-init             # skip `mneme init`
#   scripts/install.sh --minilm              # default to MiniLM (~80 MB) instead of BGE-M3 (~1.5 GB)
#   scripts/install.sh --help                # show this banner
#
# Idempotent: safe to re-run. Prints (does not modify) any
# Claude Code / Claude Desktop registration hints — wiring the MCP
# host is left to the user.

set -euo pipefail

# ---------- args ----------

PREFIX=""
RUN_INIT=1
USE_MINILM=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            PREFIX="${2:?--prefix requires a directory}"; shift 2 ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"; shift ;;
        --no-init)
            RUN_INIT=0; shift ;;
        --minilm)
            USE_MINILM=1; shift ;;
        -h|--help)
            sed -n '2,19p' "$0" | sed -E 's/^# ?//'; exit 0 ;;
        *)
            echo "WARNING: ignoring unknown argument: $1" >&2; shift ;;
    esac
done

# ---------- repo root + cargo ----------

# Locate the repo root by walking up from the script's directory
# until we find Cargo.toml. Allows the script to be invoked from
# anywhere (`bash scripts/install.sh`, `./install.sh`, etc.).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"
while [ "$REPO_ROOT" != "/" ] && [ ! -f "$REPO_ROOT/Cargo.toml" ]; do
    REPO_ROOT="$(dirname "$REPO_ROOT")"
done
if [ ! -f "$REPO_ROOT/Cargo.toml" ]; then
    echo "ERROR: could not find Cargo.toml — run this from inside the mneme repo." >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found on \$PATH." >&2
    echo "  Install the Rust toolchain first: https://rustup.rs" >&2
    exit 1
fi

# ---------- pick install dir ----------

# Heuristic for $PREFIX, in order:
#   1. --prefix <dir> if given
#   2. ~/.local/bin if it exists OR can be created
#   3. /usr/local/bin if writable without sudo
#   4. otherwise bail and tell the user to pick one
if [ -z "$PREFIX" ]; then
    LOCAL_BIN="$HOME/.local/bin"
    if mkdir -p "$LOCAL_BIN" 2>/dev/null && [ -w "$LOCAL_BIN" ]; then
        PREFIX="$LOCAL_BIN"
    elif [ -w "/usr/local/bin" ]; then
        PREFIX="/usr/local/bin"
    else
        echo "ERROR: no writable install directory found." >&2
        echo "  Tried ~/.local/bin and /usr/local/bin." >&2
        echo "  Re-run with --prefix <dir> to pick one explicitly." >&2
        exit 1
    fi
fi
mkdir -p "$PREFIX"

echo "==> mneme install"
echo "    repo:   $REPO_ROOT"
echo "    prefix: $PREFIX"

# ---------- build ----------

echo "==> cargo build --release (this takes ~15 min on a cold build because of candle)"
( cd "$REPO_ROOT" && cargo build --release )

BIN_SRC="$REPO_ROOT/target/release/mneme"
BIN_DST="$PREFIX/mneme"
if [ ! -x "$BIN_SRC" ]; then
    echo "ERROR: build succeeded but $BIN_SRC is not executable. Aborting." >&2
    exit 1
fi

echo "==> install binary"
install -m 0755 "$BIN_SRC" "$BIN_DST"

# ---------- $PATH check ----------

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        echo "WARNING: $PREFIX is not on \$PATH."
        echo "  Add to your shell rc (~/.bashrc, ~/.zshrc, etc.):"
        echo "    export PATH=\"\$PATH:$PREFIX\""
        ;;
esac

# ---------- init ----------

if [ "$RUN_INIT" -eq 1 ]; then
    if [ -d "$HOME/.mneme" ] && [ -f "$HOME/.mneme/config.toml" ]; then
        echo "==> ~/.mneme already scaffolded — skipping mneme init"
    else
        echo "==> mneme init"
        "$BIN_DST" init
    fi
fi

# ---------- offer the MiniLM swap before any model download ----------

CONFIG="$HOME/.mneme/config.toml"
if [ "$USE_MINILM" -eq 1 ] && [ -f "$CONFIG" ]; then
    if grep -q '^model = "bge-m3"' "$CONFIG"; then
        echo "==> switching default embedder bge-m3 -> minilm-l6 (per --minilm)"
        # Portable in-place edit: write a sibling and rename.
        tmp="$CONFIG.tmp"
        sed 's/^model = "bge-m3"/model = "minilm-l6"/' "$CONFIG" > "$tmp"
        mv "$tmp" "$CONFIG"
    fi
fi

# ---------- next steps ----------

cat <<NEXT

==> mneme installed.

Next steps:

1. Confirm it runs:

     mneme --help
     mneme stats

2. Wire it into your MCP host. For Claude Code (recommended path):

     claude mcp add --scope user mneme "$(command -v mneme || echo "$BIN_DST")" run

   Then restart Claude Code, type "/mcp", and you should see mneme listed.

   For Claude Desktop, add to claude_desktop_config.json:
     {"mcpServers": {"mneme": {"command": "$BIN_DST", "args": ["run"]}}}

3. (Optional) tighten the embedder. Default is BGE-M3 (~1.5 GB,
   multilingual, top-tier recall). Edit ~/.mneme/config.toml and set
   [embeddings] model = "minilm-l6" for a ~80 MB / faster-startup
   embedder, then re-run \`mneme run\`. Existing memories re-embed
   automatically on the model swap.

4. (Optional) add the Claude Code lifecycle hooks for deterministic
   load-and-save loops. See docs/CLAUDE_CODE_SETUP.md §7.

For end-to-end verification of the install:

     scripts/manual_test.sh --stub       # offline, ~10s, no model download

NEXT
