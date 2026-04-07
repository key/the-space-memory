---
name: the-space-memory:setup
description: Interactive setup wizard to create or update tsm.toml configuration.
user-invocable: true
---

# Setup — tsm.toml Configuration Wizard

Create or update `tsm.toml` in the user's project root via step-by-step questions.

## Process

1. Check if `tsm.toml` already exists in `$CLAUDE_PROJECT_DIR`
   - If yes, read it and ask what to change
   - If no, start fresh setup

2. Ask each question one at a time using AskUserQuestion. Do NOT dump all options at once.

### Step 1: content_dirs

First, scan the workspace to suggest directories:

```bash
find "$CLAUDE_PROJECT_DIR"/.. -maxdepth 2 -name "*.md" -type f 2>/dev/null | sed 's|/[^/]*$||' | sort -u | head -20
```

Then ask:

> Which directories should be indexed? I found these candidates with .md files:
> (list directories found)
>
> You can pick from these or specify your own paths. Separate multiple paths with commas.

### Step 2: search_fallback

Ask:

> When the embedder (vector search) is down, how should search behave?
>
> 1. **error** — Refuse to search (default, recommended)
> 2. **fts_only** — Fall back to text-only search

### Step 3: Session indexing

Ask:

> Should Claude Code session history be indexed for search?
> (This lets you search past conversations)
>
> - **yes** (default) — Index sessions with weight 0.3
> - **no** — Skip session indexing

## After Questions

Generate `tsm.toml` from the answers, write it, and suggest:

```bash
tsm rebuild --force   # Initial index build
tsm doctor            # Verify installation
```

## tsm.toml Format Reference

```toml
# Root directory containing workspaces
index_root = "/workspaces"

# Behavior when embedder is down: "error" or "fts_only"
# search_fallback = "error"

[index]
content_dirs = [
    { path = "/workspaces/notes", weight = 1.0, half_life_days = 90.0 },
]

[index.claude_session]
weight = 0.3
half_life_days = 30.0
```

- `weight` — Scoring weight, higher = more important (default: 1.0)
- `half_life_days` — Time decay half-life in days (default: 90.0)
