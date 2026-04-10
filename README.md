# The Space Memory

![The Space Memory](docs/assets/cover.png)

[日本語](README.ja.md)

## Overview

A cross-workspace knowledge search engine built in Rust.
Indexes Markdown documents across multiple workspaces and provides hybrid search
combining FTS5 full-text search with vector semantic search (ruri-v3-30m, 256-dim).

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

# 2. Download the ruri-v3-30m model
tsm setup

# 3. Set the document root directory
export TSM_INDEX_ROOT=~/my-notes

# 4. Initialize the database
tsm init

# 5. Start the daemon (embedder + file watcher)
tsm start

# 6. Index your documents
tsm index

# 7. Search
tsm search -q "query" -k 5
```

### What gets indexed

tsm recursively scans `TSM_INDEX_ROOT` for `.md` files.
A typical directory layout:

```text
~/my-notes/              ← TSM_INDEX_ROOT
├── projects/
│   ├── project-a.md
│   └── project-b.md
├── research/
│   └── notes.md
└── journal/
    └── 2026-04.md
```

All Markdown files under `TSM_INDEX_ROOT` are indexed automatically.
The file watcher detects additions, modifications, and deletions in real time.

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
