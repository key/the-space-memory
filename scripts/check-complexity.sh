#!/usr/bin/env bash
# Fail when src/ grows new high-complexity functions beyond the baseline.
#
# Why: keep cyclomatic complexity in check before code leaves the machine.
# Mirrors the `.github/workflows/metrics.yml` complexity gate so a push that
# would fail CI fails locally first. lizard flags functions with CCN > 15;
# up to 13 such warnings are tolerated as the current baseline.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE=13

cd "$REPO_ROOT"

# git invokes hooks in a non-interactive shell where mise's shims may not be on
# PATH. Fall back to `mise which` (mise itself stays on PATH) before giving up,
# so the hook works from a bare `git push` and never silently skips the check.
LIZARD=lizard
if ! command -v lizard >/dev/null 2>&1; then
    LIZARD="$(mise which lizard 2>/dev/null || true)"
fi
if [[ -z "$LIZARD" ]] || ! command -v "$LIZARD" >/dev/null 2>&1; then
    echo "ERROR: lizard not found on PATH." >&2
    echo "Install it with \`mise install\` (provides pipx:lizard)." >&2
    exit 2
fi

report=$("$LIZARD" src/ --language rust -Tcyclomatic_complexity=15 -w 2>&1 || true)
count=$(printf '%s\n' "$report" | grep -c ": warning:" || true)

echo "High-complexity functions: $count (baseline: $BASELINE)"

if [[ "$count" -gt "$BASELINE" ]]; then
    echo "ERROR: new high-complexity functions detected ($count > $BASELINE baseline)" >&2
    echo "" >&2
    printf '%s\n' "$report" >&2
    exit 1
fi

exit 0
