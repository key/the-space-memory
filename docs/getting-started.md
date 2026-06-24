# Getting Started

This guide walks through a first-time setup end to end: install the `tsm`
binary, download the embedding model, initialize a workspace, decide what gets
indexed, then start the daemon and run your first search. It links out to the
[Configuration Reference](configuration.md) and
[Command Reference](command-reference.md) for the full detail behind each step.

The whole tour takes a few minutes, most of which is the one-time model
download.

## 1. Platform

| Platform | Status |
|---|---|
| Linux x86_64 | Primary target, CI tested |
| Linux arm64 | Supported, CI build-checked |
| macOS Apple Silicon | Supported |
| macOS x86_64 | Supported |

File watching uses inotify (Linux) / FSEvents (macOS).

## 2. Install

Install the prebuilt `tsm` and `tsmd` binaries with the install script:

```bash
curl -fsSL https://key.github.io/the-space-memory/install.sh | bash
```

The script picks the right archive for your platform, verifies the checksum,
and copies both binaries into `~/.local/bin/`. Make sure that directory is on
your `PATH`. Alternatively, download an archive directly from the
[Releases page](https://github.com/key/the-space-memory/releases), or install
via mise (`"github:key/the-space-memory" = { version = "latest", extract_all = true }`),
which additionally verifies SLSA provenance.

Confirm the binary is reachable:

```bash
tsm --version
```

## 3. One-time setup

`tsm setup` downloads the ruri-v3-30m embedding model and the Japanese WordNet
database into a machine-wide cache. Run it once per machine, not once per
workspace:

```bash
tsm setup
```

This step needs network access and downloads a few hundred MB. It does not
touch any workspace — it only populates the shared cache (under
`$XDG_CACHE_HOME/tsm/` by default).

## 4. Initialize a workspace

A *workspace* is the directory tree you want to search — your notes, a repo, or
a parent directory holding several repos. The directory that holds `tsm.toml`
is the **project root**.

```bash
cd ~/my-notes
tsm init
```

`tsm init` is idempotent and bootstraps the project root: it creates the
database schema and scaffolds default files (`tsm.toml`, `.tsmignore`, and the
`.tsm/` state directory with a default user dictionary, synonyms, and Lua
hooks). Existing user-customized files are never overwritten. The default
`.tsmignore` excludes hidden directories and common build artifacts.

> `tsm` resolves the project root from the current directory when it contains a
> `tsm.toml`. To run from elsewhere, pass `--project-root <DIR>`. See
> [Configuration → Config File Search Order](configuration.md#config-file-search-order).

## 5. Decide what gets indexed

By default — with no `content_dirs` configured — tsm auto-discovers every
subdirectory of the project root and indexes all `.md` files under them,
**recursively**. For many setups that is all you need, and you can skip ahead
to [step 6](#6-start-the-daemon).

To narrow the scope or tune scoring, edit `[[index.content_dirs]]` entries in
`tsm.toml`:

```toml
[[index.content_dirs]]
path = "notes"        # relative to the project root
weight = 1.2          # score multiplier; 1.0 neutral, > 1.0 boosts (no cap)
half_life_days = 120  # time-decay half-life; 0 = timeless

[[index.content_dirs]]
path = "archive"
weight = 0.8          # 0 < weight < 1.0 attenuates
```

A few rules that are easy to trip over (full detail in
[Configuration → content_dirs Details](configuration.md#content_dirs-details)):

- **It is recursive.** Each listed directory is scanned all the way down.
- **It is a scope, not just a label.** Once `content_dirs` is non-empty, only
  the listed trees are indexed — anything not listed (and not nested under a
  listed entry) is skipped.
- **`weight` has no upper bound.** To make one area rank higher, raise its
  weight above `1.0` rather than lowering everything else.
- **Overlaps are fine.** The longest matching prefix wins, so a file is scored
  by exactly one entry — never double-counted.
- **A root catch-all does not carry scoring.** `path = "."` indexes everything
  under the project root, but its `weight` / `half_life_days` never apply
  (those files fall back to the defaults).
- **Root-level files need `path = "."`.** Files sitting directly in the project
  root (e.g. `README.md`, `CLAUDE.md`) are only indexed when a `content_dir`
  resolves to the root itself. Auto-discover and specific-directory modes index
  files under subdirectories, not at the root.

Use `.tsmignore` (gitignore syntax, at the project root) to exclude paths in any
mode.

## 6. Start the daemon

```bash
tsm start
```

This launches `tsmd`, which manages the embedder and file-watcher child
processes. The watcher keeps the index in sync as files change. To start
without the watcher (manual indexing only), use `tsm start --no-watcher`.

## 7. Index your documents

```bash
tsm index
```

Indexing is incremental — only changed chunks are re-processed. With the
watcher running, subsequent edits are picked up automatically; you mainly run
`tsm index` for the initial pass or after bulk changes.

## 8. Search

```bash
tsm search -q "query" -k 5
```

`-k` sets the number of results. For machine-readable output with the source
content of the top hits:

```bash
tsm search -q "query" -k 5 -f json --include-content 3
```

Searches can be filtered by time (`--after`, `--before`, `--recent 30d`,
`--year 2026`) and by path prefix (`--path notes`). See
[Command Reference → tsm search](command-reference.md#tsm-search).

## 9. Verify health

```bash
tsm doctor
```

`tsm doctor` is a read-only diagnostic: it reports the daemon, embedder, and
database state, queue depth, and vector integrity. It never starts the daemon
or creates state, so it is safe to run anytime.

## 10. Keep the index fresh

With the watcher running, additions, edits, and deletions are indexed in real
time. For manual control:

```bash
tsm reindex all        # FTS + vectors, in the background (non-destructive)
tsm reindex fts        # FTS only (e.g. after dictionary changes)
tsm rebuild --apply    # destructive: backup, delete, init, full re-index
```

`tsm rebuild --apply` is required after schema or tokenizer changes. Without
`--apply` it performs a dry run.

## Troubleshooting

- **Search errors with the embedder down.** By default search refuses to run
  without the embedder (to guarantee full hybrid results). Restart it with
  `tsm restart`, or fall back to full-text-only search with
  `--fallback fts-only` (the equivalent `tsm.toml` key is
  `search_fallback = "fts_only"`).
- **"Run `tsm init` first".** The current directory has no `tsm.toml` and no
  `--project-root` was given. `cd` into the workspace, run `tsm init`, or pass
  `--project-root <DIR>`.
- **A directory isn't being searched.** Check that it is under a `content_dirs`
  entry (or that `content_dirs` is empty), and that `.tsmignore` is not
  excluding it.

## Next steps

- [Configuration Reference](configuration.md) — every `tsm.toml` field and
  environment variable
- [Command Reference](command-reference.md) — every subcommand, flag, and
  example
- [Architecture](architecture.md) — how the daemon, embedder, and watcher fit
  together
