# #303 Coverage-Gate Continuation — Implementation Plan (v2: self-documenting exclusion)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Each task is an independent PR off the LATEST origin/main, in its own worktree. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Drive the coverage exclusion set down to a single self-documenting shape — **`(main|cli|_proc)\.rs`** — by (a) renaming the daemon-side process shells to a `*_proc.rs` convention, (b) consolidating in-process daemon I/O (backfill thread bodies, child-spawn helpers, the real model loader) into the relevant `*_proc.rs` while keeping their pure logic in counted `*_logic.rs` siblings, (c) covering `logging.rs` so it leaves the regex, and (d) extracting `cli.rs` / `main.rs` logic into counted modules. End state: the only excluded files are **entry points (`main`)**, **the CLI dispatch (`cli`)**, and **process shells (`*_proc`)** — each obvious from its name.

**Why this shape (decided with the user 2026-06-30):** the measured data shows only `cli.rs` carries substantial coverable logic; everything else excluded is genuinely irreducible I/O (accept/event loops, process spawn, model-load, logger-init). So the lever is not "shorten the regex by including loops" (that gates un-testable code and goes fragile) but "make every excluded file self-evidently a shell." `(main|cli|_proc)` achieves that: three buckets a reader understands at a glance.

**Architecture:** Humble-object pattern (established by #308 `watch_logic`, #311 `backfill_logic`, #312 embedder mock): pure decision/format/parse logic lives in counted `*_logic.rs`; the irreducible I/O shell keeps the base/`*_proc` name and stays excluded. Renames + consolidation are mechanical moves; behavior is preserved.

**Tech Stack:** Rust, `cargo llvm-cov`, in-memory SQLite for DB tests, `tempfile` for fs tests.

## Global Constraints

- **Gate is GREEN with headroom — this is logic-protection + naming hygiene, not gate-rescue.** Measured @ origin/main `3a1e626` (2026-06-30): current regex sees **95.42%** (margin +5.42pt); whole-workspace no-regex is 84.97%. Every renaming/consolidation move is coverage-NEUTRAL (excluded→excluded); every `*_logic` extraction + the logging coverage RAISES the aggregate. Re-measure after each PR and paste the global % into the PR body.
- Gate command (regex evolves per PR — see the evolution table): `cargo llvm-cov --ignore-filename-regex '<regex>' --fail-under-lines 90` (`.github/workflows/ci.yml:51-54`). Single GLOBAL aggregate; `--fail-under-file-lines` is NOT used.
- **`_proc\.rs` pattern is collision-safe (verified):** it matches `daemon_proc.rs`/`embedder_proc.rs`/`watcher_proc.rs` but NOT `daemon_protocol.rs` (no `_proc.rs` substring) nor any `*_logic.rs`. Keep `*_logic.rs` names free of `_proc`.
- **Per-file line gate (ADR-0018, `tests/file_line_limits.rs`):** flat cap 800 *code* lines (test module excluded); larger files need a frozen baseline entry in `tests/file-line-baseline.txt`. Consolidated `daemon_proc.rs` ≈ 690 code lines (daemon_mode ~330 + backfill ~250 + child ~110) — under 800, no baseline entry needed, but confirm with the gate. Renamed files: update their baseline keys.
- TDD required; behavior-preserving moves get characterization tests that pin the moved behavior. New pub(crate) fns need in-file `#[cfg(test)] mod tests`.
- **Move tests WITH the code.** Coverage attaches to executed lines: when pure helpers move from an excluded file (e.g. `child.rs`) into a counted `*_logic.rs`, their tests come along and now count; when I/O moves into an excluded `*_proc.rs`, leave it untested.
- `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean; `npx jscpd` ≤ 5%; `lizard src/ --language rust -Tcyclomatic_complexity=15 -w` no new warnings.
- **Coverage % is test-inclusive** (#317): confirm PRODUCTION branches of moved logic are exercised, not just that the headline rose via inline test bodies.
- Pure fns take values/bools; do FS/socket/clock checks in the shell and pass the result in.
- A rename/regex PR MUST update, in the same PR: `.github/workflows/ci.yml` regex, `CLAUDE.md` (architecture tree + exclusion-rationale + the regex echoed at CLAUDE.md:22), `tests/file-line-baseline.txt` keys, and the `mod` declarations (`src/bin/tsmd/main.rs`, `src/lib.rs`).
- `docs/command-reference.md` only if the clap surface changes (`tests/cli_docs.rs`). P5 (cli.rs) is an internal refactor → run `cargo test --test cli_docs` to confirm it stays green.
- Docs/comments in English; no inline ADR/issue refs in source (#304/#305). the-space-memory work in a worktree off LATEST origin/main; PRs target main; never direct-push. Each PR: `review-pr` + `/codex:adversarial-review` before merge.

## End-state target & regex evolution

End state: **`(main|cli|_proc)\.rs`** — buckets: entry points (`src/main.rs`, `src/bin/tsmd/main.rs`), CLI dispatch (`cli.rs`), process shells (`daemon_proc`, `embedder_proc`, `watcher_proc`).

| After | Regex | Δ |
|---|---|---|
| start | `(ruri_model\|main\|cli\|logging\|daemon_mode\|embedder_mode\|watcher_mode\|child\|backfill)\.rs` | — |
| P1 | `(ruri_model\|main\|cli\|logging\|daemon_proc\|embedder_mode\|watcher_mode)\.rs` | −daemon_mode,−child,−backfill, +daemon_proc |
| P2 | `(main\|cli\|logging\|daemon_proc\|embedder_proc\|watcher_mode)\.rs` | −embedder_mode,−ruri_model, +embedder_proc |
| P3 | `(main\|cli\|logging\|daemon_proc\|embedder_proc\|watcher_proc)\.rs` | −watcher_mode, +watcher_proc |
| P4 | `(main\|cli\|daemon_proc\|embedder_proc\|watcher_proc)\.rs` = `(main\|cli\|_proc)\.rs` | −logging (now covered+included) |
| P5,P6 | unchanged (cli.rs & main.rs stay excluded as dispatch/entry shells; their logic moves to counted modules) | — |

Each step is coverage-neutral (renames/consolidation) or coverage-positive (logic extraction, logging). Re-measure with the EXACT post-step regex before merge.

## Coordination protocol (dispatcher reads first)

- **Phase 1 (P1→P2→P3) is SERIALIZED.** Each renames files and edits the SAME shared files (ci.yml regex, CLAUDE.md, baseline, `mod` decls). Run them one at a time, each rebasing on main after the prior merges. Do NOT parallelize renames.
- **Phase 2 (P4 logging, G guard) after P1** (G touches `backfill_logic.rs`, which P1 also touches; P4 is independent but small).
- **Phase 3 (P5 cli, P6 render) is the parallel-friendly logic work,** but P5's F1–F4 are gated (below). P6 is independent.
- **P5/F1–F4 dispatch gate (codex-required): NOT in any parallel wave until the `feat/cli-command-reorg` ordering is resolved with the user.** Then serialize via an integration branch OR assign non-overlapping cli.rs call-site ranges + mandatory rebase/test after each merge. The F tasks all rewire the same `src/cli.rs` and the reorg may rename the same `cmd_*` sites.
- Every PR re-runs the full gate with its post-step regex and pastes the global % (per #317, also confirm production branches are covered, not just inline test lines).

---

## Task P1: daemon_mode → daemon_proc (absorb backfill + child; extract pure to daemon_logic) [SERIALIZED]

**Files:**
- Rename/Create: `src/bin/tsmd/daemon_mode.rs` → `src/bin/tsmd/daemon_proc.rs` (the accept loop + client dispatch).
- Absorb into `daemon_proc.rs`: the I/O thread bodies from `src/bin/tsmd/backfill.rs` (`run_backfill_pass`, `periodic_backfill`, `sleep_interruptible`, `run_reindex_fts_pass`, `run_reindex_vectors_pass`) and the process-spawn I/O from `src/bin/tsmd/child.rs` (`start_child_self`, `spawn_child`, `reap_child`, `stop_child`, `remove_stale_socket`).
- Extend counted `src/bin/tsmd/daemon_logic.rs` (NEW, counted) with: `embedder_child_args(model_dir) -> Vec<String>`, `reload_response(warnings, watcher_pid_present, sighup_ok) -> DaemonResponse`, `reindex_steps(kind) -> &'static [ReindexStep]`, the named `ReindexGuard(Arc<AtomicBool>)` (`#[derive(Debug)]` + drop test — #268 item 5), AND the pure child helpers moved out of child.rs: `read_pid_from_file`, `is_process_alive` (with their existing tests — these were tested-but-uncounted in the excluded child.rs; now counted).
- Keep `src/bin/tsmd/backfill_logic.rs` (already counted; #311) as-is here (item 3 is Task G).
- Delete `backfill.rs`, `child.rs`, `daemon_mode.rs` after their content is relocated.
- Update: `src/bin/tsmd/main.rs` mod decls (`mod daemon_proc; mod daemon_logic;` remove `mod backfill; mod child; mod daemon_mode;`), `ci.yml` regex (per evolution table P1), `CLAUDE.md` (arch tree + regex + Gotchas), `tests/file-line-baseline.txt`.

**Notes:** behavior-preserving. The "single shared guard" idea is rejected (counter `AtomicUsize` vs reset-flag `AtomicBool`); note in PR body. Pure surface from daemon_mode itself is small (much already in #301/#311) — honest coverage note. If the PR is too large to review, split as P1a (extract child pure → daemon_logic, move child I/O → daemon_proc, drop `child` term) then P1b (absorb backfill I/O, extract daemon_logic, drop `daemon_mode`/`backfill`, add `daemon_proc`).

**DoD:** daemon_proc.rs holds only daemon I/O (accept loop + worker loops + spawn); daemon_logic.rs counted + tested (incl. moved child pure helpers + ReindexGuard); backfill.rs/child.rs/daemon_mode.rs gone; regex = P1 row; global ≥ 90% (re-measured); line gate green; CLAUDE.md/baseline updated.

## Task P2: embedder_mode → embedder_proc (absorb ruri_model; extract pure to embedder_logic) [SERIALIZED, after P1]

**Files:**
- Rename: `src/bin/tsmd/embedder_mode.rs` → `src/bin/tsmd/embedder_proc.rs` (socket server loop).
- Absorb the real model loader `src/ruri_model.rs` (`RuriModel`, `load`, `load_from_paths`) into `embedder_proc.rs`. `embedder.rs` (the lib trait + mock-testable `Embedder`) is UNCHANGED — only the concrete loader relocates; #312's mock-testability is preserved. Verify `RuriModel` has no lib-level/test consumers outside the embedder process before moving from `lib.rs` into the binary; if it does, instead rename `ruri_model.rs` → keep it lib-side but covered/justified (fallback).
- Create counted `src/bin/tsmd/embedder_logic.rs`: `parse_texts(&Value) -> Vec<String>`, `panic_message(&dyn Any) -> String`, `encode_response(Result<Vec<Vec<f32>>>) -> Value`, `resolve_model_load(model_dir, all_files_present) -> ModelLoadPlan` (FS check stays in shell). Test decode/response shaping (empty/missing/non-array/panic).
- Update: mod decls, `ci.yml` regex (P2 row: drop `embedder_mode`,`ruri_model`; add `embedder_proc`), `lib.rs` (`ruri_model` mod removed if folded into bin), CLAUDE.md, baseline.

**DoD:** embedder_proc.rs holds socket loop + model load; embedder_logic.rs counted + tested; `ruri_model` term gone; embedder.rs unchanged; regex = P2 row; global ≥ 90%; docs/baseline updated.

## Task P3: watcher_mode → watcher_proc (rename) [SERIALIZED, after P2]

**Files:** Rename `src/bin/tsmd/watcher_mode.rs` → `src/bin/tsmd/watcher_proc.rs`. `watch_logic.rs` (counted, #308/#309) stays. If any reachable pure logic remains in the file beyond what #308/#309 extracted, move it to `watch_logic.rs` with tests; otherwise this is a rename + regex/doc/baseline update. Update mod decls, `ci.yml` (P3 row), CLAUDE.md, baseline.

**DoD:** watcher_proc.rs is the fs-event/signal shell; regex = P3 row (= `(main|cli|logging|_proc)\.rs`); global ≥ 90%; docs/baseline updated.

## Task P4: cover logging.rs → drop from regex [after P3]

**Files:** `src/logging.rs`; `ci.yml` (P4 row: drop `logging`); CLAUDE.md.

**Approach (evidence-first):** enumerate the 23 missed lines. Cover the testable ones: `tsm_log_format` via a `Vec<u8>` writer + a `log::Record` built with `RecordBuilder` (assert the `[ts] [LEVEL] [module] msg` shape). The irreducible residual is the `LogMode::Daemon` file-logger arm (OnceLock runs the mode closure once/process) + flexi_logger global-offset edge — that ~15-line residual is acceptable INCLUDED (dilution ≈ 15 / ~16000, margin >5pt). Drop `logging` from the regex; logging.rs is now a counted lib module.

**DoD:** `tsm_log_format` (and any other reachable missed region) covered; `logging` removed from regex; regex now `(main|cli|_proc)\.rs`; global ≥ 90% re-measured; CLAUDE.md notes logging is included with a tiny documented OnceLock residual.

## Task G: PendingWriteGuard Debug + doc (#268 item 3) [standalone, after P1]

**Files:** `src/bin/tsmd/backfill_logic.rs` only. Add `#[derive(Debug)]` to `PendingWriteGuard` + a doc note that `mem::forget` leaves the counter elevated. Trivial; backfill_logic is already counted; no behavior change. Sequence after P1 (which also touches the daemon area) to avoid churn.

**DoD:** PendingWriteGuard has Debug + doc; `cargo test`/clippy/fmt green; no behavior change.

## Task P5 (F1–F4): cli.rs logic → counted siblings [GATED on reorg ordering]

cli.rs (3617 lines, 848 missed) stays excluded as the `cli` dispatch shell; its pure logic moves to counted siblings. `tests/cli_docs.rs` stays green (no clap-surface change) — confirm with `cargo test --test cli_docs`. **Do NOT launch until `feat/cli-command-reorg` ordering is resolved (Open Question 1); then serialize or range-assign (Coordination protocol).** Each F task is one PR with a new module:

- **F1 `cli_format.rs` (~200 lines, highest value):** `render_doctor_report` box drawing (`cli.rs:1485`, ~100), `print_status_info` line assembly (`cli.rs:1677`, ~85), `format_since` (`cli.rs:1771`), `estimate_eta` (`cli.rs:1777`, parameterize `now`), `print_candidate` formatting (`cli.rs:2078`). Refactor to build `String`/write into a writer. Coordinate with P6 (`main.rs render_doctor` delegates to `cli::render_doctor_report`).
- **F2 `cli_dict_logic.rs` (~60–80):** `classify_reindex`+`ReindexPlan` (`cli.rs:1839`), `cmd_dict_add/rm/reject` verdict→message mapping (`cli.rs:1949-1999`), `report_reconcile` text (`cli.rs:1908`).
- **F3 `cli_rebuild_logic.rs` (~40–60):** `decide_backfill_action(vecs, chunks, socket_exists, backfill_in_progress) -> Action` (from `report_vectors_and_backfill`, `cli.rs:2255`), `rebuild_dry_run` caution text (`cli.rs:2119`).
- **F4 `cli_args_logic.rs` (~80–100):** `normalize_path_filters` (`cli.rs:385`, add tests), `should_place_cache_resource` (`cli.rs:211`), `same_entry_location` (`cli.rs:236`), `run_search` fallback/temporal decision (`cli.rs:416`).

**Per-F DoD:** new counted module + characterization tests; cli.rs call sites rewired; cli_docs green; global ≥ 90%; cli.rs residual for that area is I/O-only.

## Task P6: main.rs render_* → counted render module [parallel-safe]

**Files:** `src/main.rs` (954 lines, 18.88%); create counted `src/render.rs` (registered in `lib.rs`). Move `render_search/index/ingest/status/doctor/reload/reindex/import_wordnet` (`main.rs:546-657`), refactor to write into `&mut impl Write` / return `String`; `fn main` keeps thin call sites. **`main` stays in the regex** (it matches both `src/main.rs` and the true entry `bin/tsmd/main.rs`; not removable without path-specific regex, out of scope). `format_daemon_failure`/`wait_for_pid_exit` already tested — leave. Coordinate with F1 on the shared doctor renderer (run F1 first if both active).

**DoD:** `render_*` in counted `render.rs` + tested; `src/main.rs` is a thin entry/dispatch shell; global ≥ 90%; `cargo test --test cli_docs` green.

---

## Self-review

- #303 steps 3,6,8,9,10 → P4(logging), P1(child folded), P1+P2(daemon/embedder), P5(cli), P6(main). Steps 1,2,4,5,7 shipped (#307/#310/#308-9/#311/#312). ✓
- #268 item 5 (ReindexGuard) → P1; item 3 (PendingWriteGuard) → G. ✓
- Naming: every end-state excluded file is `main`/`cli`/`*_proc` — self-documenting per user's cognitive-load goal. ✓
- `_proc` collision with `daemon_protocol.rs` checked safe; `_proc` won't catch `*_logic.rs`. ✓
- Child/backfill pure helpers go to COUNTED `*_logic.rs` (not buried in the excluded `*_proc`), preserving #303's "move logic into gated files" intent. ✓
- Line gate: consolidated daemon_proc ≈690 code < 800. ✓ (verify in P1)

### Codex adversarial-review (2026-06-30, on v1) — carried into v2
- F1–F4 not independent (shared cli.rs + reorg) → P5 gated, serialized/range-assigned. ✓
- `main` not a removable term (matches 2 files) → stated; P6 keeps `main` excluded. ✓
- A/B keep-excluded could skip the contract → folded into evidence-first P1 (child) and P4 (logging): enumerate→classify→cover testable→justify residual. ✓
- #268 item 3 ridealong broke daemon-only boundary → split to Task G. ✓

## Dispatch summary

- **Serialized Phase 1 (renames/consolidation):** P1 → P2 → P3, one at a time, each rebasing after merge. These are the structural moves to `*_proc`.
- **After P1:** P4 (logging cover→include) and G (guard) — small, independent.
- **Phase 3:** P6 (render) parallel-safe; **P5/F1–F4 gated on the reorg decision**, then serialized/range-assigned.
- Every PR: `review-pr` + `/codex:adversarial-review`; paste measured global % (post-step regex).

## Open questions for the user

1. **CLI reorg ordering** (gates P5/F1–F4): land `feat/cli-command-reorg` first then rebase F onto it, or freeze the reorg and run F first? Largest chunk of #303 (848 missed lines).
2. **ruri_model fold vs rename** (P2): fold the loader into `embedder_proc` (cleanest regex, removes the term) only if `RuriModel` is embedder-process-only; if it has lib-level consumers, fall back to a covered/renamed lib file. Confirm appetite.
3. **Dispatch start:** begin P1 now (serialized chain) while the reorg/ruri questions settle, or hold for full sign-off?
