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

# Coverage (maintain 90%+, excluding entry points, modes, and infra)
cargo llvm-cov --html
cargo llvm-cov \
  --ignore-filename-regex \
  '(embedder|main|cli|tsmd|tsm_watcher|status|logging|daemon_mode|embedder_mode|watcher_mode|child|backfill)\.rs' \
  --fail-under-lines 90

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt --check

# Lint (Markdown / Shell / YAML / TOML)
rumdl check <file>.md
shellcheck <file>.sh
yamllint <file>.yml
taplo check <file>.toml

# E2E tests (requires release build + model download)
bash tests/e2e.sh
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
├── embedder.rs          — candle + ruri-v3-30m inference (pure library)
├── chunker.rs           — Markdown → H2/H3/paragraph chunking
├── session_chunker.rs   — Claude session JSONL → Q&A chunking
├── frontmatter.rs       — YAML frontmatter parser
├── tokenizer.rs         — Morphological analysis via lindera (with user dictionary)
├── entity.rs            — Entity graph (link inference)
├── classifier.rs        — Query classification (entity extraction)
├── doc_links.rs         — Inter-document link analysis
├── synonyms.rs          — Synonym expansion, WordNet import
├── temporal.rs          — Temporal filter expression parsing
├── user_dict.rs         — Dictionary candidate collection & CSV export
├── daemon.rs            — Daemon request handler (server-side dispatch)
├── daemon_protocol.rs   — IPC message protocol definitions
├── ipc.rs               — IPC wire framing (length-prefixed message read/write)
├── logging.rs           — Log initialization & configuration
├── status.rs            — Daemon status reporting
├── test_utils.rs        — Shared test helpers
└── bin/tsmd/
    ├── main.rs          — tsmd entry point, mode dispatch (--embedder / --fs-watcher)
    ├── daemon_mode.rs   — Daemon mode (accept loop, client handling)
    ├── embedder_mode.rs — Embedder child process (socket server, model inference)
    ├── watcher_mode.rs  — FS watcher child process (file change → Index IPC)
    ├── child.rs         — Child process management (spawn, reap, stop)
    └── backfill.rs      — Vector backfill orchestration
```

- **FTS5**: lindera tokenization + unicode61 tokenizer
- **Vector search**: ruri-v3-30m (256-dim) semantic search. Embedder child process (`tsmd --embedder`) runs on UNIX socket
- **Scoring**: RRF (Reciprocal Rank Fusion) combining FTS5 and vector results. Time decay + status penalty applied
- **DB schema changes require `rebuild --force`** (e.g. FTS tokenizer changes)

## Data Flow

```text
  tsmd (daemon main process)
  ┌──────────────────────────────────────────────────────┐
  │  daemon.sock ◄── tsm CLI                             │
  │     │                                                │
  │  accept loop ──► handle_request ──► DB read/write    │
  │                                                      │
  │  backfill threads ──► embedder.sock ──► chunks_vec   │
  └──────────────────────────────────────────────────────┘
        │ spawn                      │ spawn
        ▼                            ▼
  ┌──────────────────┐    ┌─────────────────────────┐
  │ tsmd --embedder  │    │ tsmd --fs-watcher       │
  │ (pure inference) │    │ (file change → Index)   │
  │ embedder.sock    │    │ daemon.sock client      │
  │ no DB access     │    │ no DB access            │
  └──────────────────┘    └─────────────────────────┘
```

**Ownership:**

- **tsmd (daemon)** — sole DB owner. All reads/writes go through here
- **tsmd --embedder** — stateless inference server. No DB access
- **tsmd --fs-watcher** — stateless file monitor. Sends Index requests to daemon via daemon.sock

## Design Principles

- **Embedder child process** (`tsmd --embedder`) — Model inference
  latency hiding. Must NOT take on unrelated concerns
- **Watcher child process** (`tsmd --fs-watcher`) — File system monitoring
  via OS-native events (inotify/FSEvents). Sends Index requests to daemon
- **Vector writes are always async** — Callers enqueue, embedder
  processes in background. FTS5 fallback if vectors not yet ready
- **Incremental over full rebuild** — Chunk-level content hashing
  for diff-based index updates
- **Transactions for batch DB writes** — Wrap inserts in transactions
  to avoid per-statement fsync in WAL mode
- **Doctor as single observability surface** — All daemon health,
  queue status, and data integrity checks via `tsm doctor`

## Testing

- **TDD required** — Red → Green → Refactor cycle:
  1. Write a failing test that defines the expected behavior
  2. Write minimal code to make the test pass
  3. Refactor while keeping tests green
- **90%+ coverage** — Enforced via `cargo llvm-cov --fail-under-lines 90` in CI
- **Unit tests required** — All pub functions must have tests in `#[cfg(test)] mod tests`
- **AAA pattern** — Arrange (setup + state cleanup like `clear_vectors`) → Act → Assert
- DB tests use in-memory SQLite (`:memory:`) to prevent state leakage
- Embedder tests should use mockable trait design
- Tests must not depend on external daemon state (embedder, etc.)

## Branch Naming

Branch names must follow `<type>/<description>` format.
The PR labeler workflow (`.github/labeler.yml`) maps prefixes to labels:

| Prefix | Label |
|---|---|
| `feat/` | enhancement |
| `fix/` | bug |
| `docs/` | documentation |
| `perf/` | performance |

## Plugin Structure

This project is a Claude Code plugin (`--plugin-dir` or marketplace install).

```text
.claude-plugin/
└── plugin.json            — Plugin manifest
skills/
├── search/SKILL.md        — /the-space-memory:search (knowledge search)
├── doctor/SKILL.md        — /the-space-memory:doctor (health check)
└── setup/SKILL.md         — /the-space-memory:setup (tsm.toml wizard)
agents/
└── deep-research.md       — Deep research sub-agent
hooks/
├── hooks.json             — Hook event definitions
└── scripts/
    ├── search.sh          — UserPromptSubmit: auto-search
    ├── index-file.sh      — PostToolUse: auto-index edited .md
    └── ingest.sh          — Stop: session JSONL ingest
```

Local testing: `claude --plugin-dir /workspaces/the-space-memory`

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

## Definition of Done

A change is merge-ready when **all** of the following hold:

- [ ] `cargo test` passes (all existing + new tests)
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Coverage ≥ 90% (on covered modules)
- [ ] New pub functions have unit tests
- [ ] `bash tests/e2e.sh` passes (if search, index, or IPC changed)
- [ ] CLAUDE.md updated if architecture or commands changed
- [ ] README.md / README.ja.md updated in sync (if user-facing change)

## Gotchas

- **Hook stdin JSON key is `prompt`** (not `user_prompt`).
  Hook output must wrap `additionalContext` in
  `hookSpecificOutput: { hookEventName, additionalContext }`
- ruri safetensors have no tensor name prefix.
  candle's ModernBert::load expects `model.` prefix — key names are remapped at load time
- Use `rusqlite`'s bundled feature (don't depend on system SQLite)
- `tsmd --embedder` spawned by tsmd has idle timeout disabled (`--no-idle-timeout`).
  If run standalone, it auto-stops after 10 min idle (configurable via `TSM_EMBEDDER_IDLE_TIMEOUT`)
- Search errors by default when embedder is down (`search_fallback = "error"`).
  Use `--fallback fts-only` or config for FTS-only mode

## Design Decisions (ADR)

Design decisions and rationale are recorded in the directory below.
Review existing records before making architectural changes.

| Directory | Contents |
|---|---|
| `decisions/` | ADR (decision records and rationale) |

For changes involving process architecture, IPC, or failure behavior,
see ADR-0001.

## Language Policy

- Chat with the user in Japanese
- Documentation and code comments in English
- README.md (English) and README.ja.md (Japanese) must be kept in sync

## License Compatibility

Verify license compatibility when adding dependencies. This project is **MIT** licensed.

All dependencies in `Cargo.toml` must use exact version pinning.
GitHub Actions must pin actions by full commit SHA (not tags).

| Project License | Allowed Dependencies | Not Allowed |
|---|---|---|
| MIT | MIT, BSD, ISC, Apache-2.0, Unlicense | GPL, LGPL, AGPL, MPL (conditional) |

- Ask the user when compatibility is uncertain
- devDependencies (test/build tools) are exempt from license restrictions
