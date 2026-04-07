#!/usr/bin/env bash
set -eu

# stdin から JSON を読む
INPUT=$(cat)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null || true)

[ -z "$SESSION_ID" ] && exit 0

# Prefer system-installed tsm over plugin-bundled one
if command -v tsm >/dev/null 2>&1; then
  TSM="tsm"
elif [ -x "${CLAUDE_PLUGIN_ROOT:-}/bin/tsm" ]; then
  TSM="${CLAUDE_PLUGIN_ROOT:-}/bin/tsm"
else
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"

# セッション JSONL ファイルを探す
SESSIONS_DIR="$HOME/.claude/projects/-workspaces-workspace"
JSONL_FILE="$SESSIONS_DIR/$SESSION_ID.jsonl"

[ ! -f "$JSONL_FILE" ] && exit 0

"$TSM" ingest-session "$JSONL_FILE" >/dev/null 2>&1
