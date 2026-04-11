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
elif [ -x "${CLAUDE_PLUGIN_ROOT:-}/bin/tsm" ]; then
  TSM="${CLAUDE_PLUGIN_ROOT:-}/bin/tsm"
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
if [ -z "$RESULT" ] || [ "$RESULT" = "null" ]; then
  echo "[$(date -Iseconds)] EMPTY: no results" >> "$LOG"
  exit 0
fi

COUNT=$(echo "$RESULT" | jq '.results | length' 2>/dev/null || echo "0")
TOTAL_HITS=$(echo "$RESULT" | jq '.total_hits // 0' 2>/dev/null || echo "0")
echo "[$(date -Iseconds)] OK: $COUNT results (total_hits: $TOTAL_HITS)" >> "$LOG"

if [ "$COUNT" = "0" ]; then
  exit 0
fi

BUDGET="${TSM_SNIPPET_BUDGET:-1000}"

# Build XML output following Anthropic prompting best practices
# See: docs/claude-code/claude-code-prompt-format.md
XML=$(echo "$RESULT" | jq -r --arg query "$QUERY" --argjson budget "$BUDGET" --argjson total_hits "$TOTAL_HITS" '
  .results | length as $count |
  reduce to_entries[] as $entry (
    {xml: "", used: 0};
    $entry.value as $item |
    ($entry.key + 1) as $idx |
    ($item.snippet | length) as $slen |

    # snippet budget check
    (if (.used + $slen) <= $budget then true else false end) as $ok |

    # source attributes
    (if $item.status != null and $item.status != ""
     then " status=\"\($item.status)\""
     else "" end) as $st |

    # related element (omit when empty)
    (if ($item.related_docs // [] | length) > 0
     then "<related>" + ($item.related_docs | map(.file_path) | join(", ")) + "</related>\n"
     else "" end) as $rel |

    # snippet element (self-closing when over budget)
    (if $ok
     then "<snippet>\n\($item.snippet)\n</snippet>\n"
     else "<snippet/>\n" end) as $snip |

    # score: truncate to 3 decimal places
    ($item.score | tostring | split(".") |
     .[0] + "." + ((.[1] // "000") | .[0:3])) as $score |

    {
      xml: (.xml
        + "<result index=\"\($idx)\" score=\"\($score)\">\n"
        + "<source type=\"\($item.source_type // "unknown")\"\($st)>\($item.source_file)</source>\n"
        + "<section>\($item.section_path)</section>\n"
        + $snip + $rel
        + "</result>\n"),
      used: (if $ok then .used + $slen else .used end)
    }
  ) |
  "<knowledge_search query=\"\($query | gsub("\""; "&quot;") | gsub("&"; "&amp;") | gsub("<"; "&lt;"))\" count=\"\($count)\" total=\"\($total_hits)\">\n\(.xml)</knowledge_search>"
')

# additionalContext 形式で出力
jq -n --arg context "$XML" '{
  hookSpecificOutput: {
    hookEventName: "UserPromptSubmit",
    additionalContext: $context
  }
}'
