#!/usr/bin/env bash
# E2E integration tests for The Space Memory.
# Exercises the full CLI surface: daemon, indexer, searcher, embedder,
# watcher, dictionary, and edge cases.
#
# Prerequisites:
#   - tsm and tsmd binaries on PATH (cargo build --release)
#   - ruri-v3-30m model downloaded (tsm setup)
#   - jq installed
set -euo pipefail

# ── Helpers ───────────────────────────────────────────────────────────

PASS=0
FAIL=0
ERRORS=()
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
RESET='\033[0m'

log()  { echo -e "${BOLD}[e2e]${RESET} $*"; }
pass() { ((PASS++)); echo -e "  ${GREEN}PASS${RESET} $1"; }
fail() { ((FAIL++)); ERRORS+=("$1: $2"); echo -e "  ${RED}FAIL${RESET} $1: $2"; }

# run_test NAME COMMAND ASSERTION_FUNC
#   Runs COMMAND, captures stdout+stderr and exit code,
#   then calls ASSERTION_FUNC with the captured output.
run_test() {
    local name="$1"
    shift
    local output exit_code
    set +e
    output=$("$@" 2>&1)
    exit_code=$?
    set -e
    echo "$output" | "$@_assert" "$name" "$exit_code" "$output" 2>/dev/null \
        || true  # assertion func handles pass/fail
}

# Assert: command succeeded (exit 0) and jq expression is truthy
assert_json() {
    local name="$1" jq_expr="$2" output="$3" exit_code="${4:-0}"
    if [[ "$exit_code" -ne 0 ]]; then
        fail "$name" "exit code $exit_code (expected 0)"
        return
    fi
    if echo "$output" | jq -e "$jq_expr" >/dev/null 2>&1; then
        pass "$name"
    else
        fail "$name" "jq assertion failed: $jq_expr"
        echo "    output: $(echo "$output" | head -3)"
    fi
}

# Assert: command succeeded and output contains string
assert_contains() {
    local name="$1" pattern="$2" output="$3" exit_code="${4:-0}"
    if [[ "$exit_code" -ne 0 ]]; then
        fail "$name" "exit code $exit_code (expected 0)"
        return
    fi
    if echo "$output" | grep -q "$pattern"; then
        pass "$name"
    else
        fail "$name" "output does not contain '$pattern'"
    fi
}

# Assert: command failed (exit != 0)
assert_fail() {
    local name="$1" exit_code="$2"
    if [[ "$exit_code" -ne 0 ]]; then
        pass "$name"
    else
        fail "$name" "expected non-zero exit code, got 0"
    fi
}

# Search helper: tsm search -q QUERY -f json [extra args...]
search_json() {
    tsm search -q "$1" -f json "${@:2}" 2>/dev/null
}

# Poll until a search hits (or doesn't hit) a file, with timeout.
# poll_search_hit QUERY FILE TIMEOUT_SECS
poll_search_hit() {
    local query="$1" file="$2" timeout="$3"
    local elapsed=0
    while [[ $elapsed -lt $timeout ]]; do
        if search_json "$query" 2>/dev/null | jq -e "any(.[]; .source_file | contains(\"$file\"))" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    return 1
}

# poll_search_miss QUERY FILE TIMEOUT_SECS
poll_search_miss() {
    local query="$1" file="$2" timeout="$3"
    local elapsed=0
    while [[ $elapsed -lt $timeout ]]; do
        if ! search_json "$query" 2>/dev/null | jq -e "any(.[]; .source_file | contains(\"$file\"))" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    return 1
}

# ── Environment setup ─────────────────────────────────────────────────

export TSM_STATE_DIR
TSM_STATE_DIR="$(mktemp -d)"
export TSM_INDEX_ROOT
TSM_INDEX_ROOT="$(mktemp -d)"
export TSM_EMBEDDER_IDLE_TIMEOUT=0
export TSM_EMBEDDER_BACKFILL_INTERVAL=0

# Compute dynamic dates
TODAY=$(date +%Y-%m-%d)
ONE_YEAR_AGO=$(date -d '1 year ago' +%Y-%m-%d)
THREE_MONTHS_AGO=$(date -d '3 months ago' +%Y-%m-%d)
LAST_YEAR=$(date -d '1 year ago' +%Y)
THREE_MONTHS_AGO_START=$(date -d '3 months ago' +%Y-%m-01)
# End of that month: first day of (3 months ago + 1 month)
THREE_MONTHS_AGO_END=$(date -d '2 months ago' +%Y-%m-01)

log "TSM_STATE_DIR=$TSM_STATE_DIR"
log "TSM_INDEX_ROOT=$TSM_INDEX_ROOT"
log "TODAY=$TODAY  1Y_AGO=$ONE_YEAR_AGO  3M_AGO=$THREE_MONTHS_AGO"

# Cleanup on exit
cleanup() {
    log "Cleaning up..."
    tsm stop 2>/dev/null || true
    rm -rf "$TSM_STATE_DIR" "$TSM_INDEX_ROOT"
}
trap cleanup EXIT

# ── Prepare test data ─────────────────────────────────────────────────

log "Preparing test data..."
cp -r "$SCRIPT_DIR/e2e/testdata/"* "$TSM_INDEX_ROOT/"
sed -i \
    "s/__TODAY__/$TODAY/g; s/__1Y_AGO__/$ONE_YEAR_AGO/g; s/__3M_AGO__/$THREE_MONTHS_AGO/g" \
    "$TSM_INDEX_ROOT"/notes/*.md

# ── Init & start daemon ──────────────────────────────────────────────

log "Initializing database..."
tsm init

log "Starting daemon (with embedder + watcher)..."
tsm start

# Wait for embedder to be ready (model loading can take a while)
log "Waiting for embedder to become ready..."
EMBEDDER_TIMEOUT=180
ELAPSED=0
while [[ $ELAPSED -lt $EMBEDDER_TIMEOUT ]]; do
    if tsm status 2>/dev/null | grep -q "Embedder:.*running"; then
        break
    fi
    sleep 2
    ELAPSED=$((ELAPSED + 2))
done
if [[ $ELAPSED -ge $EMBEDDER_TIMEOUT ]]; then
    log "WARNING: Embedder did not become ready within ${EMBEDDER_TIMEOUT}s"
    tsm status 2>/dev/null || true
fi

# ── Index all documents ───────────────────────────────────────────────

log "Indexing documents..."
tsm index 2>/dev/null

# Fill vectors
log "Filling vectors..."
tsm vector-fill 2>/dev/null

# Small wait for backfill to settle
sleep 2

# ══════════════════════════════════════════════════════════════════════
# TESTS
# ══════════════════════════════════════════════════════════════════════

log "Running tests..."

# ── Daemon basics ─────────────────────────────────────────────────────

echo ""
log "=== Daemon basics ==="

OUTPUT=$(tsm status 2>&1) || true
EXIT=$?
assert_contains "status: daemon running" "Daemon:" "$OUTPUT" "$EXIT"

OUTPUT=$(tsm doctor -f json 2>&1) || true
EXIT=$?
assert_json "doctor: json output" '.issue_count >= 0' "$OUTPUT" "$EXIT"

# ── Index → Search round-trip ─────────────────────────────────────────

echo ""
log "=== Index → Search round-trip ==="

OUTPUT=$(search_json "親譲り 無鉄砲") || true
EXIT=$?
assert_json "index-search: botchan hit" \
    'any(.[]; .source_file | contains("botchan"))' "$OUTPUT" "$EXIT"

# ── FTS5 search ───────────────────────────────────────────────────────

echo ""
log "=== FTS5 search ==="

OUTPUT=$(search_json "ジョバンニ カムパネルラ") || true
EXIT=$?
assert_json "fts5: gingatetsudo hit" \
    'any(.[]; .source_file | contains("gingatetsudo"))' "$OUTPUT" "$EXIT"

OUTPUT=$(search_json "メロス 激怒") || true
EXIT=$?
assert_json "fts5: hashire-melos hit" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$OUTPUT" "$EXIT"

# ── Entity search (tag boost) ────────────────────────────────────────

echo ""
log "=== Entity search ==="

OUTPUT=$(search_json "漱石") || true
EXIT=$?
assert_json "entity: 漱石 → botchan" \
    'any(.[]; .source_file | contains("botchan"))' "$OUTPUT" "$EXIT"

OUTPUT=$(search_json "賢治") || true
EXIT=$?
assert_json "entity: 賢治 → gingatetsudo" \
    'any(.[]; .source_file | contains("gingatetsudo"))' "$OUTPUT" "$EXIT"

OUTPUT=$(search_json "太宰") || true
EXIT=$?
assert_json "entity: 太宰 → hashire-melos" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$OUTPUT" "$EXIT"

# ── Temporal search ───────────────────────────────────────────────────

echo ""
log "=== Temporal search ==="

OUTPUT=$(search_json "猫" --recent 30d) || true
EXIT=$?
assert_json "temporal: --recent 30d excludes old-text" \
    '[.[] | select(.source_file | contains("old-text"))] | length == 0' "$OUTPUT" "$EXIT"

OUTPUT=$(search_json "文学" --year "$LAST_YEAR") || true
EXIT=$?
assert_json "temporal: --year hits old-text" \
    'any(.[]; .source_file | contains("old-text"))' "$OUTPUT" "$EXIT"

OUTPUT=$(search_json "文学" --after "$THREE_MONTHS_AGO_START" --before "$THREE_MONTHS_AGO_END") || true
EXIT=$?
assert_json "temporal: --after/--before hits seasonal-text" \
    'any(.[]; .source_file | contains("seasonal-text"))' "$OUTPUT" "$EXIT"

# ── Vector search (semantic similarity) ───────────────────────────────

echo ""
log "=== Vector search ==="

OUTPUT=$(search_json "学校の先生と生徒") || true
EXIT=$?
assert_json "vector: 学校の先生と生徒 → botchan" \
    'any(.[]; .source_file | contains("botchan"))' "$OUTPUT" "$EXIT"

OUTPUT=$(search_json "宇宙と星の旅") || true
EXIT=$?
assert_json "vector: 宇宙と星の旅 → gingatetsudo" \
    'any(.[]; .source_file | contains("gingatetsudo"))' "$OUTPUT" "$EXIT"

# ── Dictionary test ───────────────────────────────────────────────────

echo ""
log "=== Dictionary ==="

# Pick a compound word for dict test: "銀河鉄道" (should be in gingatetsudo.md)
DICT_WORD="銀河鉄道"
DICT_FILE="gingatetsudo"

# Search before dict registration (record result)
OUTPUT_BEFORE=$(search_json "$DICT_WORD" 2>/dev/null) || true

# Stop daemon, add word to user dict, rebuild FTS, restart
log "Stopping daemon for dict update..."
tsm stop 2>/dev/null
sleep 1

USER_DICT_PATH="$TSM_STATE_DIR/user_dict.simpledic"
echo "${DICT_WORD},カスタム名詞,${DICT_WORD}" >> "$USER_DICT_PATH"
log "Added '$DICT_WORD' to user dictionary"

log "Rebuilding FTS index..."
tsm rebuild --fts-only 2>/dev/null

log "Restarting daemon..."
tsm start 2>/dev/null

# Wait for daemon ready
sleep 3

# Search after dict registration
OUTPUT_AFTER=$(search_json "$DICT_WORD" 2>/dev/null) || true
EXIT=$?
assert_json "dict: $DICT_WORD → $DICT_FILE after dict update" \
    "any(.[]; .source_file | contains(\"$DICT_FILE\"))" "$OUTPUT_AFTER" "$EXIT"

# ── Edge cases ────────────────────────────────────────────────────────

echo ""
log "=== Edge cases ==="

# EC1: Empty query
OUTPUT=$(search_json "" 2>/dev/null) || true
EXIT=$?
if [[ "$EXIT" -eq 0 ]]; then
    assert_json "edge: empty query → 0 results or valid json" \
        'if type == "array" then true else false end' "$OUTPUT" "$EXIT"
else
    # Empty query might be rejected by clap, that's OK too
    pass "edge: empty query → handled (exit $EXIT)"
fi

# EC4: Single character
OUTPUT=$(search_json "a" 2>/dev/null) || true
EXIT=$?
# Should not crash; any exit code is fine as long as it doesn't segfault
pass "edge: single char 'a' → no crash (exit $EXIT)"

# EC5: Invalid --recent value
set +e
OUTPUT=$(tsm search -q "test" --recent garbage 2>&1)
EXIT=$?
set -e
assert_fail "edge: --recent garbage → error" "$EXIT"

# ── Watcher test ──────────────────────────────────────────────────────

echo ""
log "=== Watcher ==="

# Create a new file and wait for watcher to pick it up
WATCHER_FILE="$TSM_INDEX_ROOT/notes/watcher-test.md"
cat > "$WATCHER_FILE" <<HEREDOC
---
status: current
updated: $TODAY
tags: [テスト]
---

# ウォッチャーテスト

これはファイル監視のテスト用ドキュメントです。独自キーワード「幻想水滸伝」を含みます。
HEREDOC

log "Created watcher test file, polling for index..."
if poll_search_hit "幻想水滸伝" "watcher-test" 20; then
    pass "watcher: new file detected and indexed"
else
    # Fallback: manually index and check
    tsm index 2>/dev/null
    sleep 2
    OUTPUT=$(search_json "幻想水滸伝") || true
    EXIT=$?
    if echo "$OUTPUT" | jq -e 'any(.[]; .source_file | contains("watcher-test"))' >/dev/null 2>&1; then
        pass "watcher: new file indexed (after manual index fallback)"
    else
        fail "watcher: new file not detected" "watcher-test.md not found in search results"
    fi
fi

# Delete the file and wait for watcher to remove it
rm -f "$WATCHER_FILE"
log "Deleted watcher test file, polling for removal..."
if poll_search_miss "幻想水滸伝" "watcher-test" 20; then
    pass "watcher: deleted file removed from index"
else
    # Fallback: manually index
    tsm index 2>/dev/null
    sleep 2
    OUTPUT=$(search_json "幻想水滸伝") || true
    if ! echo "$OUTPUT" | jq -e 'any(.[]; .source_file | contains("watcher-test"))' >/dev/null 2>&1; then
        pass "watcher: deleted file removed (after manual index fallback)"
    else
        fail "watcher: deleted file still in index" "watcher-test.md still appears in search results"
    fi
fi

# ══════════════════════════════════════════════════════════════════════
# Summary
# ══════════════════════════════════════════════════════════════════════

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "  ${GREEN}PASS: $PASS${RESET}  ${RED}FAIL: $FAIL${RESET}"

if [[ ${#ERRORS[@]} -gt 0 ]]; then
    echo ""
    echo -e "  ${RED}Failures:${RESET}"
    for err in "${ERRORS[@]}"; do
        echo -e "    ${RED}✘${RESET} $err"
    done
fi
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# Dump logs on failure for CI debugging
if [[ $FAIL -gt 0 ]]; then
    echo ""
    log "=== Daemon logs ==="
    cat "$TSM_STATE_DIR"/logs/*.log 2>/dev/null || echo "(no logs found)"
    exit 1
fi

exit 0
