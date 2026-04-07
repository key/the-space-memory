#!/usr/bin/env bash
set -euo pipefail

FILE=$(jq -r '.tool_input.file_path // empty') || exit 0
[[ -z "$FILE" || "$FILE" == "null" ]] && exit 0

# .md ファイルのみ対象
[[ "$FILE" != *.md ]] && exit 0

# Prefer system-installed tsm over plugin-bundled one
if command -v tsm >/dev/null 2>&1; then
  TSM="tsm"
elif [ -x "${CLAUDE_PLUGIN_ROOT:-}/bin/tsm" ]; then
  TSM="${CLAUDE_PLUGIN_ROOT:-}/bin/tsm"
else
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"

# PROJECT_ROOT からの相対パスに変換
# tsm.toml の project_root を基準にする（デフォルト /workspaces）
REL_PATH="${FILE#/workspaces/}"

# 相対パスに変換できなかった場合（/workspaces 以外のパス）はスキップ
[ "$REL_PATH" = "$FILE" ] && exit 0

echo "$REL_PATH" | "$TSM" index --files-from-stdin >/dev/null 2>&1
