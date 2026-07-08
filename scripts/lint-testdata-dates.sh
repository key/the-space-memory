#!/usr/bin/env bash
# Detect hard-coded ISO dates (YYYY-MM-DD) in tests/e2e/testdata/**.
#
# Why: time-decay scoring (session half-life 30d, note half-life 90d) makes
# fixed-date testdata flaky as the calendar advances. Use placeholders
# (__TODAY__, __1Y_AGO__, __3M_AGO__, ...) which `tests/e2e.sh` substitutes
# at runtime. See CLAUDE.md "Testing" section.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TESTDATA_DIR="$REPO_ROOT/tests/e2e/testdata"

if [[ ! -d "$TESTDATA_DIR" ]]; then
    echo "ERROR: testdata dir not found at $TESTDATA_DIR" >&2
    exit 2
fi

# Find ISO dates that are NOT inside a __PLACEHOLDER__ token.
# Allowed forms:  __TODAY__, __1Y_AGO__, __3M_AGO__, __<UPPER_SNAKE>__
# Disallowed:     2026-04-01, 2025-12-31, ...
violations=$(grep -rEn '[0-9]{4}-[0-9]{2}-[0-9]{2}' "$TESTDATA_DIR" || true)

if [[ -z "$violations" ]]; then
    echo "OK: no hard-coded dates in $TESTDATA_DIR"
    exit 0
fi

echo "ERROR: hard-coded ISO dates found in tests/e2e/testdata/" >&2
echo "Use placeholders like __TODAY__ / __1Y_AGO__ / __3M_AGO__ instead." >&2
echo "See CLAUDE.md 'Testing' section for rationale." >&2
echo "" >&2
echo "$violations" >&2
exit 1
