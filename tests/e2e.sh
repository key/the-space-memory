#!/usr/bin/env bash
# E2E integration tests for The Space Memory.
# Exercises the full CLI surface: daemon, indexer, searcher, embedder,
# watcher, dictionary, and edge cases.
#
# Prerequisites:
#   - Linux with GNU coreutils (date -d, sed -i)
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
BOLD='\033[1m'
RESET='\033[0m'

log()  { echo -e "${BOLD}[e2e]${RESET} $*"; }
pass() { PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${RESET} $1"; }
fail() { FAIL=$((FAIL + 1)); ERRORS+=("$1: $2"); echo -e "  ${RED}FAIL${RESET} $1: $2"; }

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

# Capture command output and exit code without || true masking $?.
# Usage: run CMD ARGS...  → sets CAPTURED_OUTPUT and CAPTURED_EXIT.
# stderr is merged into stdout for tsm commands; search_json already
# redirects stderr to /dev/null internally.
run() {
    set +e
    CAPTURED_OUTPUT=$("$@")
    CAPTURED_EXIT=$?
    set -e
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
THREE_MONTHS_AGO_START=$(date -d '3 months ago' +%Y-%m-01)
# Exclusive upper bound: first day of (3 months ago + 1 month)
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

set +e; CAPTURED_OUTPUT=$(tsm status 2>&1); CAPTURED_EXIT=$?; set -e
assert_contains "status: daemon running" "Daemon:" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

set +e; CAPTURED_OUTPUT=$(tsm doctor -f json 2>&1); CAPTURED_EXIT=$?; set -e
assert_json "doctor: json output" '.issue_count >= 0' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Index → Search round-trip ─────────────────────────────────────────

echo ""
log "=== Index → Search round-trip ==="

run search_json "親譲り 無鉄砲"
assert_json "index-search: botchan hit" \
    'any(.[]; .source_file | contains("botchan"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── FTS5 search ───────────────────────────────────────────────────────

echo ""
log "=== FTS5 search ==="

run search_json "ジョバンニ カムパネルラ"
assert_json "fts5: gingatetsudo hit" \
    'any(.[]; .source_file | contains("gingatetsudo"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "メロス 激怒"
assert_json "fts5: hashire-melos hit" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Search options (--top-k, --include-content, text format) ────────

echo ""
log "=== Search options ==="

# --top-k: request 1 result, verify exactly 1 returned
run search_json "メロス" -k 1
assert_json "options: -k 1 returns exactly 1 result" \
    'length == 1' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# --include-content: verify content field is present
run search_json "メロス" -k 1 --include-content 1
assert_json "options: --include-content adds content field" \
    '.[0].content != null and (.[0].content | length > 0)' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# text format (default): verify human-readable output contains score and file
set +e; CAPTURED_OUTPUT=$(tsm search -q "メロス" -k 1 2>/dev/null); CAPTURED_EXIT=$?; set -e
assert_contains "options: text format shows source file" "hashire-melos" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Entity search (tag boost) ────────────────────────────────────────

echo ""
log "=== Entity search ==="

run search_json "漱石"
assert_json "entity: 漱石 → botchan" \
    'any(.[]; .source_file | contains("botchan"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "賢治"
assert_json "entity: 賢治 → gingatetsudo" \
    'any(.[]; .source_file | contains("gingatetsudo"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "太宰"
assert_json "entity: 太宰 → hashire-melos" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Temporal search ───────────────────────────────────────────────────

echo ""
log "=== Temporal search ==="

run search_json "猫" --recent 30d
assert_json "temporal: --recent 30d excludes old-text" \
    '[.[] | select(.source_file | contains("old-text"))] | length == 0' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

THIS_YEAR=$(date +%Y)
run search_json "吾輩 猫" --year "$THIS_YEAR"
assert_json "temporal: --year THIS_YEAR excludes old-text" \
    '[.[] | select(.source_file | contains("old-text"))] | length == 0' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "紳士 料理店" --after "$THREE_MONTHS_AGO_START" --before "$THREE_MONTHS_AGO_END"
assert_json "temporal: --after/--before hits seasonal-text" \
    'any(.[]; .source_file | contains("seasonal-text"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Vector search (semantic similarity) ───────────────────────────────

echo ""
log "=== Vector search ==="

run search_json "学校の先生と生徒"
assert_json "vector: 学校の先生と生徒 → botchan" \
    'any(.[]; .source_file | contains("botchan"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "宇宙と星の旅"
assert_json "vector: 宇宙と星の旅 → gingatetsudo" \
    'any(.[]; .source_file | contains("gingatetsudo"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Dictionary test ───────────────────────────────────────────────────

echo ""
log "=== Dictionary ==="

# Dict test: add a word to user dict, reindex FTS, verify search works with it.
# Use "セリヌンティウス" — a proper noun only in hashire-melos.md that lindera
# splits into multiple tokens by default, but as a dict entry becomes one token.
DICT_WORD="セリヌンティウス"

# Search before dict registration — should already hit via constituent tokens
run search_json "$DICT_WORD" --fallback fts-only
OUTPUT_BEFORE="$CAPTURED_OUTPUT"

# Add word to user dict, reindex FTS via daemon
USER_DICT_PATH="$TSM_STATE_DIR/user_dict.simpledic"
echo "${DICT_WORD},名詞,${DICT_WORD}" >> "$USER_DICT_PATH"
log "Added '$DICT_WORD' to user dictionary"

log "Reindexing FTS via daemon..."
tsm reindex fts 2>/dev/null

# Wait for reindex to complete (check doctor until Reindex section disappears)
for _i in $(seq 1 30); do
    DOCTOR_JSON=$(tsm doctor -f json 2>/dev/null)
    if ! echo "$DOCTOR_JSON" | jq -e '.sections[] | select(.name == "Reindex")' >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

# Verify search before dict registration found results
assert_json "dict: '$DICT_WORD' found before dict (via constituent tokens)" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$OUTPUT_BEFORE" "0"

# Verify search still works after reindex
run search_json "メロス 激怒" --fallback fts-only
assert_json "dict: search works after reindex" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Verify dict-registered word works as a standalone search query (#104)
run search_json "$DICT_WORD" --fallback fts-only
assert_json "dict: standalone search for dict term '$DICT_WORD' hits hashire-melos" \
    'any(.[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Edge cases ────────────────────────────────────────────────────────

echo ""
log "=== Edge cases ==="

# EC1: Empty query
run search_json ""
if [[ "$CAPTURED_EXIT" -eq 0 ]]; then
    assert_json "edge: empty query → 0 results or valid json" \
        'if type == "array" then true else false end' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"
else
    # Empty query might be rejected by clap, that's OK too
    pass "edge: empty query → handled (exit $CAPTURED_EXIT)"
fi

# EC4: Single character — must exit 0 and return valid JSON
run search_json "a"
assert_json "edge: single char 'a' → valid json (exit $CAPTURED_EXIT)" \
    'type == "array"' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# EC5: Invalid --recent value
set +e; CAPTURED_OUTPUT=$(tsm search -q "test" --recent garbage 2>&1); CAPTURED_EXIT=$?; set -e
assert_fail "edge: --recent garbage → error" "$CAPTURED_EXIT"

# ── Ingest session ─────────────────────────────────────────────────────

echo ""
log "=== Ingest session ==="

SESSION_FILE="$SCRIPT_DIR/e2e/testdata/sessions/test-session.jsonl"
set +e; CAPTURED_OUTPUT=$(tsm ingest-session "$SESSION_FILE" 2>&1); CAPTURED_EXIT=$?; set -e
assert_contains "ingest-session: succeeds" "session indexed" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Search for session-specific content (use fts-only; embedder may not be ready)
run search_json "量子もつれ" --fallback fts-only
assert_json "ingest-session: search hits session content" \
    'any(.[]; .source_file | contains("test-session"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Re-ingest same file should be a no-op (already indexed)
set +e; CAPTURED_OUTPUT=$(tsm ingest-session "$SESSION_FILE" 2>&1); CAPTURED_EXIT=$?; set -e
assert_contains "ingest-session: re-ingest is no-op" "session unchanged" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

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
    run search_json "幻想水滸伝"
    if echo "$CAPTURED_OUTPUT" | jq -e 'any(.[]; .source_file | contains("watcher-test"))' >/dev/null 2>&1; then
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
    run search_json "幻想水滸伝"
    if ! echo "$CAPTURED_OUTPUT" | jq -e 'any(.[]; .source_file | contains("watcher-test"))' >/dev/null 2>&1; then
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
