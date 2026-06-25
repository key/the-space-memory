# The Space Memory

![The Space Memory](docs/assets/cover.png)

[日本語](README.ja.md) · [Documentation](https://key.github.io/the-space-memory/)

## Overview

A cross-workspace knowledge search engine built in Rust.
Indexes Markdown documents across multiple workspaces and provides hybrid search
combining FTS5 full-text search with vector semantic search (ruri-v3-30m, 256-dim).

## Claude Code Plugin

This repository ships only the `tsm` / `tsmd` binaries. The Claude Code plugin
(skills, agents, hooks) lives in a separate repository,
[`key/tsm-plugin-cc`](https://github.com/key/tsm-plugin-cc), which doubles as
its own plugin marketplace:

```bash
/plugin marketplace add key/tsm-plugin-cc
/plugin install the-space-memory@tsm-plugin-cc
```

The plugin calls the `tsm` CLI, so install `tsm` separately (see below) and
ensure it is on your `PATH`.

## Concept

- **Cross-workspace search** — Index and search across multiple repositories
  (personal notes, work projects, tech notes) from a single orchestration repo
- **Sub-100ms local search** — Both indexing and search run locally
  with no network overhead, completing in under 100ms
- **Transparent Claude Code integration** — Hooks intercept prompts,
  search the knowledge base, and inject relevant context automatically

## Features

- **Hybrid search** — FTS5 + vector search fused via Reciprocal Rank Fusion (RRF)
- **Morphological analysis** — Japanese tokenization via lindera (IPADIC)
- **Semantic search** — ruri-v3-30m embeddings computed locally with candle (no ONNX Runtime)
- **Entity graph** — Automatic entity extraction and link inference
- **Synonym expansion** — WordNet + user-defined CSV for query expansion
- **Session ingestion** — Index Claude Code session transcripts as searchable knowledge
- **Single binary** — No Python, no external runtime dependencies

## Getting Started

### Platform

| Platform | Status |
|---|---|
| Linux x86_64 | Primary target, CI tested |
| Linux arm64 | Supported, CI build-checked |
| macOS Apple Silicon | Supported |
| macOS x86_64 | Supported |

File watching uses inotify (Linux) / FSEvents (macOS).

### Setup

```bash
# 1. Build
cargo build --release

# 2. Download external resources (ruri model + Japanese WordNet DB).
#    System-wide; run once per machine.
tsm setup

# 3. Initialize the workspace from your notes directory:
#    DB schema, default scaffold files (tsm.toml, .tsmignore,
#    .tsm/{user_dict.simpledic,custom_terms.toml, synonyms.csv,
#    hooks/extract/10-md_frontmatter.lua, hooks/score/10-default.lua}),
#    and WordNet/synonym imports. Idempotent — existing
#    user-customized files are never overwritten.
cd ~/my-notes
tsm init

# 5. Start the daemon (embedder + file watcher)
tsm start

# 6. Index your documents
tsm index

# 7. Search
tsm search -q "query" -k 5

# Scope to a directory (absolute or relative to the current directory)
tsm search -q "query" --path notes/
```

`tsm setup` sets `HF_HUB_CACHE` automatically; override it to redirect the
Hugging Face model cache.

### What gets indexed

tsm recursively scans the project root (directory containing `tsm.toml`) for `.md` files.
A typical directory layout:

```text
~/my-notes/              ← project root (contains tsm.toml)
├── projects/
│   ├── project-a.md
│   └── project-b.md
├── research/
│   └── notes.md
└── journal/
    └── 2026-04.md
```

By default (no `content_dirs` configured) every Markdown file under the project
root is indexed. Set `content_dirs` in `tsm.toml` to scope indexing to specific
directories — each listed directory is still scanned recursively, but anything
outside them is skipped. The file watcher detects additions, modifications, and
deletions in real time.

### Maintenance

```bash
# Re-index while daemon is running (non-destructive, background)
tsm reindex all       # FTS + vectors
tsm reindex fts       # FTS only (after dictionary changes)
tsm reindex vectors   # Vectors only (after model changes)

# Rebuild from scratch (destructive, requires daemon stopped)
tsm rebuild           # Dry run (shows DB stats)
tsm rebuild --apply   # Delete DB and rebuild
```

Use `tsm doctor` to check system health and daemon status.

## Lua Hooks

tsm uses embedded Lua (mlua, lua54) for user-editable metadata extraction and
result scoring. Hooks live in `.tsm/hooks/` and are scaffolded by `tsm init`.

### Hook directory layout

```text
.tsm/hooks/
├── extract/
│   └── 10-md_frontmatter.lua   ← extract hook (index time)
└── score/
    └── 10-default.lua          ← score hook (search time)
```

- Only `.lua` files are loaded; non-`.lua` files are ignored.
- Multiple hooks in the same directory are executed in sorted file-name order.
- To disable a hook without deleting it, rename it away from `.lua`
  (e.g. `10-default.lua.disabled`).
- An empty or absent directory falls back to the embedded default hook.

### Extract hooks

Called at index time for each document chunk. Receive a context table:

```lua
-- ctx fields: path, body, frontmatter (top-level YAML keys), metadata (accumulated so far)
function extract(ctx)
  local fm = ctx.frontmatter or {}
  return { status = fm.status, effective_date = fm.updated }
end
```

`ctx.frontmatter` exposes the top-level YAML keys: scalars (string/number/bool)
and sequences (e.g. `tags`, as a Lua array). Nested mappings are not passed.

Return a flat table of scalar values (string, number, boolean). The results of
all extract hooks are shallow-merged (last-wins for duplicate keys) and stored
in `documents.metadata` (JSON column).

### Score hooks

Called at search time for each result. Receive a context table:

```lua
-- ctx fields: metadata (from extract), rrf, source_type, path, half_life_days
-- builtins: decay(date, half_life_days), today()
function score(ctx)
  local m = ctx.metadata or {}
  local penalty = ({ outdated = 0.4 })[m.status] or 1.0
  return penalty * decay(m.effective_date, ctx.half_life_days)
end
```

The snippet above is a minimal example. The shipped default
(`score/10-default.lua`) uses the full status→penalty table:
`superseded=0.2`, `rejected=0.3`, `dropped=0.3`, `deprecated=0.3`,
`outdated=0.4`, `proposed=0.7`, everything else `1.0`.

Each hook returns a multiplier (`>= 0`). The final score is
`rrf × weight × Π(score hooks)`. Invalid returns (negative, NaN, ±Inf) are
treated as `1.0` with a warning.

### Sandboxing and lifecycle

- Lua VMs are sandboxed: no standard library (`io`/`os`/`package` absent),
  64 MiB memory ceiling. Hooks cannot touch the filesystem, network, or processes.
- The daemon validates and loads all hooks at startup (fail-fast on syntax error
  or missing entrypoint). The CLI uses a lazy per-process cache.
- **Editing a hook requires `tsm restart`** — hooks are not reloaded at runtime.
- **The `metadata` column is added automatically** (idempotent migration on connect); existing rows
  have NULL metadata and the searcher synthesizes scoring from `status`/`updated`, so scoring is
  unaffected. To populate `metadata` for existing documents after writing custom extract hooks, run
  `tsm reindex` — a full destructive rebuild is not required.

## Environment Variables

Every setting below can also be set in `tsm.toml` (the `tsm.toml key` column);
the environment variable takes precedence. See
[docs/configuration.md](docs/configuration.md) for the full reference.

| Variable | Default | `tsm.toml` key | Description |
|---|---|---|---|
| `TSM_CONFIG` | *(discovered)* | *(n/a)* | Path to the config file; the directory containing it becomes the project root |
| `TSM_STATE_DIR` | `.tsm` | `state_dir` | Root directory for all tsm state (DB, sockets, PID, logs, user dict) |
| `TSM_CACHE_DIR` | `$XDG_CACHE_HOME/tsm` (else `$HOME/.cache/tsm`) | `cache_dir` | Cache directory for the model and WordNet DB |
| `TSM_EMBEDDER_SOCKET` | `{state_dir}/embedder.sock` | `embedder_socket_path` | UNIX socket for the embedder child process |
| `TSM_DAEMON_SOCKET` | `{state_dir}/daemon.sock` | `daemon_socket_path` | UNIX socket for the `tsmd` daemon |
| `TSM_LOG_DIR` | `{state_dir}/logs` | `log_dir` | Directory for daemon log files |
| `TSM_EMBEDDER_IDLE_TIMEOUT` | `600` | `embedder_idle_timeout_secs` | Idle seconds before the embedder auto-stops (`0` = never). `tsmd` spawns it with `--no-idle-timeout`, so this applies only to standalone runs |
| `TSM_EMBEDDER_BACKFILL_INTERVAL` | `300` | `embedder_backfill_interval_secs` | Seconds between periodic vector backfill checks (`0` = disable) |
| `TSM_SEARCH_FALLBACK` | `error` | `search_fallback` | Behavior when the embedder is down: `error` or `fts_only` |
| `TSM_USER_DICT` | `{state_dir}/user_dict.simpledic` | `user_dict_path` | Path to the lindera user dictionary |
| `TSM_SETUP_LINK_MODE` | `symlink` | `[setup].link_mode` | How `tsm setup` materializes cached resources: `symlink` or `copy` |
| `TSM_INIT_LINK_MODE` | `symlink` | `[init].link_mode` | How `tsm init` links workspace resources to the cache: `symlink` or `copy` |
| `TSM_READER_POOL_SIZE` | CPU cores | `reader_pool_size` | Number of `query_only` reader connections in the daemon's reader pool; caps concurrent reads |
| `TSM_REINDEX_FTS_BATCH_SIZE` | `200` | `reindex_fts_batch_size` | Documents per FTS reindex batch; smaller = finer preemption granularity and more fsync, larger = better reindex throughput |

tsm also honors `RUST_LOG` (log level, default `info`) and `NO_COLOR`
(disable colored output).

## Benchmarks

Performance benches for the search/index pipeline. Currently only the
search latency bench is implemented; indexing benches and the CI
regression gate ship in a follow-up PR (see [#181](https://github.com/key/the-space-memory/issues/181)).

### Prerequisites

- `tsmd` running with embedder ready (`tsm start` and verify via `tsm status`)
- The standard testdata corpus indexed:

  ```bash
  cd tests/e2e/testdata
  tsm init && tsm index
  ```

### Running

```bash
# Search latency (hybrid: FTS5 + vector + entity)
cargo bench --bench search_latency
```

### Embedder call counter

For benches that need to verify embedder call counts, build with the
`bench-counters` feature. Off by default; release builds compile the
counters out entirely.

```bash
cargo build --features bench-counters
```

```rust
use the_space_memory::embedder::counters;

counters::reset_embedder_calls();
// ... run code that calls embed_via_socket_at ...
println!("calls: {}", counters::embedder_call_count());
```

## Documentation

- [Command Reference](docs/command-reference.md) — CLI commands, flags, and usage examples
- [Architecture](docs/architecture.md) — Process architecture and component responsibilities
- [Data Flow](docs/data-flow.md) — Indexing and search flow diagrams
- [Configuration](docs/configuration.md) — Environment variables and config file reference
- [User Dictionary](docs/user-dictionary.md) — Custom dictionary management
- [Design Decisions](decisions/) — ADR (Architecture Decision Records)

## Background

The Space Memory was inspired by [sui-memory](https://zenn.dev/noprogllama/articles/7c24b2c2410213),
which introduced the idea of indexing Claude Code session transcripts into a searchable
database. tsm extends this concept from session records to entire document repositories,
enabling cross-workspace knowledge search.

### Why build from scratch?

Existing tools each had critical gaps for this use case:

- **Notion / GitHub search** — Network-bound; too slow for real-time prompt injection
- **grep** — Sequential scan with no semantic correlation between terms
- **Obsidian** — Excellent Markdown editor, but not designed for AI agent integration

tsm was built to fill these gaps: a local-first, sub-100ms hybrid search engine
that integrates transparently with Claude Code via hooks. The combination of
FTS5 and vector search bridges vocabulary gaps (e.g., matching "shooting" with
"firearms"), and Japanese tokenization via lindera/IPADIC was a key reason to
build a custom solution rather than adapting English-centric tools.

### Name

The name follows sui-memory's naming pattern (prefix + memory), with "space"
representing the multiple repositories treated as a unified search space.
The cover image is an homage to *Hydlide 3*, whose subtitle is
*The Space Memories*.

## License

[MIT](LICENSE)
