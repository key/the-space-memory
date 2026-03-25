---
description: Search the knowledge base with a query
---

# Knowledge Search

Search the knowledge base with the user's query using `tsm`.

Run the search command from the project directory:

```bash
cd "$CLAUDE_PROJECT_DIR" && ${CLAUDE_PLUGIN_ROOT}/tsm search -q "$ARGUMENTS" -k 5 -f json --include-content 3
```

After getting the results, present them clearly to the user:

- Show source file paths and section headings
- Include relevant snippets
- Note if any results have `status: outdated`
- If no results found, say so honestly
