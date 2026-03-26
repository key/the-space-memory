---
description: Search the knowledge base with a query
user-invocable: true
disable-model-invocation: true
---

# Search

```bash
cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}" && ${CLAUDE_PLUGIN_ROOT}/tsm search -q "$ARGUMENTS" -k 5 -f json --include-content 3
```
