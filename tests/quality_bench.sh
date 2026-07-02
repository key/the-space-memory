#!/usr/bin/env bash
# Search quality benchmark orchestration.
#
# Measures Precision@5 / MRR / nDCG@5 for the default hybrid search path
# (FTS5 + vector + entity, real embedder) against the hand-graded golden
# corpus under tests/golden/corpus/, then gates the run against
# tests/golden/baseline.json. Also measures a genuine FTS5-only pass
# (embedder stopped) for comparison; that pass is recorded but not gated
# (see tests/quality_bench.rs module docs for why an embedder-up
# `--fallback fts_only` run would not be a true FTS-only measurement).
#
# This mirrors tests/e2e.sh's isolated-environment pattern and the
# "Isolated benchmark / perf-test environment" section of CLAUDE.md: a
# gitignored project-local dir (not /tmp — keeps the UNIX socket path
# under SUN_LEN) with its own state dir and sockets, so a running dev
# daemon and the repo's own .tsm/ are never touched.
#
# Prerequisites:
#   - `cargo build --release` (tsm/tsmd on PATH, or pass --bin-dir)
#   - ruri-v3-30m model available (`tsm setup`, or HF_HUB_CACHE pre-warmed)
#   - jq installed
#
# Usage:
#   bash tests/quality_bench.sh [--update-baseline] [--force]
#
#   --update-baseline   After the hybrid measurement, overwrite
#                        tests/golden/baseline.json with the freshly
#                        measured numbers. Use when a quality-improving
#                        change intentionally raises the baseline — see
#                        README "Benchmarks" for the update policy.
#                        Refused if the gate against the EXISTING baseline
#                        failed (i.e. this run regressed) unless --force is
#                        also passed — --update-baseline must not become a
#                        way to quietly launder a regression into the
#                        baseline. Not refused when no baseline exists yet
#                        (first-time bootstrap: there is nothing to protect
#                        against, the gate fails only because the file is
#                        missing).
#   --force             Required alongside --update-baseline to overwrite
#                        an existing baseline after a failing gate. Has no
#                        effect without --update-baseline.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$REPO_ROOT/.qbench"
BASELINE_FILE="$REPO_ROOT/tests/golden/baseline.json"

UPDATE_BASELINE=0
FORCE=0
for arg in "$@"; do
    case "$arg" in
    --update-baseline) UPDATE_BASELINE=1 ;;
    --force) FORCE=1 ;;
    *)
        echo "unknown argument: $arg" >&2
        exit 1
        ;;
    esac
done

log() { echo -e "\033[1m[quality-bench]\033[0m $*"; }

# Cleanup on exit (invoked via trap below)
# shellcheck disable=SC2329
cleanup() {
    local status=$?
    log "Tearing down..."
    if [[ -n "${TSM_STATE_DIR:-}" ]] && [[ -d "$TSM_STATE_DIR" ]]; then
        (cd "$BENCH_DIR/proj" 2>/dev/null && tsm stop 2>/dev/null) || true
    fi
    rm -rf "$BENCH_DIR"
    exit "$status"
}
trap cleanup EXIT

rm -rf "$BENCH_DIR"
mkdir -p "$BENCH_DIR/proj" "$BENCH_DIR/state/models" "$BENCH_DIR/out"

# ── Environment (pinned; never touches the repo's own .tsm/ or a running dev daemon) ──
export TSM_STATE_DIR="$BENCH_DIR/state"
export TSM_EMBEDDER_SOCKET="$BENCH_DIR/e.sock"
export TSM_DAEMON_SOCKET="$BENCH_DIR/d.sock"
export TSM_EMBEDDER_IDLE_TIMEOUT=0
# TSM_CACHE_DIR is intentionally left unset here so the machine-wide model
# cache (default ~/.cache/tsm, or whatever the caller already exported —
# e.g. CI's workflow-level TSM_CACHE_DIR) is reused across runs instead of
# re-downloading the ~60MB model every invocation. `tsm setup` is a no-op
# once the model is present.
log "tsm setup (no-op if the model is already cached)"
tsm setup

# ── Corpus: copy + substitute date placeholders (same mechanism as e2e.sh) ──
cp -r "$SCRIPT_DIR/golden/corpus/"* "$BENCH_DIR/proj/"

# Only __TODAY__ / __1Y_AGO__ are used by tests/golden/corpus/ today. Add a
# placeholder here (and to this substitution) only once a corpus doc needs it.
TODAY="$(date +%Y-%m-%d)"
ONE_YEAR_AGO="$(date -v-1y +%Y-%m-%d 2>/dev/null || date -d '1 year ago' +%Y-%m-%d)"

find "$BENCH_DIR/proj" -name '*.md' -print0 | xargs -0 sed -i.bak \
    "s/__TODAY__/$TODAY/g; s/__1Y_AGO__/$ONE_YEAR_AGO/g"
find "$BENCH_DIR/proj" -name '*.bak' -delete

# ── Index the corpus with the daemon (and embedder) up ──
log "tsm init && tsm start"
(cd "$BENCH_DIR/proj" && tsm init && tsm start)

log "Waiting for embedder..."
# Up to 180s: a cold fp32 model load on a fresh runner (CI, or a machine
# where `tsm setup` just materialized the model) can take a while — a
# tighter timeout risks a flaky false failure, not just a slower pass.
EMBEDDER_READY=0
for _ in $(seq 1 90); do
    STATUS_OUT="$(cd "$BENCH_DIR/proj" && tsm status 2>/dev/null || true)"
    echo "$STATUS_OUT" | grep -q "Embedder:.*running" && {
        EMBEDDER_READY=1
        break
    }
    sleep 2
done
if [[ "$EMBEDDER_READY" -ne 1 ]]; then
    echo "embedder did not become ready" >&2
    (cd "$BENCH_DIR/proj" && tsm status) || true
    exit 1
fi

log "tsm index"
(cd "$BENCH_DIR/proj" && tsm index)

log "Waiting for vector backfill..."
# `tsm status` prints "Vectors N/N (100%)" once fully backfilled — but right
# after `tsm index` returns, the backfill count can transiently read
# "Vectors: 0/0 (100%)" (0 of 0 chunks counted as needing vectors yet, which
# is trivially "100%"). A bare `grep "(100%)"` matches that immediately and
# exits the wait before the embedder has done any work, so the measurement
# would silently run against zero-vector chunks. Require a non-zero count on
# BOTH sides of the ratio. Logging the full status each poll (not just the
# match) makes a genuine timeout diagnosable from CI output alone.
VECTORS_READY=0
for _ in $(seq 1 90); do
    STATUS_OUT="$(cd "$BENCH_DIR/proj" && tsm status 2>/dev/null || true)"
    echo "$STATUS_OUT" | grep -E -q 'Vectors: *[1-9][0-9]*/[1-9][0-9]* \(100%\)' && {
        VECTORS_READY=1
        break
    }
    log "  ...not yet: $(echo "$STATUS_OUT" | grep 'Vectors:' || echo '(no Vectors line)')"
    sleep 2
done
if [[ "$VECTORS_READY" -ne 1 ]]; then
    echo "vector backfill did not reach a genuine 100% (non-zero N/N) in time" >&2
    (cd "$BENCH_DIR/proj" && tsm status) || true
    exit 1
fi

export TSM_QB_PROJECT_DIR="$BENCH_DIR/proj"
export TSM_QB_OUT_DIR="$BENCH_DIR/out"

# ── Pass 1: hybrid (embedder up, vectors backfilled) ──
log "Measuring: hybrid"
(cd "$REPO_ROOT" && cargo test --release --test quality_bench measure_hybrid -- --ignored --exact --nocapture)

# ── Pass 2: fts_only (embedder stopped — a genuine FTS5-only run) ──
log "Stopping embedder for FTS-only pass..."
EMBEDDER_PID_FILE="$TSM_STATE_DIR/embedder.pid"
if [[ -f "$EMBEDDER_PID_FILE" ]]; then
    EMBEDDER_PID="$(cat "$EMBEDDER_PID_FILE")"
    [[ -n "$EMBEDDER_PID" ]] && kill "$EMBEDDER_PID" 2>/dev/null || true
    sleep 2
else
    echo "embedder PID file missing; skipping FTS-only pass" >&2
fi

log "Measuring: fts_only"
(cd "$REPO_ROOT" && cargo test --release --test quality_bench measure_fts_only -- --ignored --exact --nocapture)

# ── Gate: hybrid vs committed baseline ──
# Baseline existence is checked BEFORE the gate runs: a missing baseline
# makes gate_against_baseline fail too (nothing to compare against), but
# that failure means "no prior baseline to protect," not "this run
# regressed" — --update-baseline must treat those two cases differently.
BASELINE_EXISTED=0
[[ -f "$BASELINE_FILE" ]] && BASELINE_EXISTED=1

log "Gating hybrid results against tests/golden/baseline.json"
if (cd "$REPO_ROOT" && cargo test --release --test quality_bench gate_against_baseline -- --ignored --exact --nocapture); then
    GATE_STATUS=0
else
    GATE_STATUS=1
fi

if [[ "$UPDATE_BASELINE" -eq 1 ]]; then
    if [[ "$BASELINE_EXISTED" -eq 1 ]] && [[ "$GATE_STATUS" -ne 0 ]] && [[ "$FORCE" -ne 1 ]]; then
        log "Refusing to update tests/golden/baseline.json: this run failed the gate against the existing baseline (a regression), and --update-baseline would silently launder that into the new baseline. Fix the regression, or pass --force if this failure is expected and you intend to accept it as the new baseline."
    else
        log "Updating tests/golden/baseline.json from this run's hybrid measurement"
        jq -n --slurpfile hybrid "$BENCH_DIR/out/hybrid.json" '{hybrid: $hybrid[0]}' \
            >"$BASELINE_FILE"
    fi
fi

log "fts_only (informational, not gated):"
jq '{mean_precision_at_5, mean_mrr, mean_ndcg_at_5, mean_latency_ms}' "$BENCH_DIR/out/fts_only.json"

exit "$GATE_STATUS"
