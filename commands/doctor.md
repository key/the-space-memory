---
description: Run tsm health check
user-invocable: true
disable-model-invocation: true
allowed-tools: Bash(tsm *)
---

# The Space Memory — Doctor

以下の JSON はナレッジ検索システムのヘルスチェック結果です。
タイトルは「The Space Memory — Doctor」として、セクションごとに見やすく整形して表示してください。

- status が "ok" のアイテムは ✔ を付ける
- status が "warning" のアイテムは ⚠ を付けて hint を添える
- status が "error" のアイテムは ✘ を付けて hint を添える
- issue_count が 0 なら「All good.」、それ以外は issue 数を表示

!`tsm doctor -f json 2>/dev/null`
