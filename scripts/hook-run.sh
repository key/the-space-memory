#!/usr/bin/env bash
# Run a mise-managed tool from a git hook.
#
# Why: git invokes hooks in a non-interactive shell where mise's shims are not
# reliably on PATH. Resolve the tool on PATH first, then fall back to
# `mise which` (mise itself stays on PATH), so hooks work from a bare git
# command and fail loudly instead of silently skipping. Trailing args (e.g.
# files passed by prek) are forwarded verbatim.
#
# Usage: scripts/hook-run.sh <tool> [args...]
set -euo pipefail

name="$1"
shift
tool="$name"

if ! command -v "$tool" >/dev/null 2>&1; then
    resolved="$(mise which "$name" 2>/dev/null || true)"
    if [[ -n "$resolved" ]]; then
        tool="$resolved"
    fi
fi

if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERROR: $name not found on PATH (try \`mise install\`)." >&2
    exit 2
fi

exec "$tool" "$@"
