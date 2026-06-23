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
ryl <file>.yml
taplo check <file>.toml

# Code metrics
lizard src/ --language rust -Tcyclomatic_complexity=15 -w  # CCN warnings
npx jscpd                                                  # Duplicate detection

# Git hooks (prek). Run once after cloning. Hooks live in the shared .git, so
# they apply across all worktrees; a branch without .pre-commit-config.yaml is
# blocked until it picks up the config. See .pre-commit-config.yaml for the
# stage split (fast file-scoped checks pre-commit; CI-mirroring gates pre-push).
prek install --hook-type pre-commit --hook-type pre-push

# E2E tests (requires release build + model download)
bash tests/e2e.sh

# Benchmarks (requires live tsmd + indexed corpus; see README "Benchmarks")
cargo bench --bench search_latency
cargo test --features bench-counters   # counter-instrumented tests (off by default)
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
├── lua_hooks.rs         — Embedded Lua runtime for extract/score hooks (ADR-0013)
├── hooks/extract/       — Embedded default extract hook (10-md_frontmatter.lua)
├── hooks/score/         — Embedded default score hook (10-default.lua)
├── chunker.rs           — Markdown → H2/H3/paragraph chunking
├── session_chunker.rs   — Claude session JSONL → Q&A chunking
├── frontmatter.rs       — YAML frontmatter parser
├── tokenizer.rs         — Morphological analysis via lindera (with user dictionary)
├── entity.rs            — Entity graph (link inference)
├── classifier.rs        — Query classification (entity extraction)
├── doc_links.rs         — Inter-document link analysis
├── synonyms.rs          — Synonym expansion, WordNet import, user CSV sync
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
- **Extract hooks** (index time): Lua scripts in `.tsm/hooks/extract/` produce a scalar metadata map
  stored in `documents.metadata` (JSON). `ctx.frontmatter` exposes top-level YAML keys
  (scalars + sequences; nested mappings not passed). Embedded default reproduces `frontmatter.rs`
  (status + effective\_date).
- **Score hooks** (search time): Lua scripts in `.tsm/hooks/score/` each return a multiplier;
  `final = rrf × weight × Π(score hooks)`. Embedded default reproduces time\_decay × status\_penalty.
- **DB schema changes require `rebuild --apply`** (e.g. FTS tokenizer changes) — the `metadata`
  column is added automatically on connect (idempotent migration) and does not require a rebuild
- **Live re-indexing**: `tsm reindex {all|fts|vectors}` — daemon runs batched
  re-index in background, yielding to search between batches

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
- **E2E testdata の日付は placeholder 化必須** — `tests/e2e/testdata/**` 内で
  日付を直書きしない。`__TODAY__` / `__1Y_AGO__` / `__3M_AGO__` 等を使い、
  `tests/e2e.sh` の sed で実行時に置換する。直書きは time-decay スコアで
  flake する（session half-life は 30 日）。CI の `testdata-lint` job が機械的に検出する

## Branch Naming

Branch names must follow `<type>/<description>` format.
The PR labeler workflow (`.github/labeler.yml`) maps prefixes to labels:

| Prefix | Label |
|---|---|
| `feat/` | enhancement |
| `fix/` | bug |
| `docs/` | documentation |
| `perf/` | performance |

Mark **breaking changes** with a Conventional Commits `!` in the PR title
(e.g. `feat!:`, `fix(scope)!:`) or a `BREAKING CHANGE` footer in the body. The
labeler then applies the `breaking` label, which surfaces the PR under
"💥 Breaking Changes" in the generated release notes (`.github/release.yml`).

## Claude Code Plugin

The Claude Code plugin definition (skills/agents/hooks) for `tsm` lives in a
separate repository:
[`key/claude-code-plugins`](https://github.com/key/claude-code-plugins),
under `plugins/the-space-memory/`.

This repo only ships the `tsm` / `tsmd` binaries. Install the plugin from the
plugins repo and ensure `tsm` is on `PATH`.

## Build & Deploy

End users install via mise from GitHub Releases:

```toml
# In consumer .mise.toml
"github:key/the-space-memory" = { version = "latest", extract_all = true }
```

mise verifies SLSA provenance + GitHub artifact attestations during install.

For local development (testing unreleased changes):

```bash
cargo build --release   # Binaries land in ./target/release/{tsm,tsmd}
```

Releases are published by CI on tagged commits as
`tsm-v<version>-<os>-<arch>.tar.gz` (e.g. `tsm-v0.5.1-linux-x86_64.tar.gz`)
containing `bin/{tsm,tsmd}` plus `LICENSE`, `README.md`, and `tsm.toml.example`.

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
- [ ] `npx jscpd` duplication ≤ 5%
- [ ] `lizard src/ --language rust -Tcyclomatic_complexity=15 -w` no new warnings
- [ ] `bash tests/e2e.sh` passes (if search, index, or IPC changed)
- [ ] CLAUDE.md updated if architecture or commands changed
- [ ] README.md / README.ja.md updated in sync (if user-facing change)

## Gotchas

- **Lua hooks dir layout** — `.tsm/hooks/{extract,score}/NN-name.lua` (sorted by file name, `.lua` only).
  Empty or absent dir → embedded defaults. Disable a hook by renaming away the `.lua` extension.
- **Editing a hook requires `tsm restart`** — daemon validates and loads all hooks at startup
  (fail-fast on broken script). CLI uses a lazy per-process cache. Neither reloads hooks at runtime.
- **`metadata` column is added automatically** — `documents.metadata` (JSON) is added by an
  idempotent `ALTER TABLE` migration on connect. Existing rows have NULL metadata; the searcher
  synthesizes scoring from `status`/`updated` columns, so scoring is unaffected. To populate
  metadata for existing documents after writing custom extract hooks, run `tsm reindex` — a full
  destructive rebuild is not required.
- **Lua VMs are sandboxed** — `StdLib::NONE` + 64 MiB memory ceiling per VM.
  No `io`/`os`/`package` available; hooks cannot touch the filesystem, network, or spawn processes.
  Infinite loops are NOT bounded (op-count/timeout limit is deferred); hooks are user-owned scripts,
  not untrusted third-party code.
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
- **User dictionary POS is `名詞`** — simpledic format: `surface,名詞,reading`.
  Uses standard POS so existing noun filters work without special handling.
  `#` comment lines are stripped before passing to lindera
- **`rebuild --apply` resets reject list** — DB is deleted and recreated,
  so `dictionary_candidates` table (including rejected status) is lost.
  Run `tsm dict reject --apply` after rebuild to re-sync from `reject_words.txt`
- **`dict update --apply` uses daemon when available** — if daemon running,
  sends `reindex fts` via IPC (daemon resets its own segmenter).
  If daemon stopped, resets segmenter locally and rebuilds FTS directly
- **Segmenter is cached** — `tokenizer::get_segmenter()` caches the Segmenter
  (including user dict). Call `reset_segmenter()` after writing new simpledic
  if rebuilding FTS in the same process
- **Daemon fail-fasts on an uninitialized DB — without creating `.tsm`** —
  both `cmd_start` and `tsmd` call `db::probe_initialized` BEFORE the stderr log,
  logger, startup lock, or socket touch the state directory. The probe returns
  `Ok(false)` for an absent DB without opening anything (so nothing is
  materialized), opens an existing DB read-write but with no `CREATE` flag (never
  creates it, yet still recovers a hot WAL), and propagates genuine open failures
  (permissions, corruption) as `Err` rather than misreporting "not initialized".
  Starting in an unconfigured directory exits with "Run `tsm init` first" and
  leaves no stray `.tsm` behind (ADR-0008: init is explicit, never
  auto-created). The daemon re-checks `is_initialized` once more after taking the
  startup lock to close the probe→open race. `cmd_start` also surfaces the
  spawned daemon's captured stderr via `try_wait`, so a daemon that dies after
  spawn fails immediately instead of blocking on the 30s socket-wait timeout
- **`tsm doctor` never auto-starts the daemon** — it is a read-only diagnostic.
  Uses the daemon's report if already running, else falls back to a local
  `doctor_check` (which reports an uninitialized/missing DB gracefully and does
  not create a `tsm.db` file)
- **Log files: two, not per-process** — the daemon (`tsmd`) writes a structured,
  daily-rotated `tsmd.log` (kept 3 generations; holds all daemon info/warn).
  Children (embedder, watcher) keep NO own files; they log to stderr, which
  `cmd_start` captures into a single, unrotated `logs/tsmd-stderr.log`. That
  capture replaces terminal inheritance (no shell spam) and `cmd_start` reads it
  to surface startup failures. Shell spam is prevented structurally (stderr
  capture + the daemon dropping `duplicate_to_stderr`), NOT by lowering log
  levels — so child lifecycle/diagnostic logs stay visible in the file.
  `tsmd-stderr.log` is truncated on each start (bounded across runs) but is NOT
  rotated, so a single long-lived session can grow it (size cap is a follow-up).
  A foreground `tsmd` still logs to the terminal
- **All log modes default to `info`** (`logging::default_log_spec`). User-facing
  command output is `println!`, not the logger, so it shows regardless of level.
  Set `RUST_LOG=warn` to quiet the CLI terminal, or `debug` for more detail
- **macOS tempdir vs `current_dir()` symlinks** — `tempfile::tempdir()` returns
  `/var/...` but after `set_current_dir`, `current_dir()` resolves the symlink to
  `/private/var/...`. A test asserting `path == cwd` must canonicalize the tempdir
  path up front and use it for both `set_current_dir` and the assertion (see
  `config::tests::test_load_config_relative_path_resolves_against_cwd`); otherwise
  it fails on macOS only while passing on Linux CI

## Design Decisions (ADR)

Design decisions and rationale are recorded in the directory below.
Review existing records before making architectural changes.
The ADR format authority is `decisions/README.md` (target-state only; no
`Follow-ups`/task-list sections, no review-process attribution). Get the next
ADR number from `main`, not your branch (renumbering may be in flight).

| Directory | Contents |
|---|---|
| `decisions/` | ADR (decision records and rationale) |

For changes involving process architecture, IPC, or failure behavior,
see ADR-0001. For uninitialized-DB fail-fast, daemon auto-start boundaries,
and read-only `doctor`, see ADR-0011. For the output-channel model
(user output → stdout, logs/errors → stderr/file, log-file consolidation),
see ADR-0012.

## Language Policy

- Chat with the user in Japanese
- Documentation and code comments in English
- README.md (English) and README.ja.md (Japanese) must be kept in sync

## Documentation Style

`docs/` guides should follow this section order:

1. Design concept (Why) — background and design decisions
2. File layout (What) — file structure and formats
3. Operations guide (How to use) — setup, maintenance, troubleshooting
4. Internals (How it works) — collection logic, data flow
5. Implementation reference (Code) — source files and roles

## License Compatibility

Verify license compatibility when adding dependencies. This project is **MIT** licensed.

All dependencies in `Cargo.toml` must use exact version pinning.
GitHub Actions must pin actions by full commit SHA (not tags).

| Project License | Allowed Dependencies | Not Allowed |
|---|---|---|
| MIT | MIT, BSD, ISC, Apache-2.0, Unlicense | GPL, LGPL, AGPL, MPL (conditional) |

- Ask the user when compatibility is uncertain
- devDependencies (test/build tools) are exempt from license restrictions
