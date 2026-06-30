# #303 Coverage-Gate Continuation — Implementation Plan (v3: rename-to-`_proc`, no merges)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Each task is an independent PR off the LATEST origin/main, in its own worktree. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Drive the coverage exclusion set to a self-documenting shape — **`(main|cli|_proc|model_loader)\.rs`** — so a reader knows at a glance why each excluded file is excluded: entry points (`main`), CLI dispatch (`cli`), daemon-side process/worker shells (`*_proc`), and the real model loader (`model_loader`). Achieve it by RENAMING the daemon-side I/O shells to a `*_proc.rs` convention (NOT merging them — the line gate forbids a consolidated file), extracting their pure logic into counted `*_logic.rs`, renaming the model loader to a self-evident lib-side name, and covering `logging.rs` so it leaves the regex.

**Why rename, not merge (decided with the user 2026-06-30, after Codex v2):** the user asked whether `backfill` could fold into `daemon_mode` (it runs as daemon threads). It can architecturally, BUT measured line counts (daemon_mode 466 + backfill 273 + child 130 = 869 code lines, only ~70 extractable as pure) would push a merged `daemon_proc.rs` over the ADR-0018 800-line gate, and the backfill loop bodies are I/O (not extractable). So the same cognitive-load win (self-evident names) is taken by RENAMING each shell to `*_proc` while keeping files separate — no oversized file, no gate pressure.

**Why this shape:** measured data shows only `cli.rs` carries substantial coverable logic; everything else excluded is irreducible I/O (accept/event loops, worker loops, process spawn, model load, logger init). The win is not "shorten the regex by gating un-testable loops" (fragile) but "make every excluded file self-evidently a shell."

**Architecture:** Humble-object pattern (#308 `watch_logic`, #311 `backfill_logic`, #312 embedder mock): pure logic → counted `*_logic.rs`; irreducible I/O keeps the `*_proc`/base name and stays excluded. Renames are mechanical; behavior is preserved.

**Tech Stack:** Rust, `cargo llvm-cov`, in-memory SQLite, `tempfile`.

## Global Constraints

- **Gate is GREEN (95.42%, +5.42pt @ `3a1e626`).** Renames are coverage-NEUTRAL (excluded→excluded); `*_logic` extractions and the logging coverage RAISE the aggregate. Re-measure after every PR and paste the global % into the PR body.
- Gate command (regex evolves — see table): `cargo llvm-cov --ignore-filename-regex '<regex>' --fail-under-lines 90` (`.github/workflows/ci.yml:51-54`). Single GLOBAL aggregate.
- **`_proc\.rs` is collision-safe (verified):** matches `daemon_proc/embedder_proc/watcher_proc/backfill_proc/child_proc.rs`; does NOT match `daemon_protocol.rs` (no `_proc.rs` substring) nor any `*_logic.rs`. Keep `*_logic.rs` free of `_proc`.
- **Per-file line gate (ADR-0018, `tests/file_line_limits.rs`):** flat cap 800 *code* lines (test module excluded); >800 needs a frozen baseline. With NO merges, every renamed file keeps its current size (all currently < 800: daemon 466, backfill 273, child 130) → no oversized file. P1 must still run a preflight `cargo test --test file_line_limits` and update baseline KEYS for renamed files.
- TDD; behavior-preserving renames get characterization tests for any newly-counted extracted logic. **Move tests WITH the code:** pure helpers leaving an excluded shell for a counted `*_logic.rs` bring their tests (now counted); I/O staying in `*_proc` stays untested.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `npx jscpd` ≤ 5%, `lizard -T15` no new warnings — all clean.
- Coverage % is test-inclusive (#317): confirm PRODUCTION branches of moved logic are exercised.
- Pure fns take values/bools; FS/socket/clock checks stay in the shell.
- A rename/regex PR updates IN THE SAME PR: `.github/workflows/ci.yml` regex, `CLAUDE.md` (arch tree + exclusion rationale + the regex echoed at CLAUDE.md:22), `tests/file-line-baseline.txt` keys, and `mod` declarations (`src/bin/tsmd/main.rs`, `src/lib.rs`).
- `docs/command-reference.md` only if the clap surface changes (`tests/cli_docs.rs`). P5 (cli.rs) is internal → `cargo test --test cli_docs` must stay green.
- English docs/comments; no inline ADR/issue refs (#304/#305). Worktree off LATEST origin/main; PRs target main; never direct-push. Each PR: `review-pr` + `/codex:adversarial-review` before merge.

## End-state target & regex evolution

End state: **`(main|cli|_proc|model_loader)\.rs`** — entry points (`src/main.rs`, `bin/tsmd/main.rs`), CLI dispatch (`cli.rs`), process/worker shells (`daemon_proc`, `embedder_proc`, `watcher_proc`, `backfill_proc`, `child_proc`), model loader (`model_loader.rs`).

| After | Regex | Δ |
|---|---|---|
| start | `(ruri_model\|main\|cli\|logging\|daemon_mode\|embedder_mode\|watcher_mode\|child\|backfill)\.rs` | — |
| P1 | `(ruri_model\|main\|cli\|logging\|_proc\|embedder_mode\|watcher_mode)\.rs` | rename daemon_mode/child/backfill → `*_proc`; introduce `_proc` term |
| P2 | `(model_loader\|main\|cli\|logging\|_proc\|watcher_mode)\.rs` | embedder_mode→embedder_proc (covered by `_proc`); ruri_model→model_loader |
| P3 | `(model_loader\|main\|cli\|logging\|_proc)\.rs` | watcher_mode→watcher_proc (covered by `_proc`) |
| P4 | `(main\|cli\|_proc\|model_loader)\.rs` | −logging (covered + included) → FINAL |
| P5,P6 | unchanged (cli.rs & main.rs stay excluded; logic moves to counted modules) | — |

**Every regex edit is SERIALIZED and applied in lock-step with the matching rename — never jump to the final regex early** (Codex v2 finding: a premature final regex would expose still-named `ruri_model`/`*_mode` files). Re-measure with the EXACT post-step regex before merge. Each step is coverage-neutral (rename) or positive (extraction/logging); none dips below 90% (margin >5pt).

## Coordination protocol (dispatcher reads first)

- **Phase 1 = P1 → P2 → P3, STRICTLY SERIALIZED.** Each renames files and edits the SAME shared files (ci.yml regex, CLAUDE.md, baseline, `mod` decls). One at a time; rebase on main after the prior merges. No parallel renames.
- **P4 (logging) STRICTLY AFTER P3** — it produces the FINAL regex, only valid once `ruri_model`/`embedder_mode`/`watcher_mode` are renamed. (Codex v2 finding: do not run P4 after P1.)
- **G (PendingWriteGuard) after P1** (P1 may touch the daemon area; G touches `backfill_logic.rs`). Small, standalone.
- **Phase 3:** P6 (render) parallel-safe and independent of the renames. **P5/F1–F4 are GATED** on the `feat/cli-command-reorg` ordering (Open Q1); then serialize via integration branch OR non-overlapping cli.rs call-site ranges + mandatory rebase/test after each merge (the F tasks all rewire `src/cli.rs`).
- Every PR re-runs the full gate with its post-step regex, pastes the global %, and (per #317) confirms production branches are covered, not just inline test lines.

---

## Task P1: rename daemon-side shells to `*_proc`; extract pure → daemon_logic [SERIALIZED #1]

**Renames (mechanical, behavior-preserving):**
- `src/bin/tsmd/daemon_mode.rs` → `daemon_proc.rs` (accept loop + client dispatch).
- `src/bin/tsmd/backfill.rs` → `backfill_proc.rs` (worker-loop bodies: `run_backfill_pass`, `periodic_backfill`, `sleep_interruptible`, `run_reindex_fts_pass`, `run_reindex_vectors_pass`). Pure parts already in `backfill_logic.rs` (#311) — leave them.
- `src/bin/tsmd/child.rs` → `child_proc.rs` (process spawn/signal I/O: `start_child_self`, `spawn_child`, `reap_child`, `stop_child`, `remove_stale_socket`).

**Extract pure → `src/bin/tsmd/daemon_logic.rs` (NEW, counted):**
- `embedder_child_args(model_dir: Option<&Path>) -> Vec<String>`, `reload_response(warnings, watcher_pid_present, sighup_ok) -> DaemonResponse`, `reindex_steps(kind) -> &'static [ReindexStep]`.
- Named `ReindexGuard(Arc<AtomicBool>)` moved out of the inline daemon code, `#[derive(Debug)]` + drop test (#268 item 5). Reject a single shared guard type (counter `AtomicUsize` vs reset-flag `AtomicBool` differ) — note in PR.
- The PURE child helpers `read_pid_from_file`, `is_process_alive` (currently tested-but-uncounted inside the excluded `child.rs`) move here WITH their tests → now counted. `child_proc.rs` keeps only the spawn/signal I/O.

**Shared-file updates (same PR):** `mod` decls in `src/bin/tsmd/main.rs` (`daemon_proc`/`backfill_proc`/`child_proc`/`daemon_logic`); `ci.yml` regex (P1 row); `CLAUDE.md` (arch tree + regex + Gotchas); `tests/file-line-baseline.txt` keys.

**Preflight (Codex v2 finding 3):** run `cargo test --test file_line_limits` BEFORE finishing — confirm `daemon_proc.rs` (≈466 − extracted) / `backfill_proc.rs` (≈273) / `child_proc.rs` (≈100 after pure helpers leave) are each < 800 code lines. They are today; the renames don't grow them. If any exceeds, extract more pure logic into `daemon_logic.rs`.

**If too large to review:** split — P1a (child→child_proc + child pure→daemon_logic), P1b (backfill→backfill_proc), P1c (daemon_mode→daemon_proc + daemon_logic extraction). Keep the regex serialized across the sub-PRs.

**DoD:** three files renamed to `*_proc`; daemon_logic.rs counted + tested (daemon pure + child pure helpers + ReindexGuard); regex = P1 row; global ≥ 90% (re-measured); `file_line_limits` green; CLAUDE.md/baseline/mod-decls updated.

## Task P2: embedder_mode→embedder_proc; ruri_model→model_loader (lib-side); extract embedder_logic [SERIALIZED #2, after P1]

**Renames:**
- `src/bin/tsmd/embedder_mode.rs` → `embedder_proc.rs` (socket server loop).
- `src/ruri_model.rs` → `src/model_loader.rs` — **stays in the LIBRARY crate** (Codex v2 finding 1: `RuriModel` defines inherent methods on the lib type `Embedder` and calls the `pub(crate)` `Embedder::from_parts`; a `bin/` module is a separate crate and cannot do either, so the loader CANNOT move to the binary). This is a pure file rename + `mod` rename in `lib.rs` (`pub mod model_loader;`); `embedder.rs` and the doc-comment references update to the new name. `embedder.rs` logic is otherwise unchanged (mock-testability preserved).

**Extract pure → `src/bin/tsmd/embedder_logic.rs` (NEW, counted):** `parse_texts(&Value) -> Vec<String>`, `panic_message(&dyn Any) -> String`, `encode_response(Result<Vec<Vec<f32>>>) -> Value`, `resolve_model_load(model_dir, all_files_present) -> ModelLoadPlan` (FS check stays in the shell). Tests: empty/missing/non-array/panic.

**Shared-file updates:** `mod` decls; `ci.yml` regex (P2 row — `embedder_mode`→covered by `_proc`; `ruri_model`→`model_loader`); CLAUDE.md (arch tree calls it the "model loader (model-coupled shell; gate-excluded)"); baseline keys.

**DoD:** embedder_proc.rs is the socket shell; embedder_logic.rs counted + tested; `model_loader.rs` is the renamed lib-side loader (still excluded, now self-evident); `ruri_model`/`embedder_mode` terms gone; regex = P2 row; global ≥ 90%.

## Task P3: watcher_mode→watcher_proc [SERIALIZED #3, after P2]

Rename `src/bin/tsmd/watcher_mode.rs` → `watcher_proc.rs`. `watch_logic.rs` (counted, #308/#309) stays. If any reachable pure logic remains beyond #308/#309, move it to `watch_logic.rs` with tests; else this is rename + regex/doc/baseline/mod-decl update.

**DoD:** watcher_proc.rs is the fs-event/signal shell; regex = P3 row (`(model_loader|main|cli|logging|_proc)\.rs`); global ≥ 90%; docs/baseline updated.

## Task P4: cover logging.rs → drop from regex [STRICTLY AFTER P3]

**Files:** `src/logging.rs`; `ci.yml` (P4 row = FINAL regex); CLAUDE.md.

**Approach (evidence-first):** enumerate the 23 missed lines. Cover `tsm_log_format` via a `Vec<u8>` writer + a `log::Record` built with `RecordBuilder` (assert the `[ts] [LEVEL] [module] msg` shape). Irreducible residual: the `LogMode::Daemon` file-logger arm (OnceLock runs the mode closure once/process) + flexi_logger global-offset edge (~15 lines) — acceptable INCLUDED (dilution ≈ 15/~16000; margin >5pt). Drop `logging`; the regex reaches the FINAL `(main|cli|_proc|model_loader)\.rs`.

**DoD:** `tsm_log_format` (and any reachable region) covered; `logging` removed; regex FINAL; global ≥ 90% re-measured; CLAUDE.md documents the tiny OnceLock residual now counted.

## Task G: PendingWriteGuard Debug + doc (#268 item 3) [standalone, after P1]

`src/bin/tsmd/backfill_logic.rs` only: add `#[derive(Debug)]` to `PendingWriteGuard` + a doc note that `mem::forget` leaves the counter elevated. Trivial; counted already; no behavior change. After P1 to avoid daemon-area churn.

**DoD:** Debug + doc; tests/clippy/fmt green; no behavior change.

## Task P5 (F1–F4): cli.rs logic → counted siblings [GATED on reorg ordering]

cli.rs (3617 lines, 848 missed) stays excluded as the `cli` dispatch shell; logic moves to counted siblings. `tests/cli_docs.rs` stays green (no clap change) — `cargo test --test cli_docs`. **Do NOT launch until `feat/cli-command-reorg` ordering is resolved (Open Q1); then serialize or range-assign.** One PR per module:
- **F1 `cli_format.rs` (~200, highest value):** `render_doctor_report` box (`cli.rs:1485`), `print_status_info` (`cli.rs:1677`), `format_since` (`:1771`), `estimate_eta` (`:1777`, param `now`), `print_candidate` (`:2078`). Build `String`/write to a writer. Coordinate with P6 (shared `render_doctor_report`).
- **F2 `cli_dict_logic.rs` (~60–80):** `classify_reindex`+`ReindexPlan` (`:1839`), dict verdict→message (`:1949-1999`), `report_reconcile` (`:1908`).
- **F3 `cli_rebuild_logic.rs` (~40–60):** `decide_backfill_action(...)` (from `report_vectors_and_backfill` `:2255`), `rebuild_dry_run` text (`:2119`).
- **F4 `cli_args_logic.rs` (~80–100):** `normalize_path_filters` (`:385`), `should_place_cache_resource` (`:211`), `same_entry_location` (`:236`), `run_search` fallback/temporal (`:416`).

**Per-F DoD:** new counted module + characterization tests; cli.rs call sites rewired; cli_docs green; global ≥ 90%; cli.rs residual is I/O-only.

## Task P6: main.rs render_* → counted render module [parallel-safe]

`src/main.rs` (954 lines, 18.88%); create counted `src/render.rs` (registered in `lib.rs`). Move `render_search/index/ingest/status/doctor/reload/reindex/import_wordnet` (`main.rs:546-657`), refactor to write into `&mut impl Write` / return `String`; `fn main` keeps thin call sites. **`main` stays excluded** (matches `src/main.rs` AND the true entry `bin/tsmd/main.rs`; not removable without path-specific regex, out of scope). Coordinate with F1 on the shared doctor renderer (F1 first if both active).

**DoD:** `render_*` in counted `render.rs` + tested; `src/main.rs` a thin entry shell; global ≥ 90%; `cargo test --test cli_docs` green.

---

## Self-review

- #303 steps 3,6,8,9,10 → P4(logging), P1(child renamed+pure extracted), P1+P2(daemon/embedder), P5(cli), P6(main). Steps 1,2,4,5,7 shipped. ✓
- #268 item 5 (ReindexGuard) → P1; item 3 (PendingWriteGuard) → G. ✓
- Every end-state excluded file is `main`/`cli`/`*_proc`/`model_loader` — self-documenting. ✓
- `_proc` collision-safe vs `daemon_protocol.rs`; won't catch `*_logic.rs`. ✓
- Child/backfill PURE helpers → COUNTED `*_logic.rs` (not buried in `*_proc`). ✓

### Codex adversarial-review — incorporated
- v1: F1–F4 not independent → P5 gated/serialized; `main` non-removable → P6 keeps it; A/B keep-excluded → evidence-first (now P1 child pure + P4 logging); #268 item3 ridealong → Task G. ✓
- v2 [high] RuriModel can't move to a binary crate (inherent methods on lib `Embedder` + `pub(crate)` `from_parts`) → P2 keeps the loader LIB-side, renamed `model_loader.rs`; `embedder.rs` truly unchanged. ✓
- v2 [medium] P4 ordering contradicted the regex table → P4 STRICTLY after P3; all regex edits serialized; no early jump to final. ✓
- v2 [medium] daemon_proc line-gate undercount (869 actual) → NO merge; rename-only keeps every file < 800; P1 runs a `file_line_limits` preflight. ✓

## Dispatch summary

- **Serialized Phase 1:** P1 → P2 → P3 (renames to `*_proc` + `model_loader`, logic extraction), one at a time, rebasing after each merge.
- **After P1:** G (guard). **After P3:** P4 (logging cover→include, FINAL regex).
- **Phase 3:** P6 (render) parallel-safe; **P5/F1–F4 gated on the reorg decision**, then serialized/range-assigned.
- Every PR: `review-pr` + `/codex:adversarial-review`; paste measured global % (post-step regex).

## Open questions for the user

1. **CLI reorg ordering** (gates P5/F1–F4): land `feat/cli-command-reorg` first then rebase F onto it, or freeze the reorg and run F first?
2. **`_proc` for backfill/child** (P1): `backfill_proc`/`child_proc` read as "daemon worker/spawn shells." OK, or prefer a different suffix (e.g. keep `backfill`/`child` and just document)? Merging into daemon_proc is OFF the table (line gate).
3. **Dispatch start:** begin P1 now (serialized chain) while the reorg question settles, or hold for full sign-off?
