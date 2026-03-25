#!/usr/bin/env bash
set -euo pipefail

# stdin から JSON を読む
INPUT=$(cat)
QUERY=$(echo "$INPUT" | jq -r '.user_prompt // empty' 2>/dev/null)

# クエリが短すぎる場合はスキップ
[ ${#QUERY} -lt 3 ] && exit 0

TSM="${CLAUDE_PLUGIN_ROOT}/tsm"
[ ! -x "$TSM" ] && exit 0

cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"

SOCKET="/tmp/tsm-embedder.sock"

# embedder デーモンが起動していなければバックグラウンドで起動
if [ ! -S "$SOCKET" ]; then
  "$TSM" embedder-start >/dev/null 2>&1 &
  for _ in $(seq 1 50); do
    [ -S "$SOCKET" ] && break
    sleep 0.1
  done
fi

# 検索実行
RESULT=$("$TSM" search --query "$QUERY" --format json 2>/dev/null) || exit 0

# 結果が空なら何も出力しない
[ -z "$RESULT" ] || [ "$RESULT" = "null" ] || [ "$RESULT" = "[]" ] && exit 0

# additionalContext 形式で出力
jq -n --argjson results "$RESULT" '{
  additionalContext: ("ナレッジ検索結果 (自動):\n" + ($results | map("- [\(.source_file)] \(.section_path): \(.snippet[0:100])") | join("\n")))
}'
