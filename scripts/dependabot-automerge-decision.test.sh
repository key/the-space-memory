#!/usr/bin/env bash
# Table-driven tests for scripts/dependabot-automerge-decision.sh.
#
# Why: the decision script is the only place where "is this dependency bump
# safe to merge unattended?" is expressed. It runs in a workflow that merges to
# main without a human in the loop, so its version arithmetic — especially the
# Cargo 0.x rule, where 0.11 -> 0.12 is breaking — needs cases pinned down
# rather than reasoned about at review time.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DECIDE="$SCRIPT_DIR/dependabot-automerge-decision.sh"

if [[ ! -x "$DECIDE" ]]; then
    echo "ERROR: $DECIDE not found or not executable" >&2
    exit 2
fi

failures=0
checks=0

# expect <want> <branch> <title>
expect() {
    local want="$1" branch="$2" title="$3"
    local got
    checks=$((checks + 1))
    got="$("$DECIDE" "$branch" "$title" 2>/dev/null || echo "<exit $?>")"
    if [[ "$got" != "$want" ]]; then
        failures=$((failures + 1))
        echo "FAIL: branch=$branch title=$title" >&2
        echo "      want=$want got=$got" >&2
    fi
}

# expect_exit <code> [args...]
expect_exit() {
    local want="$1"
    shift
    local got=0
    checks=$((checks + 1))
    "$DECIDE" "$@" >/dev/null 2>&1 || got=$?
    if [[ "$got" != "$want" ]]; then
        failures=$((failures + 1))
        echo "FAIL: args=$* want exit $want, got exit $got" >&2
    fi
}

# --- cargo: compatible bumps within Cargo's caret range -> merge -------------
expect merge dependabot/cargo/clap-4.6.4 \
    "deps: bump clap from 4.6.1 to 4.6.4"
expect merge dependabot/cargo/serde-1.0.229 \
    "deps: bump serde from 1.0.228 to 1.0.229"
expect merge dependabot/cargo/quinn-proto-0.11.16 \
    "deps: bump quinn-proto from 0.11.14 to 0.11.16"
expect merge dependabot/cargo/ignore-0.4.28 \
    "deps: bump ignore from 0.4.27 to 0.4.28"
# Build metadata is not part of the compatibility range.
expect merge dependabot/cargo/toml-1.1.3 \
    "deps: bump toml from 1.1.2+spec-1.1.0 to 1.1.3+spec-1.1.0"
# Omitted components default to zero: 1.2 -> 1.3 stays inside ^1.
expect merge dependabot/cargo/foo-1.3 \
    "deps: bump foo from 1.2 to 1.3"

# --- cargo: range-crossing bumps -> hold -------------------------------------
expect hold dependabot/cargo/foo-2.0.0 \
    "deps: bump foo from 1.9.9 to 2.0.0"
# 0.x: the minor component is the significant one, so 0.11 -> 0.12 is breaking.
expect hold dependabot/cargo/foo-0.12.0 \
    "deps: bump foo from 0.11.14 to 0.12.0"
# 0.0.x: every patch is its own compatibility range.
expect hold dependabot/cargo/foo-0.0.4 \
    "deps: bump foo from 0.0.3 to 0.0.4"
expect hold dependabot/cargo/foo-2 \
    "deps: bump foo from 1 to 2"
# Pre-release ordering is subtler than the caret rule; never merge unattended.
expect hold dependabot/cargo/foo-1.0.0 \
    "deps: bump foo from 1.0.0-beta.1 to 1.0.0"
expect hold dependabot/cargo/foo-1.1.0 \
    "deps: bump foo from 1.0.0 to 1.1.0-rc.1"

# --- cargo: unparseable titles -> hold ---------------------------------------
# Grouped updates carry no single from/to pair.
expect hold dependabot/cargo/multi \
    "deps: bump the cargo group with 3 updates"
expect hold dependabot/cargo/foo-1.0.0 \
    "deps: update foo"
expect hold dependabot/cargo/foo-1.0.0 ""

# --- github_actions: always merge -------------------------------------------
expect merge dependabot/github_actions/jdx/mise-action-4.2.1 \
    "ci: bump jdx/mise-action from 4.2.0 to 4.2.1"
# Major action bumps are in scope: CI green is the gate.
expect merge dependabot/github_actions/actions/labeler-7.0.0 \
    "ci: bump actions/labeler from 6.2.0 to 7.0.0"
# SHA-pinned actions carry no version to compare, and must not be held for it.
expect merge dependabot/github_actions/dtolnay/rust-toolchain-2c7215f132e9ebf062739d9130488b56d53c060c \
    "ci: bump dtolnay/rust-toolchain from fa04a1451ff1842e2626ccb99004d0195b455a88 to 2c7215f132e9ebf062739d9130488b56d53c060c"

# --- out of scope -> hold ----------------------------------------------------
expect hold dependabot/npm_and_yarn/foo-1.0.1 \
    "deps: bump foo from 1.0.0 to 1.0.1"
expect hold feat/some-feature "feat: add a thing"
expect hold chore/bench-baseline-update-20260715-014948 \
    "chore(bench): update performance baseline"
# A branch named `dependabot/` with nothing after it has no ecosystem segment.
expect hold dependabot/ "deps: bump foo from 1.0.0 to 1.0.1"
# `dependabot-foo/...` must not be accepted as a Dependabot branch.
expect hold dependabot-fake/cargo/foo-2.0.0 \
    "deps: bump foo from 1.0.0 to 1.0.1"

# --- usage errors ------------------------------------------------------------
expect_exit 2
expect_exit 2 dependabot/cargo/foo-1.0.0
expect_exit 2 a b c

if [[ "$failures" -gt 0 ]]; then
    echo "FAILED: $failures of $checks checks" >&2
    exit 1
fi

echo "OK: $checks checks passed"
