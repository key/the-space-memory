#!/bin/bash
# Claude Code notification hook
# Parses stdin JSON for event-specific notifications
# Sends via ntfy, OS-native desktop notification, and terminal bell

# Terminal bell
printf '\a'

# Read stdin
INPUT=$(cat)

# jq が無い場合はベル通知のみで終了
if ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)

# Build title and message based on event type
case "$EVENT" in
  Stop)
    TITLE="タスク完了"
    MSG=$(echo "$INPUT" | jq -r '(.last_assistant_message // "")[0:200]' 2>/dev/null)
    [ -z "$MSG" ] && MSG="応答が完了しました"
    ;;
  Notification)
    TYPE=$(echo "$INPUT" | jq -r '.notification_type // empty' 2>/dev/null)
    case "$TYPE" in
      permission_prompt) TITLE="許可が必要" ;;
      *)                 TITLE="入力が必要" ;;
    esac
    MSG=$(echo "$INPUT" | jq -r '.message // empty' 2>/dev/null)
    [ -z "$MSG" ] && MSG="確認が必要です"
    ;;
  *)
    TITLE="Claude Code"
    MSG="確認が必要です"
    ;;
esac

# ntfy push notification (requires both NTFY_URL and NTFY_TOPIC)
if [ -n "${NTFY_URL:-}" ] && [ -n "${NTFY_TOPIC:-}" ]; then
  curl -s --fail --connect-timeout 2 --max-time 5 \
    -H "Title: $TITLE" -d "$MSG" \
    "${NTFY_URL}/$NTFY_TOPIC" >/dev/null 2>&1 || true
fi

# OS-native desktop notification
case "$(uname -s)" in
  Linux*)
    if command -v notify-send &>/dev/null; then
      notify-send "$TITLE" "$MSG" 2>/dev/null || true
    fi
    ;;
  Darwin*)
    osascript -e 'on run argv
display notification (item 1 of argv) with title (item 2 of argv)
end run' -- "$MSG" "$TITLE" 2>/dev/null || true
    ;;
esac
