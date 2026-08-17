#!/usr/bin/env bash
# Tests for scripts/lint-automerge-triggers.sh.
#
# Why: the guard's whole value is that it fails when the auto-merge trigger
# list drifts. A guard that silently passes on drift is worse than none — it
# certifies the very stall it was written to prevent — so each way it must fail
# gets a fixture, not just the happy path.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GUARD="$SCRIPT_DIR/lint-automerge-triggers.sh"

if [[ ! -x "$GUARD" ]]; then
    echo "ERROR: $GUARD not found or not executable" >&2
    exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/automerge-triggers.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

failures=0
checks=0

write_automerge() {  # write_automerge <dir> <flow-sequence-body>
    cat >"$1/dependabot-auto-merge.yml" <<EOF
name: Dependabot Auto-merge
on:
  workflow_run:
    workflows: [$2]
    types: [completed]
EOF
}

write_workflow() {  # write_workflow <dir> <file> <name> <trigger>
    cat >"$1/$2" <<EOF
name: $3
on:
  $4:
    paths:
      - "src/**"
EOF
}

# expect_exit <want> <label> <dir>
expect_exit() {
    local want="$1" label="$2" dir="$3" got=0
    checks=$((checks + 1))
    "$GUARD" "$dir" >/dev/null 2>&1 || got=$?
    if [[ "$got" != "$want" ]]; then
        failures=$((failures + 1))
        echo "FAIL: $label — want exit $want, got exit $got" >&2
    fi
}

# --- in sync -----------------------------------------------------------------
d="$WORK/ok"
mkdir -p "$d"
write_automerge "$d" "CI, Lint"
write_workflow "$d" ci.yml CI pull_request
write_workflow "$d" lint.yml Lint pull_request
write_workflow "$d" labeler.yml "PR Labeler" pull_request_target
expect_exit 0 "every pull_request workflow listed, target-only one omitted" "$d"

# --- a pull_request workflow missing from the list ---------------------------
# This is the stall: its completion cannot resume an evaluation blocked on it.
d="$WORK/missing"
mkdir -p "$d"
write_automerge "$d" "CI"
write_workflow "$d" ci.yml CI pull_request
write_workflow "$d" devcontainer.yml DevContainer pull_request
expect_exit 1 "pull_request workflow absent from the list" "$d"

# --- a pull_request_target workflow wrongly listed ---------------------------
d="$WORK/target-listed"
mkdir -p "$d"
write_automerge "$d" "CI, PR Labeler"
write_workflow "$d" ci.yml CI pull_request
write_workflow "$d" labeler.yml "PR Labeler" pull_request_target
expect_exit 1 "pull_request_target-only workflow listed" "$d"

# --- the list names a workflow that does not exist ---------------------------
# A typo disables a trigger with no error anywhere in GitHub.
d="$WORK/typo"
mkdir -p "$d"
write_automerge "$d" "CI, Lnit"
write_workflow "$d" ci.yml CI pull_request
write_workflow "$d" lint.yml Lint pull_request
expect_exit 1 "list names a workflow that does not exist" "$d"

# --- unparseable input fails closed, never passes ----------------------------
d="$WORK/no-name"
mkdir -p "$d"
write_automerge "$d" "CI"
write_workflow "$d" ci.yml CI pull_request
printf 'on:\n  pull_request:\n' >"$d/anonymous.yml"
expect_exit 2 "workflow file without a top-level name" "$d"

d="$WORK/no-list"
mkdir -p "$d"
printf 'name: Dependabot Auto-merge\non:\n  workflow_run:\n    types: [completed]\n' \
    >"$d/dependabot-auto-merge.yml"
write_workflow "$d" ci.yml CI pull_request
expect_exit 2 "auto-merge workflow without a trigger list" "$d"

d="$WORK/empty-list"
mkdir -p "$d"
write_automerge "$d" ""
write_workflow "$d" ci.yml CI pull_request
expect_exit 2 "empty trigger list" "$d"

d="$WORK/no-automerge"
mkdir -p "$d"
write_workflow "$d" ci.yml CI pull_request
expect_exit 2 "auto-merge workflow missing entirely" "$d"

expect_exit 2 "workflow dir does not exist" "$WORK/absent"

if [[ "$failures" -gt 0 ]]; then
    echo "FAILED: $failures of $checks checks" >&2
    exit 1
fi

echo "OK: $checks checks passed"
