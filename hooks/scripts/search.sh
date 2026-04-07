#!/usr/bin/env bash
set -eu

LOG="/tmp/tsm-hook-search.log"

# stdin から JSON を読む
INPUT=$(cat)
echo "[$(date -Iseconds)] RAW_INPUT='${INPUT:0:300}'" >> "$LOG"
QUERY=$(echo "$INPUT" | jq -r '.prompt // .user_prompt // empty' 2>/dev/null || true)

echo "[$(date -Iseconds)] query='${QUERY:0:80}' PLUGIN_ROOT='${CLAUDE_PLUGIN_ROOT:-}' PROJECT_DIR='${CLAUDE_PROJECT_DIR:-}'" >> "$LOG"

# クエリが短すぎる場合はスキップ
if [ ${#QUERY} -lt 3 ]; then
  echo "[$(date -Iseconds)] SKIP: query too short (${#QUERY} chars)" >> "$LOG"
  exit 0
fi

# Prefer system-installed tsm over plugin-bundled one
# (bundled binary may have hardcoded paths from Docker build)
if command -v tsm >/dev/null 2>&1; then
  TSM="tsm"
elif [ -x "${CLAUDE_PLUGIN_ROOT:-}bin/tsm" ]; then
  TSM="${CLAUDE_PLUGIN_ROOT:-}bin/tsm"
else
  echo "[$(date -Iseconds)] SKIP: tsm not found" >> "$LOG"
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"

# 検索実行（tsmd が未起動なら自動起動される）
RESULT=$("$TSM" search --query "$QUERY" --format json 2>>"$LOG") || {
  echo "[$(date -Iseconds)] FAIL: tsm search exited with $?" >> "$LOG"
  exit 0
}

# 結果が空なら何も出力しない
if [ -z "$RESULT" ] || [ "$RESULT" = "null" ] || [ "$RESULT" = "[]" ]; then
  echo "[$(date -Iseconds)] EMPTY: no results" >> "$LOG"
  exit 0
fi

COUNT=$(echo "$RESULT" | jq 'length' 2>/dev/null || echo "?")
echo "[$(date -Iseconds)] OK: $COUNT results" >> "$LOG"

# additionalContext 形式で出力
jq -n --argjson results "$RESULT" '{
  hookSpecificOutput: {
    hookEventName: "UserPromptSubmit",
    additionalContext: ("ナレッジ検索結果 (自動):\n" + ($results | map(
      "- [\(.source_file)] \(.section_path): \(.snippet[0:100])"
      + (if (.related_docs // [] | length) > 0
         then "\n  関連: " + (.related_docs | map("[\(.file_path)](\(.link_type))") | join(", "))
         else "" end)
    ) | join("\n")))
  }
}'
