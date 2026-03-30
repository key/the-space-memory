# The Space Memory

A cross-workspace knowledge search engine. Hybrid search with FTS5 + vector (ruri-v3-30m).

## Commands

```bash
# Build
cargo build --release

# Run all tests
cargo test

# Run tests for a specific module
cargo test --lib chunker
cargo test --lib frontmatter

# Coverage (maintain 90%+, excluding embedder/main)
cargo llvm-cov --html
cargo llvm-cov --ignore-filename-regex '(embedder|main|cli)\.rs' --fail-under-lines 90

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt --check

# Lint (Markdown / Shell / YAML / TOML)
rumdl check <file>.md
shellcheck <file>.sh
yamllint <file>.yml
taplo check <file>.toml
```

## Architecture

```text
src/
├── lib.rs              — Crate root
├── main.rs             — CLI entry point (clap)
├── cli.rs              — CLI command implementations
├── config.rs           — Configuration (TSM_* env vars, config file, scoring params)
├── db.rs               — SQLite (rusqlite) DB init & connection management
├── indexer.rs           — Indexer (diff detection, FTS5/vector registration)
├── searcher.rs          — FTS5 + vector search, RRF fusion, scoring
├── embedder.rs          — candle + ruri-v3-30m inference, UNIX socket daemon
├── chunker.rs           — Markdown → H2/H3/paragraph chunking
├── session_chunker.rs   — Claude session JSONL → Q&A chunking
├── frontmatter.rs       — YAML frontmatter parser
├── tokenizer.rs         — Morphological analysis via lindera (with user dictionary)
├── entity.rs            — Entity graph (link inference)
├── classifier.rs        — Query classification (entity extraction)
├── doc_links.rs         — Inter-document link analysis
├── synonyms.rs          — Synonym expansion, WordNet import
├── temporal.rs          — Temporal filter expression parsing
└── user_dict.rs         — Dictionary candidate collection & CSV export
```

- **FTS5**: lindera tokenization + unicode61 tokenizer
- **Vector search**: ruri-v3-30m (256-dim) semantic search. Embedder daemon runs on UNIX socket
- **Scoring**: RRF (Reciprocal Rank Fusion) combining FTS5 and vector results. Time decay + status penalty applied
- **DB schema changes require `rebuild --force`** (e.g. FTS tokenizer changes)

## Testing

- **TDD required** — Write tests first, then implement to pass them
- **90%+ coverage** — Enforced via `cargo llvm-cov --fail-under-lines 90` in CI
- **Unit tests required** — All pub functions must have tests in `#[cfg(test)] mod tests`
- **AAA pattern** — Arrange (setup + state cleanup like `clear_vectors`) → Act → Assert
- DB tests use in-memory SQLite (`:memory:`) to prevent state leakage
- Embedder tests should use mockable trait design
- Tests must not depend on external daemon state (embedder, etc.)

## Build & Deploy

Build from a host container via Docker, then reference the binary from hooks/skills.

```bash
docker build -t the-space-memory /path/to/the-space-memory
```

## DevContainer

- Base image: `mcr.microsoft.com/devcontainers/base:ubuntu`
- Tool management via mise (`.mise.toml`). Minimal devcontainer features
- Claude Code installed via native installer (not npm/features)
- Secrets stored in `.env` (git-ignored)

## MCP

- Serena MCP via Docker (`ghcr.io/oraios/serena:latest`). Config in `.mcp.json`

## Gotchas

- **Hook stdin JSON key is `prompt`** (not `user_prompt`). Hook output must wrap `additionalContext` in `hookSpecificOutput: { hookEventName, additionalContext }`
- ruri safetensors have no tensor name prefix.
  candle's ModernBert::load expects `model.` prefix — key names are remapped at load time
- Use `rusqlite`'s bundled feature (don't depend on system SQLite)
- Embedder daemon auto-stops after 10 min idle. Check with `doctor`, restart if needed
- Search works without embedder (FTS5-only fallback)

## License Compatibility

Verify license compatibility when adding dependencies. This project is **MIT** licensed.

| Project License | Allowed Dependencies | Not Allowed |
|---|---|---|
| MIT | MIT, BSD, ISC, Apache-2.0, Unlicense | GPL, LGPL, AGPL, MPL (conditional) |

- Ask the user when compatibility is uncertain
- devDependencies (test/build tools) are exempt from license restrictions
