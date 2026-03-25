#!/usr/bin/env bash
set -euo pipefail

# jq が無い環境ではフックをスキップする
if ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

FILE=$(jq -r '.tool_input.file_path') || exit 0
[[ -z "$FILE" || "$FILE" == "null" ]] && exit 0

CLAUDE_PROJECT_DIR="${CLAUDE_PROJECT_DIR:-}"
# CLAUDE_PROJECT_DIR が未設定の場合はスキップ
[[ -z "$CLAUDE_PROJECT_DIR" ]] && exit 0
# プロジェクト外のファイル（memory等）はスキップ
[[ "$FILE" != "$CLAUDE_PROJECT_DIR"/* ]] && exit 0

case "$FILE" in
  *.md)         command -v rumdl >/dev/null 2>&1 && rumdl check "$FILE" ;;
  *.sh)         command -v shellcheck >/dev/null 2>&1 && shellcheck "$FILE" ;;
  *.yml|*.yaml) command -v yamllint >/dev/null 2>&1 && yamllint "$FILE" ;;
  *.toml)       command -v taplo >/dev/null 2>&1 && taplo check "$FILE" ;;
esac
