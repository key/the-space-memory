---
name: search
description: Search the cross-workspace knowledge base using hybrid FTS5 + vector search.
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
