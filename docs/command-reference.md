# Command Reference

Complete reference for all `tsm` CLI subcommands.

## Table of Contents

- [Global Flags](#global-flags)
- [Lifecycle Commands](#lifecycle-commands)
  - [tsm init](#tsm-init)
  - [tsm start](#tsm-start)
  - [tsm stop](#tsm-stop)
  - [tsm restart](#tsm-restart)
  - [tsm setup](#tsm-setup)
  - [tsm reload](#tsm-reload)
- [Search and Index](#search-and-index)
  - [tsm search](#tsm-search)
  - [tsm index](#tsm-index)
  - [tsm ingest-session](#tsm-ingest-session)
  - [tsm vector-fill](#tsm-vector-fill)
- [Diagnostics](#diagnostics)
  - [tsm status](#tsm-status)
  - [tsm doctor](#tsm-doctor)
- [Maintenance](#maintenance)
  - [tsm reindex](#tsm-reindex)
  - [tsm rebuild](#tsm-rebuild)
  - [tsm import-wordnet](#tsm-import-wordnet)
- [Dictionary Management](#dictionary-management)
  - [tsm dict update](#tsm-dict-update)
  - [tsm dict reject](#tsm-dict-reject)
  - [tsm dict add](#tsm-dict-add)
  - [tsm dict rm](#tsm-dict-rm)
- [Synonym Management](#synonym-management)
  - [tsm synonym add](#tsm-synonym-add)
  - [tsm synonym rm](#tsm-synonym-rm)
  - [tsm synonym export](#tsm-synonym-export)
  - [tsm synonym import](#tsm-synonym-import)
- [Temporal Query Syntax](#temporal-query-syntax)
- [Output Formats](#output-formats)

---

## Global Flags

These flags apply to every subcommand.

| Flag | Description |
|---|---|
| `--project-root <DIR>` | Directory holding `tsm.toml`, treated as the project root. Used when the current directory has no `tsm.toml`; `content_dirs` paths resolve against it (the `state_dir`, default `.tsm/`, stays relative to the working directory). When neither resolves a root, commands fail (except `tsm init` / `tsm setup`, which fall back to the current directory). See ADR-0009 §2. |

---

## Lifecycle Commands

These commands run directly (not routed through the daemon).

### tsm init

Initialize the workspace: schema, scaffold files, WordNet import, user
synonym import. All steps are idempotent and re-runnable.

```text
tsm init
```

Performs the following per-workspace setup steps. Every file write uses
`OpenOptions::create_new`, so existing user-customized files are never
overwritten:

1. Creates the SQLite database at `$TSM_DB_PATH`
   (default: `{state_dir}/tsm.db`).
2. Writes default scaffold files when missing:
   - `.tsmignore` (project root) — `.gitignore`-style ignore patterns
   - `tsm.toml` (project root) — fully commented configuration template
   - `.tsm/user_dict.simpledic` — empty (lindera user dictionary)
   - `.tsm/custom_terms.toml` — header comment with format example
   - `.tsm/synonyms.csv` — header comment for user synonym pairs
3. Imports Japanese WordNet synonyms from `.tsm/wnjpn.db` if present.
   If missing, logs a warning and continues — run `tsm setup` to
   download the file, then re-run `tsm init` to import.
4. Imports user-defined synonyms from `.tsm/synonyms.csv` (insert-only — unlike
   [tsm synonym import](#tsm-synonym-import) it never deletes, so re-running
   `tsm init` never drops pairs added with `tsm synonym add`).

**Flags:** none

**Example:**

```bash
tsm setup       # one-time: fetch ruri model + WordNet DB
tsm init        # per-workspace: schema, scaffold, synonym import
```

---

### tsm start

Start the `tsmd` daemon (embedder + file watcher).

```text
tsm start [--no-watcher]
```

Spawns `tsmd` as a detached background process. Waits up to 30 seconds for the
daemon socket to become ready. If the daemon is already running, exits immediately.
Stale sockets are removed automatically.

**Flags:**

| Flag | Description |
|---|---|
| `--no-watcher` | Skip starting the file watcher child process |

**Examples:**

```bash
# Start daemon with file watcher (default)
tsm start

# Start daemon without file watcher (manual indexing only)
tsm start --no-watcher
```

---

### tsm stop

Stop the `tsmd` daemon.

```text
tsm stop
```

Sends a shutdown request to the running daemon. If the daemon socket exists
but is unreachable, removes the stale socket and logs a warning.

**Flags:** none

**Example:**

```bash
tsm stop
```

---

### tsm restart

Stop and start the daemon.

```text
tsm restart
```

Equivalent to running `tsm stop` followed by `tsm start`.

**Flags:** none

**Example:**

```bash
tsm restart
```

---

### tsm setup

Download external resources (embedding model + WordNet DB). System-wide;
no workspace DB writes. Run once per machine; re-run only when the
upstream resources change.

```text
tsm setup
```

Pure resource-fetch layer:

1. Downloads `cl-nagoya/ruri-v3-30m` model files (`config.json`,
   `tokenizer.json`, `model.safetensors`) from HuggingFace Hub
   and copies them to `.tsm/models/ruri-v3-30m/`.
2. Downloads Japanese WordNet (`wnjpn.db.gz`) from GitHub and
   decompresses it to `.tsm/wnjpn.db`.

Importing WordNet synonyms into the workspace DB is `tsm init`'s job.
After running `tsm setup` for the first time, run `tsm init` (or re-run
it) so the freshly downloaded WordNet DB gets imported.

**Flags:** none

**Example:**

```bash
tsm setup
tsm init        # imports WordNet synonyms into the workspace DB
```

---

### tsm reload

Reload `tsm.toml` configuration without restarting the daemon.

```text
tsm reload
```

Daemon-routed command — auto-starts `tsmd` if not running. Applies config
changes that do not require a full restart. Warnings about non-reloadable
settings are printed to stderr.

**Flags:** none

**Example:**

```bash
# Edit tsm.toml, then apply without downtime
tsm reload
```

---

## Search and Index

These commands are daemon-routed: `tsm` forwards them to `tsmd` via a UNIX socket,
auto-starting the daemon if it is not running.

### tsm search

Search indexed documents.

```text
tsm search -q <query> [options]
```

Performs hybrid search (FTS5 + vector) fused via Reciprocal Rank Fusion (RRF).
Temporal expressions embedded in the query are automatically extracted and
applied as date filters (see [Temporal Query Syntax](#temporal-query-syntax)).

**Flags:**

| Flag | Short | Type | Default | Description |
|---|---|---|---|---|
| `--query` | `-q` | string | *(required)* | Search query |
| `--top-k` | `-k` | integer | `5` | Maximum number of results |
| `--format` | `-f` | `text`\|`json` | `text` | Output format |
| `--include-content` | | integer | | Include full file content for top N results (JSON only) |
| `--after` | | date | | Return only documents after this date |
| `--before` | | date | | Return only documents before this date |
| `--recent` | | duration | | Return documents from the last N days/weeks/months |
| `--year` | | integer | | Return documents from a specific year |
| `--path` | | string | | Scope to a directory (absolute or CWD-relative, repeatable — OR logic) |
| `--fallback` | | `error`\|`fts-only` | `error` | Behavior when embedder is unavailable |

**Date format for `--after` / `--before`:** `YYYY-MM-DD`, `YYYY-MM`, or `YYYY`.

**Duration format for `--recent`:** `Nd` (days), `Nw` (weeks), `Nm` (months).
Example: `30d`, `2w`, `3m`.

**`--path` flag:** Scopes results to a directory. Accepts an **absolute path or
a path relative to the current working directory** (resolved to an absolute path
before matching; `.`/`..` are resolved lexically). Matching is at a **directory
boundary**, so `--path daily` matches `daily/notes/x.md` but not `daily-report/…`;
a trailing slash is optional (`daily` ≡ `daily/`). Multiple `--path` flags are
combined with OR logic (any match). A path that resolves outside the indexed
content, or matches nothing, returns no results (not an error); an empty string
is rejected. Matching is case-insensitive (ASCII), consistent with SQLite's
`LIKE`.

**`--fallback` flag:** When `error` (default), search fails if the embedder is
not running. When `fts-only`, falls back to full-text search only. Note the
CLI value is hyphenated (`fts-only`); the equivalent `tsm.toml` key uses an
underscore (`search_fallback = "fts_only"`).

**Examples:**

```bash
# Basic search
tsm search -q "Rust async runtime"

# Return top 10 results in JSON
tsm search -q "memory management" -k 10 -f json

# Filter by date range
tsm search -q "release notes" --after 2025-01-01 --before 2026-01-01

# Documents from the last 30 days
tsm search -q "meeting notes" --recent 30d

# Documents from 2025
tsm search -q "architecture decisions" --year 2025

# Filter to a specific subdirectory
tsm search -q "config" --path daily/

# Multiple path prefixes (OR)
tsm search -q "API design" --path projects/ --path research/

# Include full content for top 3 results
tsm search -q "deployment" -f json --include-content 3

# FTS-only mode (no embedder required)
tsm search -q "lindera tokenizer" --fallback fts-only
```

---

### tsm index

Index documents from the configured content directories.

```text
tsm index [--files-from-stdin]
```

Without `--files-from-stdin`, recursively scans the directories configured in
`tsm.toml` (`content_dirs`) — only those trees are indexed, all the way down.
If `content_dirs` is not configured, it instead recursively scans every
subdirectory of the project root. Exclusions (`.tsmignore`, the forced
`.git/` / `.tsm/` excludes, and the extension allowlist) apply in both cases.

With `--files-from-stdin`, reads file paths (one per line) from stdin.
Each path is resolved relative to the project root.

Index updates are incremental: only changed chunks are re-indexed.

**Flags:**

| Flag | Description |
|---|---|
| `--files-from-stdin` | Read file paths from stdin instead of scanning directories |

**Examples:**

```bash
# Index all documents
tsm index

# Index only changed files (from git diff)
git diff --name-only HEAD | tsm index --files-from-stdin

# Index a specific directory
find ~/my-notes/daily -name "*.md" | tsm index --files-from-stdin
```

---

### tsm ingest-session

Ingest a Claude Code session JSONL file as searchable knowledge.

```text
tsm ingest-session <session_file>
```

Parses Claude session transcripts (JSONL format) and indexes Q&A pairs as
chunks. Skips unchanged files based on content hash.

**Arguments:**

| Argument | Description |
|---|---|
| `<session_file>` | Path to the `.jsonl` session file |

**Example:**

```bash
tsm ingest-session ~/.claude/projects/my-project/session-abc123.jsonl
```

---

### tsm vector-fill

Fill missing vector embeddings for indexed chunks.

```text
tsm vector-fill [--batch-size N]
```

Processes chunks that have been indexed via FTS5 but do not yet have vector
embeddings. Requires the embedder (`tsmd --embedder`) to be running.
If the daemon is running, delegates to it.

**Flags:**

| Flag | Type | Default | Description |
|---|---|---|---|
| `--batch-size` | integer | `64` | Number of chunks to embed per batch |

**Example:**

```bash
# Fill missing vectors with default batch size
tsm vector-fill

# Use a larger batch for faster processing
tsm vector-fill --batch-size 128
```

---

## Diagnostics

These commands are daemon-routed — auto-starts `tsmd` if not running.

### tsm status

Show current system status.

```text
tsm status
```

Displays a summary of daemon, embedder, watcher, backfill, and data statistics.

**Flags:** none

**Example output:**

```text
=== The Space Memory Status ===

  Daemon:    running (PID 12345)
  Embedder:  running (since 10m ago, PID 12346)
  Watcher:   running (since 10m ago)

  Documents: 1234
  Chunks:    5678
  Vectors:   5678
```

**Example:**

```bash
tsm status
```

---

### tsm doctor

Run health checks and report system issues.

```text
tsm doctor [-f json]
```

Checks database integrity, embedder availability, vector coverage, and
dictionary state. Outputs a formatted report with pass/warn/fail indicators.

**Flags:**

| Flag | Short | Type | Default | Description |
|---|---|---|---|---|
| `--format` | `-f` | `text`\|`json` | `text` | Output format |

**Example text output:**

```text
╭─ Knowledge Search Doctor ──────────────────────────╮
│                                                     │
│  Database                                           │
│    ✔ DB: /home/user/.tsm/tsm.db (12.3 MB)          │
│    ✔ Documents: 1234                                │
│    ✔ Chunks: 5678                                   │
│                                                     │
│  Embedder                                           │
│    ✔ Running (idle timeout: 600s)                   │
│    ✔ Vectors: 5678 (matches all chunks)             │
│                                                     │
│  All good.                                          │
│                                                     │
╰─────────────────────────────────────────────────────╯
```

**Example JSON output fields:**

```json
{
  "sections": [
    {
      "name": "Database",
      "items": [
        { "status": "ok", "message": "DB: /home/user/.tsm/tsm.db (12.3 MB)" },
        { "status": "ok", "message": "Documents: 1234" },
        { "status": "ok", "message": "Chunks: 5678" }
      ]
    },
    {
      "name": "Embedder",
      "items": [
        { "status": "ok", "message": "Running (idle timeout: 600s)" },
        { "status": "ok", "message": "Vectors: 5678 (matches all chunks)" }
      ]
    }
  ],
  "issue_count": 0
}
```

**Examples:**

```bash
tsm doctor
tsm doctor -f json
```

---

## Maintenance

### tsm reindex

Re-index in background while the daemon is running (non-destructive).

```text
tsm reindex <kind>
```

Sends a reindex request to the running daemon. The daemon processes the
reindex in batches, yielding to search requests between batches.

**Arguments:**

| Argument | Description |
|---|---|
| `all` | Re-tokenize FTS and re-compute vectors |
| `fts` | Re-tokenize FTS only (after dictionary changes) |
| `vectors` | Re-compute vectors only (after model changes) |

**Requires:** Running daemon (`tsm start`).

**Examples:**

```bash
# Re-index FTS after adding words to user dictionary
tsm reindex fts

# Re-index everything
tsm reindex all

# Check progress
tsm doctor
```

---

### tsm rebuild

Rebuild the database from scratch (destructive).

```text
tsm rebuild [--apply]
```

Without `--apply`: dry run showing database size, chunk count, and vector count.

With `--apply`: backs up the database, deletes it, re-initializes, and runs a
full index.

**Flags:**

| Flag | Description |
|---|---|
| `--apply` | Actually perform the rebuild (without: dry run) |

**Requires:** Daemon must not be running (`tsm stop` first) when using `--apply`.

**Examples:**

```bash
# Dry run — see what would be rebuilt
tsm rebuild

# Rebuild
tsm stop && tsm rebuild --apply
```

---

### tsm import-wordnet

Import Japanese WordNet synonyms into the database.

```text
tsm import-wordnet <wnjpn.db>
```

Imports synonym pairs from a Japanese WordNet SQLite database (`wnjpn.db`)
into the local synonyms table. Used for query expansion during search.

`tsm setup` downloads and imports WordNet automatically. This command is
for manual import from a custom path.

**Arguments:**

| Argument | Description |
|---|---|
| `<wnjpn.db>` | Path to the Japanese WordNet SQLite database |

**Example:**

```bash
tsm import-wordnet ~/downloads/wnjpn.db
```

---

## Dictionary Management

### tsm dict update

Show or apply user dictionary candidates.

```text
tsm dict update [--threshold N] [--apply]
```

Without `--apply`: dry run — shows candidate words that appear frequently
enough to be added to the user dictionary.

With `--apply`: writes the dictionary file and triggers FTS re-index. If the
daemon is running, the FTS re-index is sent via IPC (no need to stop). If the
daemon is stopped, FTS is rebuilt directly. No git operations are performed —
if you want the dictionary under version control, commit the file yourself.

**Flags:**

| Flag | Type | Default | Description |
|---|---|---|---|
| `--threshold` | integer | `5` | Minimum frequency for a word to be a candidate |
| `--apply` | | | Write CSV and trigger FTS re-index |

**Examples:**

```bash
# Show candidates with default threshold
tsm dict update

# Show candidates with higher threshold
tsm dict update --threshold 10

# Apply changes (works with or without daemon)
tsm dict update --apply
```

---

### tsm dict reject

Manage the dictionary reject list.

```text
tsm dict reject [--apply] [--all]
```

The reject list (`reject_words.txt`) prevents specific words from being added
to the user dictionary.

Without flags: shows words currently in `reject_words.txt` that are pending
sync.

`--apply`: syncs `reject_words.txt` to the database.

`--all`: shows all rejected words stored in the database.

`--apply` and `--all` are mutually exclusive.

**Flags:**

| Flag | Description |
|---|---|
| `--apply` | Sync `reject_words.txt` to the database |
| `--all` | Show all rejected words in the database |

**Examples:**

```bash
# Sync reject list to DB
tsm dict reject --apply

# Show all rejected words
tsm dict reject --all
```

---

### tsm dict add

Accept one word into the user dictionary.

```text
tsm dict add <surface> [<yomi>]
```

Adds a single term as **accepted**, removing it from the reject list if present.
Use this for compounds lindera mis-splits (e.g. `ハンドロード` → `ハンド` + `ロード`),
which never surface as frequency candidates and so cannot be added via
`dict update`. The accepted set is the authority in the database; the change
regenerates `user_dict.simpledic` and triggers an FTS re-index so the tokenizer
picks it up (via the daemon if running, otherwise a direct rebuild).

The optional reading (`yomi`) is stored but not yet used for matching (search is
surface-based today). Omit it for an all-kana surface, where the surface is its
own reading. A surface containing kanji with no reading is accepted with a
warning, storing the surface as a substitute.

**Examples:**

```bash
# Add a mis-split compound (all-kana: reading defaults to the surface)
tsm dict add ハンドロード

# Add with an explicit reading
tsm dict add 宇宙記憶 うちゅうきおく
```

---

### tsm dict rm

Reset one word to pending.

```text
tsm dict rm <word>
```

Removes the word from whichever side it is on — the user dictionary (accepted)
or the reject list — returning it to **pending**. If the word was accepted, the
change regenerates `user_dict.simpledic` and triggers an FTS re-index. Resetting
a word that was never registered is an error (there is nothing to reset).

**Examples:**

```bash
# Stop treating a word as a dictionary term
tsm dict rm ハンドロード
```

---

## Synonym Management

`tsm dict` manages the lindera **user dictionary** (tokenization). Synonyms are a
separate axis — query-expansion pairs applied at search time — and live under
`tsm synonym`. The DB is the authority; `export` / `import` move pairs between it
and CSV text. They default to **stdout / stdin** so they compose like `pg_dump`;
pass `--file <PATH>` to read or write a file directly.

The four subcommands operate only on `source = 'user'` pairs. WordNet pairs
(`tsm import-wordnet`) and learned pairs are left untouched.

### tsm synonym add

Add a single synonym pair to the database.

```text
tsm synonym add <a> <b>
```

Inserts the pair with `source = 'user'` (score 0.7, no decay). Words are
lowercased, trimmed, and stored in a canonical order. Adding a pair that already
exists with a lower score upgrades it to a user pair. Identical or empty words
are rejected.

```bash
tsm synonym add 猟銃 散弾銃
```

### tsm synonym rm

Remove user synonym pair(s) from the database.

```text
tsm synonym rm <a> [<b>]
```

With both words, removes the single pair `(a, b)` (order-insensitive). With one
word, removes every user pair involving it. Only `source = 'user'` rows are
deleted; a matching WordNet pair is reported and left intact.

```bash
tsm synonym rm 猟銃 散弾銃   # remove one pair
tsm synonym rm 猟銃          # remove all user pairs involving 猟銃
```

### tsm synonym export

Export user synonym pairs as CSV (DB → stdout, or `--file`).

```text
tsm synonym export [--file <PATH>]
```

Emits only `source = 'user'` pairs as `a,b` lines, sorted for stable diffs,
under a header comment. Writes to stdout by default; the pair count is printed to
stderr so the CSV stream stays clean. With `--file <PATH>`, writes the CSV to
that file and prints the count to stdout.

```bash
tsm synonym export                          # dump CSV to stdout
tsm synonym export | grep lora              # compose (pairs are stored lowercase)
tsm synonym export --file .tsm/synonyms.csv  # refresh the git-tracked file
```

### tsm synonym import

Import user synonym pairs from CSV (stdin → DB, or `--file`).

```text
tsm synonym import [--file <PATH>]
```

Reads CSV from stdin by default, or from `--file <PATH>`. Mirrors the input onto
the `source = 'user'` subset: pairs in the input are inserted, and user pairs
absent from it are deleted. Pairs from other sources (e.g. WordNet) are not
affected. Inverse of `export`; the round-trip is exact over the user subset.
`tsm init` imports `.tsm/synonyms.csv` automatically when it exists.

Because import mirrors (absent pairs are deleted), an empty or all-malformed
input would wipe every user pair. As a guard, import **refuses** when the input
parses to zero pairs while user pairs exist — delete them explicitly with
`tsm synonym rm` instead. (Reading from an interactive terminal is also rejected,
so the command never blocks waiting for input that isn't coming.)

**CSV format:**

```csv
# Comments start with #
猟銃,散弾銃
猟銃,鉄砲
LoRa,LPWAN
```

Two columns, no header. Lines starting with `#` are ignored.

```bash
# Pipe pairs straight in
printf '猟銃,散弾銃\n' | tsm synonym import

# Or import from a file
tsm synonym import --file .tsm/synonyms.csv
```

---

## Temporal Query Syntax

Temporal expressions embedded in search queries are automatically extracted and
converted to date filters. The matched expression is removed from the query
before search.

CLI flags (`--after`, `--before`, `--recent`, `--year`) take precedence over
query-embedded expressions.

### Single-Token Keywords

| Expression | Meaning |
|---|---|
| `先月` | Last calendar month |
| `今月` | Current month (no upper bound) |
| `去年` / `昨年` | Last year |
| `一昨年` / `おととし` | Two years ago |
| `今年` | This year (no upper bound) |
| `最近` / `少し前` | Last N days (configured via `RECENT_DAYS`, default 30) |
| `先週` | Last 7 days |
| `半年前` | Last 180 days |
| `年末` | November–December of current (or previous) year |
| `年始` / `年初` | January–February of current (or previous) year |

### Relative N + Unit Patterns

| Pattern | Meaning |
|---|---|
| `N年前` | N years ago (1 year = 365 days) |
| `N週間前` / `N週前` | N weeks ago |
| `N日前` | N days ago |
| `Nヶ月前` / `Nか月前` | N months ago (1 month = 30 days) |

### Specific Month Pattern

| Pattern | Meaning |
|---|---|
| `N月の` / `N月に` | Month N of the current year; if N is in the future, uses the previous year |

### CLI Flag Formats

| Flag | Format | Examples |
|---|---|---|
| `--recent` | `Nd`, `Nw`, `Nm` | `30d`, `2w`, `3m` |
| `--after` | `YYYY`, `YYYY-MM`, `YYYY-MM-DD` | `2025`, `2025-06`, `2025-06-15` |
| `--before` | `YYYY`, `YYYY-MM`, `YYYY-MM-DD` | `2026`, `2026-01`, `2026-01-01` |
| `--year` | `YYYY` | `2025` |

### Examples

```bash
# Query-embedded temporal expression
tsm search -q "先月のミーティングメモ"
tsm search -q "去年のアーキテクチャ決定"
tsm search -q "3ヶ月前のリリースノート"
tsm search -q "6月のスプリント振り返り"

# CLI flag overrides query expression
tsm search -q "最近のバグ報告" --recent 7d

# Explicit date range
tsm search -q "release notes" --after 2025-01-01 --before 2026-01-01
```

---

## Output Formats

### Text Format (search)

Default output for `tsm search`. One result per block:

```text
1. [markdown] projects/api-design.md — ## Authentication (score: 0.8421)
   Token-based authentication using JWT. Refresh tokens are stored in...
   status: active
   related:
     - [wiki_link] projects/security.md (strength: 0.85)

2. [session] sessions/2025-06-10.jsonl — Q: How to handle auth? (score: 0.7103)
   A: Use short-lived JWT access tokens with refresh token rotation...
```

Fields:

| Field | Description |
|---|---|
| Result number | Sequential index starting at 1 |
| `[source_type]` | `markdown` for `.md` files, `session` for JSONL sessions |
| File path | Relative path from the project root |
| Section path | Heading path or Q&A label |
| Score | RRF-fused relevance score |
| Snippet | Relevant excerpt |
| Status | Frontmatter `status` field (if present) |
| Related docs | Inferred document links with link type and strength |

### JSON Format (search)

Output for `tsm search -f json`. Returns a JSON array:

```json
[
  {
    "source_file": "projects/api-design.md",
    "source_type": "markdown",
    "section_path": "## Authentication",
    "snippet": "Token-based authentication using JWT...",
    "score": 0.8421,
    "status": "active",
    "related_docs": [
      {
        "file_path": "projects/security.md",
        "link_type": "wiki_link",
        "strength": 0.85
      }
    ],
    "content": "Full file content here..."
  }
]
```

The `content` field is only present when `--include-content N` is used and
the result is within the top N.
