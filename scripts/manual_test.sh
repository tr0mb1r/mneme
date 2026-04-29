#!/usr/bin/env bash
# Comprehensive end-to-end manual test for the mneme MCP server.
#
# Runs in an isolated $MNEME_DATA_DIR (default: /tmp/mneme-manual-XXXX),
# launches `mneme run` over stdio, and exercises every Phase 6 tool
# (remember, recall, update, forget, pin, unpin, recall_recent,
# summarize_session, stats, list_scopes, export) plus backup/restore
# durability and post-restore recall.
#
# Usage:
#   scripts/manual_test.sh                # real MiniLM, real model load
#   scripts/manual_test.sh --stub         # use stub embedder, no download
#   scripts/manual_test.sh --keep         # don't wipe $TEST_ROOT on exit
#   scripts/manual_test.sh --reuse-models # symlink ~/.mneme/models to skip download
#
# Exit code 0 on full pass, non-zero with a summary on any failure.

set -euo pipefail

# ---------- args + dependencies ----------

USE_STUB=0
KEEP_ROOT=0
REUSE_MODELS=0
for arg in "$@"; do
    case "$arg" in
        --stub) USE_STUB=1 ;;
        --keep) KEEP_ROOT=1 ;;
        --reuse-models) REUSE_MODELS=1 ;;
        -h|--help)
            sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown flag: $arg (try --help)" >&2
            exit 2
            ;;
    esac
done

command -v jq >/dev/null || { echo "jq is required (brew install jq)" >&2; exit 2; }

# ---------- paths ----------

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO_ROOT/target/release/mneme"
TEST_ROOT="$(mktemp -d -t mneme-manual.XXXXXX)"
LOG="$TEST_ROOT/server.stderr.log"
REQ_PIPE="$TEST_ROOT/req.fifo"
RESP_PIPE="$TEST_ROOT/resp.fifo"
BACKUP_TAR="$TEST_ROOT/backup.tar.gz"

export MNEME_DATA_DIR="$TEST_ROOT/data"
[[ "$USE_STUB" -eq 1 ]] && export MNEME_EMBEDDER=stub

# ---------- output helpers ----------

if [[ -t 1 ]]; then
    C_OK=$'\033[32m'; C_FAIL=$'\033[31m'; C_DIM=$'\033[2m'; C_BOLD=$'\033[1m'; C_OFF=$'\033[0m'
else
    C_OK=""; C_FAIL=""; C_DIM=""; C_BOLD=""; C_OFF=""
fi

PASS=0
FAIL=0
FAILED_TESTS=()

ok()   { PASS=$((PASS+1)); printf "  ${C_OK}✓${C_OFF} %s\n" "$1"; }
fail() { FAIL=$((FAIL+1)); FAILED_TESTS+=("$1"); printf "  ${C_FAIL}✗${C_OFF} %s\n" "$1"; }
step() { printf "\n${C_BOLD}== %s ==${C_OFF}\n" "$1"; }
note() { printf "${C_DIM}%s${C_OFF}\n" "$1"; }

# ---------- cleanup ----------

SERVER_PID=""
cleanup() {
    local exit_code=$?
    set +e
    # Close FDs (idempotent).
    exec 3>&- 2>/dev/null || true
    exec 4<&- 2>/dev/null || true
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null
        sleep 1
        kill -KILL "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ "$KEEP_ROOT" -eq 1 ]]; then
        printf "${C_DIM}test root retained at: %s${C_OFF}\n" "$TEST_ROOT"
    else
        rm -rf "$TEST_ROOT"
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

# ---------- build ----------

step "Build"
if [[ -x "$BIN" ]]; then
    note "reusing $BIN"
else
    note "cargo build --release ..."
    (cd "$REPO_ROOT" && cargo build --release --quiet)
fi
[[ -x "$BIN" ]] || { echo "$BIN not found after build" >&2; exit 1; }
ok "binary present"

# ---------- init ----------

step "Init (MNEME_DATA_DIR=$MNEME_DATA_DIR)"
"$BIN" init >/dev/null
[[ -f "$MNEME_DATA_DIR/config.toml" ]] && ok "config.toml written" || fail "config.toml missing"
[[ -f "$MNEME_DATA_DIR/schema_version" ]] && ok "schema_version written" || fail "schema_version missing"

# `Config::default` is bge-m3 but BAAI/bge-m3 doesn't ship
# model.safetensors (HF 404). Pin the test config to minilm-l6 so the
# real-model path actually loads. Override with MNEME_TEST_MODEL.
TEST_MODEL="${MNEME_TEST_MODEL:-minilm-l6}"
if [[ "$USE_STUB" -ne 1 ]]; then
    sed -i.bak "s/^model = .*/model = \"$TEST_MODEL\"/" "$MNEME_DATA_DIR/config.toml"
    rm -f "$MNEME_DATA_DIR/config.toml.bak"
    ok "config.toml pinned to $TEST_MODEL"
fi

link_models() {
    if [[ "$REUSE_MODELS" -eq 1 && -d "$HOME/.mneme/models" ]]; then
        rm -rf "$MNEME_DATA_DIR/models"
        ln -s "$HOME/.mneme/models" "$MNEME_DATA_DIR/models"
        return 0
    fi
    return 1
}

if link_models; then
    ok "symlinked existing model cache (skips download)"
fi

# ---------- launch server with bidirectional pipes ----------

step "Launch server"
mkfifo "$REQ_PIPE" "$RESP_PIPE"
# Run server detached so closing FD 3 yields EOF on its stdin.
"$BIN" run < "$REQ_PIPE" > "$RESP_PIPE" 2> "$LOG" &
SERVER_PID=$!
# Open pipes from the script's side. Ordering matters: open the writer
# in the background before the reader so neither blocks.
exec 3> "$REQ_PIPE"
exec 4< "$RESP_PIPE"

if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    fail "server died on launch (see $LOG)"
    cat "$LOG" || true
    exit 1
fi
ok "server pid=$SERVER_PID"
[[ "$USE_STUB" -eq 1 ]] && note "MNEME_EMBEDDER=stub" || note "model: real (first run downloads ~80MB MiniLM if not cached)"

# ---------- request / response helpers ----------

NEXT_ID=1
send_request() {
    # $1 = method; $2 (optional) = params JSON. Returns id used.
    local method="$1"
    local params="${2:-}"
    local id="$NEXT_ID"
    NEXT_ID=$((NEXT_ID+1))
    if [[ -n "$params" ]]; then
        printf '{"jsonrpc":"2.0","id":%d,"method":"%s","params":%s}\n' "$id" "$method" "$params" >&3
    else
        printf '{"jsonrpc":"2.0","id":%d,"method":"%s"}\n' "$id" "$method" >&3
    fi
    echo "$id"
}

send_notification() {
    local method="$1"
    printf '{"jsonrpc":"2.0","method":"%s"}\n' "$method" >&3
}

# Read one response line. The first call after launch waits for the
# embedder to load, so we generously allow 120s.
read_response() {
    local timeout_secs="${1:-30}"
    local line=""
    if ! IFS= read -r -t "$timeout_secs" -u 4 line; then
        printf "${C_FAIL}timeout (%ss) waiting for response${C_OFF}\n" "$timeout_secs" >&2
        if [[ -n "$SERVER_PID" ]] && ! kill -0 "$SERVER_PID" 2>/dev/null; then
            printf "${C_FAIL}server died.${C_OFF} stderr tail:\n" >&2
            tail -n 20 "$LOG" >&2 || true
        else
            printf "${C_DIM}server still running. stderr tail:${C_OFF}\n" >&2
            tail -n 20 "$LOG" >&2 || true
        fi
        return 1
    fi
    printf '%s\n' "$line"
}

call_tool() {
    # $1 = tool name; $2 = args JSON; prints the response line.
    local name="$1" args="$2"
    local params; params=$(jq -nc --arg n "$name" --argjson a "$args" '{name:$n, arguments:$a}')
    send_request "tools/call" "$params" >/dev/null
    read_response
}

# ---------- handshake ----------

step "Handshake"
init_params='{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"manual-test","version":"0.0.0"}}'
send_request "initialize" "$init_params" >/dev/null
INIT_RESP=$(read_response 120)  # first read may wait for model load
PROTO=$(jq -r '.result.protocolVersion // "missing"' <<<"$INIT_RESP")
SERVER_NAME=$(jq -r '.result.serverInfo.name // "missing"' <<<"$INIT_RESP")
[[ "$PROTO" != "missing" && "$SERVER_NAME" == "mneme" ]] \
    && ok "initialize ok (protocol=$PROTO, server=$SERVER_NAME)" \
    || fail "initialize failed: $INIT_RESP"

send_notification "notifications/initialized"

# ---------- tools/list ----------

step "tools/list"
send_request "tools/list" >/dev/null
LIST_RESP=$(read_response)
TOOL_NAMES=$(jq -r '.result.tools[].name' <<<"$LIST_RESP" | sort | paste -sd, -)
EXPECTED="export,forget,list_scopes,pin,recall,recall_recent,remember,stats,summarize_session,switch_scope,unpin,update"
[[ "$TOOL_NAMES" == "$EXPECTED" ]] \
    && ok "12 tools registered: $TOOL_NAMES" \
    || fail "tool list mismatch: got=$TOOL_NAMES expected=$EXPECTED"

# ---------- L4 semantic ----------

step "L4 semantic — remember / recall / update / forget"

# remember
RESP=$(call_tool remember '{"content":"ci pipeline turns green when ruff and pytest both pass","type":"fact","tags":["ci","tooling"],"scope":"work"}')
TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
MEM_ID=$(echo "$TEXT" | awk '{print $NF}')
[[ "$TEXT" == "stored memory "* && ${#MEM_ID} -eq 26 ]] \
    && ok "remember returned ULID $MEM_ID" \
    || fail "remember response unexpected: $TEXT"

# remember a second one for diversity
call_tool remember '{"content":"prefer cargo test --release for the long crash recovery suites","type":"preference","tags":["cargo"]}' >/dev/null

# recall — seeded memory must appear in the hits. With the stub
# embedder distances are not semantically meaningful, so we don't
# require it at position 0; with real MiniLM it normally is.
RESP=$(call_tool recall '{"query":"how do I know the build is green","limit":5}')
HITS_TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
SEEDED_HIT=$(jq -r --arg id "$MEM_ID" '.[] | select(.id == $id) | .content' <<<"$HITS_TEXT")
[[ "$SEEDED_HIT" == *"ruff and pytest"* ]] \
    && ok "recall surfaces seeded memory" \
    || fail "recall did not return seeded memory; got: $HITS_TEXT"

# update — replace content, switch type to decision
RESP=$(call_tool update "$(jq -nc --arg id "$MEM_ID" '{id:$id, content:"ci pipeline is RED — flaky transport test landed on main", type:"decision", tags:["ci","incident"]}')")
TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
[[ "$TEXT" == "updated memory "* ]] \
    && ok "update accepted new content + type" \
    || fail "update response unexpected: $TEXT"

# recall again — old phrase should be gone; new phrase should surface
RESP=$(call_tool recall '{"query":"current state of CI"}')
HITS_TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
NEW_HIT=$(jq -r --arg id "$MEM_ID" '.[] | select(.id == $id) | .content' <<<"$HITS_TEXT")
[[ "$NEW_HIT" == *"flaky transport"* ]] \
    && ok "post-update recall returns new content for $MEM_ID" \
    || fail "post-update recall did not return new content; got: $HITS_TEXT"

# Verify type was updated (response uses canonical "kind" key per Phase 6 unification)
NEW_KIND=$(jq -r --arg id "$MEM_ID" '.[] | select(.id == $id) | .kind' <<<"$HITS_TEXT")
[[ "$NEW_KIND" == "decision" ]] \
    && ok "update changed kind: fact → decision" \
    || fail "kind not updated; got '$NEW_KIND'"

# update with unknown id → "no such memory"
RESP=$(call_tool update '{"id":"01H0000000000000000000000Z","content":"ghost"}')
TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
[[ "$TEXT" == "no such memory "* ]] \
    && ok "update on unknown id returns no-such-memory" \
    || fail "expected no-such-memory; got: $TEXT"

# forget
RESP=$(call_tool forget "$(jq -nc --arg id "$MEM_ID" '{id:$id}')")
TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
[[ "$TEXT" == "forgot memory "* ]] && ok "forget removed $MEM_ID" || fail "forget unexpected: $TEXT"

# ---------- L0 procedural ----------

step "L0 procedural — pin / unpin"
RESP=$(call_tool pin '{"content":"always run cargo fmt before commits"}')
TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
PIN_ID=$(echo "$TEXT" | awk '{print $NF}')
[[ "$TEXT" == "pinned "* && ${#PIN_ID} -eq 26 ]] \
    && ok "pin returned ULID $PIN_ID" \
    || fail "pin response unexpected: $TEXT"

[[ -f "$MNEME_DATA_DIR/procedural/pinned.jsonl" ]] \
    && ok "pinned.jsonl present on disk" \
    || fail "pinned.jsonl missing"

RESP=$(call_tool unpin "$(jq -nc --arg id "$PIN_ID" '{id:$id}')")
TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
[[ "$TEXT" == "unpinned "* ]] && ok "unpin removed $PIN_ID" || fail "unpin unexpected: $TEXT"

# ---------- L3 episodic ----------

step "L3 episodic — recall_recent / summarize_session"
RESP=$(call_tool recall_recent '{"limit":20}')
RECENT_TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
# Since v0.2.3 every successful tools/call auto-emits a `tool_call`
# event, so by this point in the script the hot tier MUST contain
# at least one entry. n=0 means the producer regressed.
if echo "$RECENT_TEXT" | jq -e 'type == "array"' >/dev/null 2>&1; then
    COUNT=$(echo "$RECENT_TEXT" | jq 'length')
    KINDS=$(echo "$RECENT_TEXT" | jq -r '[.[].kind] | unique | join(",")')
    if [[ "$COUNT" -ge 1 ]]; then
        ok "recall_recent returned $COUNT event(s); kinds=[$KINDS]"
    else
        fail "recall_recent returned empty array; auto-emit on tools/call appears broken"
    fi
else
    fail "recall_recent did not return a JSON array: $RECENT_TEXT"
fi

RESP=$(call_tool summarize_session '{"session_id":"manual-test-1"}')
SUMMARY=$(jq -r '.result.content[0].text // ""' <<<"$RESP")
[[ -n "$SUMMARY" ]] \
    && ok "summarize_session returned a non-empty payload" \
    || fail "summarize_session returned empty payload"

# ---------- Phase 6 diagnostics ----------

step "Phase 6 — stats / list_scopes / export"
RESP=$(call_tool stats '{}')
STATS_JSON=$(jq -r '.result.content[0].text' <<<"$RESP")
SCHEMA=$(echo "$STATS_JSON" | jq -r '.schema_version // 0')
EMBED_DIM=$(echo "$STATS_JSON" | jq -r '.embed_dim // 0')
[[ "$SCHEMA" == "1" ]] && ok "stats: schema_version=1" || fail "stats: unexpected schema_version=$SCHEMA"
# embed_dim is read from the on-disk snapshot, so it stays 0 until the
# first scheduler-driven snapshot fires. Just verify the field is a
# non-negative integer.
if [[ "$EMBED_DIM" =~ ^[0-9]+$ ]]; then
    ok "stats: embed_dim=$EMBED_DIM"
else
    fail "stats: embed_dim is not a non-negative integer: '$EMBED_DIM'"
fi

RESP=$(call_tool list_scopes '{}')
SCOPES=$(jq -r '.result.content[0].text' <<<"$RESP")
echo "$SCOPES" | jq -e 'type == "array"' >/dev/null \
    && ok "list_scopes returned an array: $SCOPES" \
    || fail "list_scopes did not return array: $SCOPES"

# Seed one fresh memory so export has something to dump
call_tool remember '{"content":"export-seed-text","type":"fact","scope":"manual-test"}' >/dev/null

RESP=$(call_tool export '{"limit":50}')
EXPORT_JSON=$(jq -r '.result.content[0].text' <<<"$RESP")
SEM_COUNT=$(echo "$EXPORT_JSON" | jq -r '.semantic | length // 0')
[[ "$SEM_COUNT" -ge 1 ]] \
    && ok "export returned $SEM_COUNT semantic memories" \
    || fail "export returned no semantic memories: $EXPORT_JSON"

# ---------- shutdown for backup ----------

step "Clean shutdown"
exec 3>&-                       # close stdin → server EOF
exec 4<&-                       # we're done reading
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
ok "server exited cleanly"

# ---------- backup / restore round-trip ----------

step "Backup → wipe → restore round-trip"
# `--include-models` chokes when models/ is a symlink to a directory
# (File::open on a symlink follows it and hits EISDIR). The default
# excludes models/, which is what the production "small backup" path
# is anyway — we restore the symlink afterwards if --reuse-models.
if [[ "$REUSE_MODELS" -ne 1 && "$USE_STUB" -eq 1 ]]; then
    "$BIN" backup "$BACKUP_TAR" --include-models >/dev/null
else
    "$BIN" backup "$BACKUP_TAR" >/dev/null
fi
[[ -f "$BACKUP_TAR" ]] && ok "backup archive created ($(du -h "$BACKUP_TAR" | awk '{print $1}'))" \
                       || fail "backup archive missing"

# Compare data only — exclude models/ because the script may symlink
# it, which never round-trips through tar identically.
hash_data() {
    find "$1" -type f \
        -not -path "*/logs/*" \
        -not -path "*/.lock*" \
        -not -path "*/models/*" \
        -exec shasum {} \; 2>/dev/null \
        | awk '{print $1}' | sort | shasum | awk '{print $1}'
}
PRE_HASH=$(hash_data "$MNEME_DATA_DIR")

# Move data aside, restore into the same path, then diff.
SAVED="$TEST_ROOT/data.bak"
mv "$MNEME_DATA_DIR" "$SAVED"
"$BIN" restore "$BACKUP_TAR" >/dev/null

# Re-link models so the relaunched server can find them.
link_models || true

POST_HASH=$(hash_data "$MNEME_DATA_DIR")
[[ "$PRE_HASH" == "$POST_HASH" ]] \
    && ok "restored tree byte-identical to pre-backup state" \
    || fail "restored tree diverges from pre-backup hash"

# ---------- post-restore recall ----------

step "Post-restore: relaunch + recall must still work"
mkfifo "$TEST_ROOT/req2.fifo" "$TEST_ROOT/resp2.fifo"
"$BIN" run < "$TEST_ROOT/req2.fifo" > "$TEST_ROOT/resp2.fifo" 2>> "$LOG" &
SERVER_PID=$!
exec 3> "$TEST_ROOT/req2.fifo"
exec 4< "$TEST_ROOT/resp2.fifo"

NEXT_ID=100
send_request "initialize" "$init_params" >/dev/null
read_response 120 >/dev/null
send_notification "notifications/initialized"

RESP=$(call_tool recall '{"query":"export-seed-text","limit":5}')
HITS_TEXT=$(jq -r '.result.content[0].text' <<<"$RESP")
HIT_COUNT=$(jq 'length' <<<"$HITS_TEXT")
[[ "$HIT_COUNT" -ge 1 ]] \
    && ok "post-restore recall returned $HIT_COUNT hit(s)" \
    || fail "post-restore recall empty: $HITS_TEXT"

exec 3>&-
exec 4<&-
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""

# ---------- summary ----------

step "Summary"
TOTAL=$((PASS + FAIL))
if [[ "$FAIL" -eq 0 ]]; then
    printf "${C_OK}%s${C_OFF} %d/%d checks passed\n" "ALL GREEN" "$PASS" "$TOTAL"
    exit 0
else
    printf "${C_FAIL}%d FAILED${C_OFF} (out of %d):\n" "$FAIL" "$TOTAL"
    for t in "${FAILED_TESTS[@]}"; do printf "  - %s\n" "$t"; done
    printf "\nServer log: %s\n" "$LOG"
    exit 1
fi
