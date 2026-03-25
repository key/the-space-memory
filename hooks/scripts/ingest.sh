#!/usr/bin/env bash
set -euo pipefail

# stdin から JSON を読む
INPUT=$(cat)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)

[ -z "$SESSION_ID" ] && exit 0

TSM="${CLAUDE_PLUGIN_ROOT}/tsm"
[ ! -x "$TSM" ] && exit 0

cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"

# セッション JSONL ファイルを探す
SESSIONS_DIR="$HOME/.claude/projects/-workspaces-workspace"
JSONL_FILE="$SESSIONS_DIR/$SESSION_ID.jsonl"

[ ! -f "$JSONL_FILE" ] && exit 0

"$TSM" ingest-session "$JSONL_FILE" >/dev/null 2>&1 &
