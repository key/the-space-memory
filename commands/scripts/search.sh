#!/usr/bin/env bash
set -eu

# Sanitize input: remove shell metacharacters, keep only safe chars
QUERY=$(printf '%s' "$*" | tr -d '`$(){}|;&<>!\\'\'' "' | head -c 500)

[ ${#QUERY} -lt 2 ] && echo '[]' && exit 0

# Prefer system-installed tsm over plugin-bundled one
if command -v tsm >/dev/null 2>&1; then
  TSM="tsm"
elif [ -x "${CLAUDE_PLUGIN_ROOT:-$(dirname "$0")/../..}/tsm" ]; then
  TSM="${CLAUDE_PLUGIN_ROOT:-$(dirname "$0")/../..}/tsm"
else
  echo '[]' && exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"

"$TSM" search -q "$QUERY" -k 5 -f json --include-content 3 2>/dev/null || echo '[]'
