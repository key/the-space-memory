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
        if search_json "$query" 2>/dev/null | jq -e "any(.results[]; .source_file | contains(\"$file\"))" >/dev/null 2>&1; then
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
        if ! search_json "$query" 2>/dev/null | jq -e "any(.results[]; .source_file | contains(\"$file\"))" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    return 1
}

# Wait until `tsm status` reports the embedder as running.
# wait_embedder_ready TIMEOUT_SECS → returns 0 if ready, 1 on timeout.
# Callers should surface a final `tsm status` on timeout for diagnostics
# (this function suppresses stderr so the poll loop stays quiet).
wait_embedder_ready() {
    local timeout="$1"
    local elapsed=0
    while [[ $elapsed -lt $timeout ]]; do
        if tsm status 2>/dev/null | grep -q "Embedder:.*running"; then
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
# ADR-0009 §2 / §3: tsm resolves the project root from the CWD's tsm.toml.
# Run from a dedicated temp project dir so `tsm init` scaffolds tsm.toml there
# (not in the repo) and every later command resolves to the same root.
# Content is placed directly under TSM_PROJECT_DIR (no separate TSM_INDEX_ROOT).
TSM_PROJECT_DIR="$(mktemp -d)"
cd "$TSM_PROJECT_DIR" || exit 1
export TSM_EMBEDDER_IDLE_TIMEOUT=0
export TSM_EMBEDDER_BACKFILL_INTERVAL=0
# Small batch so a reindex-in-progress spans multiple yields; lets the
# write-preemption test observe real preemption rather than a no-op race.
export TSM_REINDEX_FTS_BATCH_SIZE=10

# Compute dynamic dates
TODAY=$(date +%Y-%m-%d)
ONE_YEAR_AGO=$(date -d '1 year ago' +%Y-%m-%d)
THREE_MONTHS_AGO=$(date -d '3 months ago' +%Y-%m-%d)
THREE_MONTHS_AGO_START=$(date -d '3 months ago' +%Y-%m-01)
# Exclusive upper bound: first day of (3 months ago + 1 month)
THREE_MONTHS_AGO_END=$(date -d '2 months ago' +%Y-%m-01)

log "TSM_STATE_DIR=$TSM_STATE_DIR"
log "TSM_PROJECT_DIR=$TSM_PROJECT_DIR"
log "TODAY=$TODAY  1Y_AGO=$ONE_YEAR_AGO  3M_AGO=$THREE_MONTHS_AGO"

# Cleanup on exit (invoked via trap below)
# shellcheck disable=SC2329
cleanup() {
    log "Cleaning up..."
    tsm stop 2>/dev/null || true
    rm -rf "$TSM_STATE_DIR" "$TSM_PROJECT_DIR"
}
trap cleanup EXIT

# ── Prepare test data ─────────────────────────────────────────────────

log "Preparing test data..."
cp -r "$SCRIPT_DIR/e2e/testdata/"* "$TSM_PROJECT_DIR/"
sed -i \
    "s/__TODAY__/$TODAY/g; s/__1Y_AGO__/$ONE_YEAR_AGO/g; s/__3M_AGO__/$THREE_MONTHS_AGO/g" \
    "$TSM_PROJECT_DIR"/notes/*.md \
    "$TSM_PROJECT_DIR"/sessions/*.jsonl

# ── Fail-fast: uninitialized `tsm start` leaves no stray state ────────
# Regression guard: starting with a tsm.toml present but no initialized DB must
# fail with "Run `tsm init` first" WITHOUT materializing the state directory.
# Both main.rs and daemon_mode.rs are excluded from unit coverage, so this
# process-level check is the only automated guard for that ordering.
log "=== Fail-fast on uninitialized DB (no stray state) ==="
UNINIT_PROJECT="$(mktemp -d)"
UNINIT_STATE_PARENT="$(mktemp -d)"
UNINIT_STATE="$UNINIT_STATE_PARENT/state"   # deliberately does not exist yet
printf '[[index.content_dirs]]\npath = "."\nweight = 1.0\n' > "$UNINIT_PROJECT/tsm.toml"
# Capture both streams: the fail-fast error is printed to stderr. The `cd` runs
# inside the command-substitution subshell, so the suite's CWD is untouched.
set +e
UNINIT_OUT=$(cd "$UNINIT_PROJECT" && TSM_STATE_DIR="$UNINIT_STATE" tsm start 2>&1)
UNINIT_EXIT=$?
set -e
assert_fail "uninit-start: exits non-zero" "$UNINIT_EXIT"
if echo "$UNINIT_OUT" | grep -q 'Run .tsm init. first'; then
    pass "uninit-start: error tells user to run tsm init"
else
    fail "uninit-start: error tells user to run tsm init" "got: $UNINIT_OUT"
fi
if [[ ! -e "$UNINIT_STATE" ]]; then
    pass "uninit-start: state dir not materialized"
else
    fail "uninit-start: state dir not materialized" "exists: $(ls -a "$UNINIT_STATE" 2>/dev/null)"
fi
rm -rf "$UNINIT_PROJECT" "$UNINIT_STATE_PARENT"

# ── Init & start daemon ──────────────────────────────────────────────

log "Initializing database..."
tsm init

log "Starting daemon (with embedder + watcher)..."
tsm start

# Wait for embedder to be ready (model loading can take a while on first run)
log "Waiting for embedder to become ready..."
if ! wait_embedder_ready 180; then
    log "WARNING: Embedder did not become ready within 180s"
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
    'any(.results[]; .source_file | contains("botchan"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── FTS5 search ───────────────────────────────────────────────────────

echo ""
log "=== FTS5 search ==="

run search_json "ジョバンニ カムパネルラ"
assert_json "fts5: gingatetsudo hit" \
    'any(.results[]; .source_file | contains("gingatetsudo"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "メロス 激怒"
assert_json "fts5: hashire-melos hit" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Search options (--top-k, --include-content, text format) ────────

echo ""
log "=== Search options ==="

# --top-k: request 1 result, verify exactly 1 returned
run search_json "メロス" -k 1
assert_json "options: -k 1 returns exactly 1 result" \
    '.results | length == 1' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# --include-content: verify content field is present
run search_json "メロス" -k 1 --include-content 1
assert_json "options: --include-content adds content field" \
    '.results[0].content != null and (.results[0].content | length > 0)' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# text format (default): verify human-readable output contains score and file
set +e; CAPTURED_OUTPUT=$(tsm search -q "メロス" -k 1 2>/dev/null); CAPTURED_EXIT=$?; set -e
assert_contains "options: text format shows source file" "hashire-melos" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Path scoping (--path, ADR-0017) ──────────────────────────────────
# Stored file_path is absolute; --path accepts CWD-relative or absolute input
# and matches at a directory boundary. CWD is TSM_PROJECT_DIR.

echo ""
log "=== Path scoping (--path) ==="

# In-scope, CWD-relative: a bare relative path is resolved against the CWD
# (TSM_PROJECT_DIR) before matching — `notes` → `<project>/notes`.
run search_json "メロス" --path notes
assert_json "path: relative --path notes includes hashire-melos" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# In-scope, CWD-relative with explicit `./` prefix resolves the same way.
run search_json "メロス" --path ./notes
assert_json "path: relative --path ./notes includes hashire-melos" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# In-scope: absolute directory also works (same hit as the relative form).
run search_json "メロス" --path "$TSM_PROJECT_DIR/notes"
assert_json "path: absolute --path includes hashire-melos" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# In-scope, parent traversal: a `../` round-trip back into the project resolves
# lexically to `<project>/notes`. CWD is TSM_PROJECT_DIR, so `../<base>/notes`
# folds to the same in-scope directory. Verifies `..` is resolved (ADR-0017).
PROJECT_BASE="$(basename "$TSM_PROJECT_DIR")"
run search_json "メロス" --path "../$PROJECT_BASE/notes"
assert_json "path: parent-traversal --path ../<base>/notes includes hashire-melos" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Out-of-scope parent path: `../<base>/note` (boundary) must NOT match notes/.
run search_json "メロス" --path "../$PROJECT_BASE/note"
assert_json "path: parent-traversal --path ../<base>/note excludes notes/ (boundary)" \
    'all(.results[]; (.source_file | contains("hashire-melos")) | not)' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Case-insensitive (ADR-0017, deliberate owner-accepted tradeoff): an upper-case
# `NOTES` still matches the lower-case `notes/` directory. This pins the chosen
# forgiving behavior so a future change to case-sensitive matching is caught.
run search_json "メロス" --path NOTES
assert_json "path: --path is case-insensitive (NOTES matches notes/)" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Boundary: `note` must NOT match the `notes/` directory (no substring leak)
run search_json "メロス" --path note
assert_json "path: --path note excludes notes/ (boundary)" \
    'all(.results[]; (.source_file | contains("hashire-melos")) | not)' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Entity search (tag boost) ────────────────────────────────────────

echo ""
log "=== Entity search ==="

run search_json "漱石"
assert_json "entity: 漱石 → botchan" \
    'any(.results[]; .source_file | contains("botchan"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "賢治"
assert_json "entity: 賢治 → gingatetsudo" \
    'any(.results[]; .source_file | contains("gingatetsudo"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "太宰"
assert_json "entity: 太宰 → hashire-melos" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Temporal search ───────────────────────────────────────────────────

echo ""
log "=== Temporal search ==="

run search_json "猫" --recent 30d
assert_json "temporal: --recent 30d excludes old-text" \
    '[.results[] | select(.source_file | contains("old-text"))] | length == 0' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

THIS_YEAR=$(date +%Y)
run search_json "吾輩 猫" --year "$THIS_YEAR"
assert_json "temporal: --year THIS_YEAR excludes old-text" \
    '[.results[] | select(.source_file | contains("old-text"))] | length == 0' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "紳士 料理店" --after "$THREE_MONTHS_AGO_START" --before "$THREE_MONTHS_AGO_END"
assert_json "temporal: --after/--before hits seasonal-text" \
    'any(.results[]; .source_file | contains("seasonal-text"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Vector search (semantic similarity) ───────────────────────────────

echo ""
log "=== Vector search ==="

# Use a wider window than the default top-5: `tsm init` imports the full
# Japanese WordNet, and query expansion lets one multi-chunk document fill the
# first several hybrid-ranked slots, pushing the semantically-matched botchan
# just past the default cutoff. -k 10 keeps the assertion about semantic recall
# without depending on the exact rank.
run search_json "学校の先生と生徒" -k 10
assert_json "vector: 学校の先生と生徒 → botchan" \
    'any(.results[]; .source_file | contains("botchan"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

run search_json "宇宙と星の旅"
assert_json "vector: 宇宙と星の旅 → gingatetsudo" \
    'any(.results[]; .source_file | contains("gingatetsudo"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

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
    'any(.results[]; .source_file | contains("hashire-melos"))' "$OUTPUT_BEFORE" "0"

# Verify search still works after reindex
run search_json "メロス 激怒" --fallback fts-only
assert_json "dict: search works after reindex" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Verify dict-registered word works as a standalone search query (#104)
run search_json "$DICT_WORD" --fallback fts-only
assert_json "dict: standalone search for dict term '$DICT_WORD' hits hashire-melos" \
    'any(.results[]; .source_file | contains("hashire-melos"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Edge cases ────────────────────────────────────────────────────────

echo ""
log "=== Edge cases ==="

# EC1: Empty query
run search_json ""
if [[ "$CAPTURED_EXIT" -eq 0 ]]; then
    assert_json "edge: empty query → 0 results or valid json" \
        '.results | type == "array"' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"
else
    # Empty query might be rejected by clap, that's OK too
    pass "edge: empty query → handled (exit $CAPTURED_EXIT)"
fi

# EC4: Single character — must exit 0 and return valid JSON
run search_json "a"
assert_json "edge: single char 'a' → valid json (exit $CAPTURED_EXIT)" \
    '.results | type == "array"' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# EC5: Invalid --recent value
set +e; CAPTURED_OUTPUT=$(tsm search -q "test" --recent garbage 2>&1); CAPTURED_EXIT=$?; set -e
assert_fail "edge: --recent garbage → error" "$CAPTURED_EXIT"

# ── Ingest session ─────────────────────────────────────────────────────

echo ""
log "=== Ingest session ==="

SESSION_FILE="$TSM_PROJECT_DIR/sessions/test-session.jsonl"
set +e; CAPTURED_OUTPUT=$(tsm ingest-session "$SESSION_FILE" 2>&1); CAPTURED_EXIT=$?; set -e
assert_contains "ingest-session: succeeds" "session indexed" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Search for session-specific content (use fts-only; embedder may not be ready)
run search_json "量子もつれ" --fallback fts-only
assert_json "ingest-session: search hits session content" \
    'any(.results[]; .source_file | contains("test-session"))' "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# Re-ingest same file should be a no-op (already indexed)
set +e; CAPTURED_OUTPUT=$(tsm ingest-session "$SESSION_FILE" 2>&1); CAPTURED_EXIT=$?; set -e
assert_contains "ingest-session: re-ingest is no-op" "session unchanged" "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

# ── Watcher test ──────────────────────────────────────────────────────

echo ""
log "=== Watcher ==="

# Create a new file and wait for watcher to pick it up
WATCHER_FILE="$TSM_PROJECT_DIR/notes/watcher-test.md"
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
    if echo "$CAPTURED_OUTPUT" | jq -e 'any(.results[]; .source_file | contains("watcher-test"))' >/dev/null 2>&1; then
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
    if ! echo "$CAPTURED_OUTPUT" | jq -e 'any(.results[]; .source_file | contains("watcher-test"))' >/dev/null 2>&1; then
        pass "watcher: deleted file removed (after manual index fallback)"
    else
        fail "watcher: deleted file still in index" "watcher-test.md still appears in search results"
    fi
fi

# ── Parallel ingest race ─────────────────────────────────────────────
#
# Verifies that concurrent file creations all land in the index without
# dedup drops or lost events. The watcher debounces changes and coalesces a
# burst into one index request, so 20 parallel writes should all be indexed.
# Also checks the reverse direction: concurrent deletions remove all entries.
#
# Regression guard for #149: the watcher used to forward notify `Access(Open)`
# events, so the daemon reading a file to index it re-triggered indexing in an
# infinite loop that starved most of a parallel burst (only ~5/20 indexed).
# The watcher now filters to content events; this exercises that fix on Linux.

echo ""
log "=== 並列投入 race ==="

RACE_COUNT=20
RACE_DIR="$TSM_PROJECT_DIR/notes"

# All race files share the tokens "unique"/"marker" plus an identical Japanese
# phrase, so every `unique-marker-$i` query matches all RACE_COUNT docs. The
# default top-k (5) would then return only the 5 highest-scoring docs, and the
# other 15 would look "missing" even when fully indexed — a search-ranking
# artifact, not an event drop. Request more than RACE_COUNT results so the
# presence check reflects indexing, not top-k truncation. (A genuinely
# unindexed file is absent from results at any -k, so this never hides a drop.)
RACE_K=$((RACE_COUNT + 5))

# Parallel create
for i in $(seq 1 "$RACE_COUNT"); do
    cat > "$RACE_DIR/race-$i.md" <<HEREDOC &
---
status: current
updated: $TODAY
tags: [race]
---

# race-$i

unique-marker-$i — 並列投入テスト用のユニークコンテンツ。
HEREDOC
done
wait

log "Created $RACE_COUNT files in parallel, waiting for debounced batch..."

# Give the 2s debounce + indexer a head start before polling.
sleep 5

# Poll all files with a shared budget. Each iteration checks every missing
# file once; we stop as soon as all are indexed.
RACE_TIMEOUT=60
RACE_ELAPSED=0
MISSING_CREATE=()
while [[ $RACE_ELAPSED -lt $RACE_TIMEOUT ]]; do
    MISSING_CREATE=()
    for i in $(seq 1 "$RACE_COUNT"); do
        if ! search_json "unique-marker-$i" -k "$RACE_K" 2>/dev/null \
             | jq -e "any(.results[]; .source_file | contains(\"race-$i.md\"))" \
               >/dev/null 2>&1; then
            MISSING_CREATE+=("$i")
        fi
    done
    if [[ ${#MISSING_CREATE[@]} -eq 0 ]]; then
        break
    fi
    sleep 2
    RACE_ELAPSED=$((RACE_ELAPSED + 2))
done

if [[ ${#MISSING_CREATE[@]} -eq 0 ]]; then
    pass "race: all $RACE_COUNT parallel files indexed"
else
    fail "race: parallel ingest missed ${#MISSING_CREATE[@]}/$RACE_COUNT files" \
         "not indexed: ${MISSING_CREATE[*]}"
fi

# Parallel delete — verifies concurrent removals also coalesce correctly.
for i in $(seq 1 "$RACE_COUNT"); do
    rm -f "$RACE_DIR/race-$i.md" &
done
wait

log "Deleted $RACE_COUNT files in parallel, waiting for index removal..."
sleep 5

RACE_ELAPSED=0
STILL_PRESENT=()
while [[ $RACE_ELAPSED -lt $RACE_TIMEOUT ]]; do
    STILL_PRESENT=()
    for i in $(seq 1 "$RACE_COUNT"); do
        if search_json "unique-marker-$i" -k "$RACE_K" 2>/dev/null \
           | jq -e "any(.results[]; .source_file | contains(\"race-$i.md\"))" \
             >/dev/null 2>&1; then
            STILL_PRESENT+=("$i")
        fi
    done
    if [[ ${#STILL_PRESENT[@]} -eq 0 ]]; then
        break
    fi
    sleep 2
    RACE_ELAPSED=$((RACE_ELAPSED + 2))
done

if [[ ${#STILL_PRESENT[@]} -eq 0 ]]; then
    pass "race: all $RACE_COUNT parallel deletions removed from index"
else
    fail "race: parallel delete left ${#STILL_PRESENT[@]}/$RACE_COUNT entries" \
         "still indexed: ${STILL_PRESENT[*]}"
fi

# ── Embedder crash recovery ──────────────────────────────────────────
#
# Verifies embedder lifecycle edge cases:
#   1. Baseline: default search succeeds with embedder up.
#   2. After kill: default search fails with an embedder-specific error.
#   3. After kill: --fallback fts-only still returns FTS5 results (degraded).
#   4. After `tsm restart`: embedder ready + default search recovers.
#
# Uses SIGTERM (default). The embedder has no SIGTERM handler, so this
# simulates a hard crash for our purposes — the socket file persists
# until `tsm restart` cleans it up.
#
# Kept last (before Summary) so the forced embedder kill doesn't leak
# into earlier tests.

echo ""
log "=== Embedder crash ==="

EMBEDDER_PID_FILE="$TSM_STATE_DIR/embedder.pid"
if [[ ! -f "$EMBEDDER_PID_FILE" ]]; then
    fail "embedder-crash: PID file missing" "expected $EMBEDDER_PID_FILE"
else
    EMBEDDER_PID=$(cat "$EMBEDDER_PID_FILE")
    if [[ -z "$EMBEDDER_PID" ]]; then
        # PID file exists but is empty — possible race with tsmd's reaper
        # clearing contents between our `-f` check and `cat`.
        fail "embedder-crash: PID file is empty" "$EMBEDDER_PID_FILE"
    else
        # (1) Baseline — prove default search works before the kill.
        # Without this, (2) and (4) could trivially pass if the embedder
        # was already broken due to a prior test or flaky startup.
        run search_json "メロス"
        assert_json "embedder-crash: baseline default search succeeds" \
            '.results | type == "array" and length > 0' \
            "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

        log "Killing embedder (PID $EMBEDDER_PID)..."
        # Swallowing errors: the process may already be gone if tsmd's
        # reaper noticed first. The next assertions verify the consequence.
        kill "$EMBEDDER_PID" 2>/dev/null || true

        # Give tsmd's child reaper a moment to notice the dead process.
        # The accept loop polls `try_wait()` every 100ms, so 2s is ample.
        sleep 2

        # (2) Default search must fail *with* an embedder-specific error.
        # Inline `set +e` (rather than the `run` helper) because we need
        # stderr separately from stdout to assert on the error message —
        # exit code alone could false-pass on DB/socket/parse errors.
        set +e
        CAPTURED_ERR=$(tsm search -q "メロス" -f json 2>&1 >/dev/null)
        CAPTURED_EXIT=$?
        set -e
        assert_fail "embedder-crash: default search fails without embedder" \
            "$CAPTURED_EXIT"
        # Pass `0` (not `$CAPTURED_EXIT`) as the 4th arg: assert_contains
        # short-circuits to fail when its exit_code arg is non-zero, which
        # would mask the string check here. The non-zero exit is already
        # asserted above by assert_fail, so we only need the message check.
        assert_contains "embedder-crash: error mentions embedder" \
            "Embedder is not running" "$CAPTURED_ERR" 0

        # (3) FTS-only fallback still returns results.
        run search_json "メロス" --fallback fts-only
        assert_json "embedder-crash: --fallback fts-only returns FTS results" \
            '.results | type == "array" and length > 0' \
            "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"

        # (4) tsm restart recovers the embedder. Let stderr surface so a
        # restart failure is diagnosable — otherwise `set -e` would abort
        # the script and skip the summary + daemon-log dump at the end.
        log "Restarting daemon to recover embedder..."
        # Capture exit code explicitly: `if ! tsm restart` consumes $? so
        # inside the then-branch $? would always be 0 (the negation result),
        # making the diagnostic message useless.
        set +e
        tsm restart >/dev/null
        RESTART_EXIT=$?
        set -e
        if [[ $RESTART_EXIT -ne 0 ]]; then
            fail "embedder-crash: tsm restart failed" "exit $RESTART_EXIT"
        elif wait_embedder_ready 60; then
            # 60s rather than initial 180s: the model file is already
            # warm in the OS page cache; restart only respawns the
            # process, not the model weights.
            pass "embedder-crash: embedder ready after restart"
            run search_json "メロス"
            assert_json "embedder-crash: default search recovers after restart" \
                '.results | type == "array" and length > 0' \
                "$CAPTURED_OUTPUT" "$CAPTURED_EXIT"
        else
            # Surface a final `tsm status` for diagnostics — the poll loop
            # inside wait_embedder_ready silences stderr to stay quiet.
            DAEMON_STATUS=$(tsm status 2>&1 || true)
            fail "embedder-crash: embedder not ready after restart" \
                 "timeout 60s — daemon status: $DAEMON_STATUS"
        fi

        # Note on `tsm status`: it reports "Embedder: running" based on
        # socket-file existence, not process liveness. Between the kill
        # and the restart the socket lingers, so status would spuriously
        # say "running". We rely on search exit code / error instead.
    fi
fi

# ── ADR-0015 regression: reads stay responsive during reindex ─────────
#
# Kick a background reindex and immediately time a `tsm status` call.
# Before ADR-0015 (reader pool), status blocked on the writer lock and
# could freeze for the full reindex duration. The 2000ms ceiling is a
# generous guard — the freeze it protects against was multi-second to
# indefinite; the read should complete in <100ms with a reader pool.
# After timing, drain the reindex so it doesn't contaminate the next
# block.

echo ""
log "=== ADR-0015 regression: reads responsive during reindex ==="

tsm reindex all >/dev/null 2>&1 &
_reindex_read_pid=$!
_start_ns=$(date +%s%N)
set +e; tsm status >/dev/null 2>&1; _rc=$?; set -e
_elapsed_ms=$(( ($(date +%s%N) - _start_ns) / 1000000 ))
wait "$_reindex_read_pid" 2>/dev/null || true

if [ "$_rc" -ne 0 ]; then
    fail "rw-split: status responsive during reindex" "command failed (exit $_rc)"
elif [ "$_elapsed_ms" -gt 2000 ]; then
    fail "rw-split: status responsive during reindex" \
        "took ${_elapsed_ms}ms (expected < 2000ms)"
else
    pass "rw-split: status responsive during reindex (${_elapsed_ms}ms)"
fi

# Drain the reindex before the next block.
for _i in $(seq 1 30); do
    _doc_json=$(tsm doctor -f json 2>/dev/null)
    if ! echo "$_doc_json" | jq -e '.sections[] | select(.name == "Reindex")' >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
if [ "$_i" -eq 30 ]; then log "WARNING: reindex did not drain within 30s"; fi

# ── ADR-0015 regression: file index preempts in-progress reindex ──────
#
# Kick a background reindex (small batch via TSM_REINDEX_FTS_BATCH_SIZE=10
# set before daemon start), then immediately pipe a real corpus file into
# `tsm index --files-from-stdin`. The reindex yields after each batch so
# the index request should be served before all batches complete.
# The 3000ms ceiling guards against the pre-ADR-0015 behaviour where a
# file index had to wait for the entire reindex to finish.

echo ""
log "=== ADR-0015 regression: file index preempts reindex ==="

tsm reindex fts >/dev/null 2>&1 &
_reindex_write_pid=$!
_start_ns=$(date +%s%N)
set +e; echo "notes/botchan.md" | tsm index --files-from-stdin >/dev/null 2>&1; _rc=$?; set -e
_elapsed_ms=$(( ($(date +%s%N) - _start_ns) / 1000000 ))
wait "$_reindex_write_pid" 2>/dev/null || true

if [ "$_rc" -ne 0 ]; then
    fail "rw-split: index preempts reindex" "command failed (exit $_rc)"
elif [ "$_elapsed_ms" -gt 3000 ]; then
    fail "rw-split: index preempts reindex" \
        "took ${_elapsed_ms}ms (expected preemption < 3000ms)"
else
    pass "rw-split: index preempts reindex (${_elapsed_ms}ms)"
fi

# Drain before cleanup.
for _i in $(seq 1 30); do
    _doc_json=$(tsm doctor -f json 2>/dev/null)
    if ! echo "$_doc_json" | jq -e '.sections[] | select(.name == "Reindex")' >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
if [ "$_i" -eq 30 ]; then log "WARNING: reindex did not drain within 30s"; fi

# ── ADR-0015 regression (issue #151): search stays fast during reindex ─
#
# A burst of searches runs while a vectors reindex re-embeds the whole
# corpus on the writer connection. With the ADR-0015 reader pool, search
# is served from a query_only reader connection that never blocks on the
# reindex writer, so every search must stay well under 2000ms. Before the
# reader pool this would starve: searches blocked behind the reindex
# writer for seconds. (Issue #151 was filed against the older
# `yield_to_search` mechanism, which #266 replaced with the reader pool;
# the reader pool is the property this now guards.)
#
# No-op guard: the e2e corpus is tiny, so a single `reindex vectors`
# finishes in well under the burst duration and most searches would run
# with no reindex in flight — proving nothing. A background loop keeps a
# reindex continuously in flight (the daemon rejects overlapping requests
# cheaply, so each finished pass is immediately re-kicked), and we assert
# via doctor that a reindex was actually observed active.

echo ""
log "=== ADR-0015 regression (issue #151): search responsive during reindex ==="

_keep_reindexing="$TSM_STATE_DIR/.keep_reindexing_151"
: > "$_keep_reindexing"
( while [ -f "$_keep_reindexing" ]; do tsm reindex vectors >/dev/null 2>&1 || true; done ) &
_reindex_loop_pid=$!

# Confirm the contention window opened: wait up to ~5s for doctor to
# report an active Reindex. If it never appears, the burst would be a
# no-op, so fail loudly rather than pass on an untested code path.
_observed_active=0
for _i in $(seq 1 50); do
    if tsm doctor -f json 2>/dev/null | jq -e '.sections[] | select(.name == "Reindex")' >/dev/null 2>&1; then
        _observed_active=1
        break
    fi
    sleep 0.1
done

_slow=0
for _i in $(seq 1 50); do
    _start_ns=$(date +%s%N)
    set +e; tsm search -q "メロス" -k 5 -f json >/dev/null 2>&1; set -e
    _elapsed_ms=$(( ($(date +%s%N) - _start_ns) / 1000000 ))
    if [ "$_elapsed_ms" -gt 2000 ]; then
        _slow=$((_slow + 1))
        echo "  slow search $_i: ${_elapsed_ms}ms"
    fi
done

# Stop the reindex loop, then drain any pass still running in the daemon.
rm -f "$_keep_reindexing"
wait "$_reindex_loop_pid" 2>/dev/null || true

if [ "$_observed_active" -eq 0 ]; then
    fail "rw-split: search responsive during reindex" \
        "reindex never observed in progress; burst was a no-op (corpus too small?)"
elif [ "$_slow" -ne 0 ]; then
    fail "rw-split: search responsive during reindex" \
        "$_slow/50 searches exceeded 2000ms"
else
    pass "rw-split: search responsive during reindex (50/50 < 2000ms)"
fi

# Drain before cleanup.
for _i in $(seq 1 30); do
    _doc_json=$(tsm doctor -f json 2>/dev/null)
    if ! echo "$_doc_json" | jq -e '.sections[] | select(.name == "Reindex")' >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
if [ "$_i" -eq 30 ]; then log "WARNING: reindex did not drain within 30s"; fi

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
