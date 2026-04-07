---
name: the-space-memory:search
description: |
  Search the cross-workspace knowledge base using hybrid FTS5 + vector search.
  Use when: user asks about past research, notes, decisions, or anything that might be in the knowledge base.
  Examples: "前に調べたLoRaの件どうなってた？", "ナレッジから探して", "〜について調べた記録ある？",
  "〜あったっけ？", "以前まとめた〜", "Search for anything about X", "What did I write about X?"
user-invocable: true
---

# Knowledge Search

Search the knowledge base using `tsm search`.

## Usage

```bash
cd "$CLAUDE_PROJECT_DIR" && "${CLAUDE_PLUGIN_ROOT}bin/tsm" search -q "$ARGUMENTS" -k 5 -f json --include-content 3
```

## Options

| Flag | Description |
|---|---|
| `-q <query>` | Search query |
| `-k <n>` | Number of results (default: 5) |
| `-f json` | JSON output format |
| `--include-content <n>` | Include content for top N results |
| `--recent <duration>` | Filter by recency (e.g., `30d`, `7d`) |
| `--after <date>` | Filter after date (e.g., `2025-01`) |
| `--year <year>` | Filter by year |
| `--path <prefix>` | Filter by file path prefix |
| `--fallback fts-only` | Use FTS-only mode if embedder is down |

## Behavior

1. If `$ARGUMENTS` is provided, use it as the query directly
2. If no arguments, infer the query from the conversation context
3. Run the search and present results with source file paths
4. For deeper investigation, delegate to the `deep-research` agent
