#!/usr/bin/env bash
# Lint the GitHub Actions workflows with actionlint.
#
# Why a wrapper rather than a bare `actionlint` in two places: the suppression
# below needs its reasoning attached, and CI and the prek hook must not drift
# apart on which suppressions are in force.
#
# actionlint also runs shellcheck over every `run:` block, which is the reason
# this is worth having at all — `shellcheck scripts/*.sh` never saw a line of
# the shell embedded in the workflows.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Suppressions. Each one states why, so a later reader can retire it.
#
#   concurrency `queue`: GitHub added this key after actionlint's most recent
#   release, so actionlint rejects a workflow GitHub accepts. Drop this once an
#   actionlint release understands the key — the check will simply pass.
IGNORE=(
    -ignore 'unexpected key "queue" for "concurrency" section'
)

# git invokes hooks in a non-interactive shell where mise's shims may not be on
# PATH. Fall back to `mise which` (mise itself stays on PATH) before giving up,
# so the hook works from a bare `git commit` and never silently skips the check.
ACTIONLINT=actionlint
if ! command -v actionlint >/dev/null 2>&1; then
    ACTIONLINT="$(mise which actionlint 2>/dev/null || true)"
fi
if [[ -z "$ACTIONLINT" ]] || ! command -v "$ACTIONLINT" >/dev/null 2>&1; then
    echo "ERROR: actionlint not found on PATH." >&2
    echo "Install it with \`mise install\`." >&2
    exit 2
fi

cd "$REPO_ROOT"

# `-oneline` keeps each finding to a single grep-able line; actionlint finds
# .github/workflows on its own from the repository root.
"$ACTIONLINT" -no-color -oneline "${IGNORE[@]}"

echo "OK: workflows pass actionlint"
