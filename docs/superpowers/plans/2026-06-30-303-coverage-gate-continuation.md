# #303 Coverage-Gate Continuation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each task is an independent PR off the LATEST origin/main, in its own worktree.

**Goal:** Continue #303 — move logic out of coverage-blind I/O-shell files into gate-counted `*_logic.rs` sibling modules, so production logic that currently escapes the `--fail-under-lines 90` gate becomes tested, while leaving only irreducible I/O shells excluded (each justified per-file in CLAUDE.md).

**Architecture:** Humble-object pattern, already established by #308 (`watcher_logic`), #311 (`backfill_logic`), #312 (embedder mock). Each excluded file keeps its accept/socket/spawn/println shell; its pure decision/format/parse logic moves verbatim into a covered sibling with characterization tests. The gate is a single GLOBAL aggregate (covered/total ≥ 90%), NOT per-file.

**Tech Stack:** Rust, `cargo llvm-cov`, in-memory SQLite for DB tests, `tempfile` for fs tests.

## Global Constraints

- **Gate is currently GREEN with headroom — this program is logic-protection, not gate-rescue.** Measured @ origin/main `3a1e626` (2026-06-30): with the current ignore regex the gate sees **95.42%** (13696 lines, 627 missed); whole-workspace without exclusions is 84.97%. Every task below ADDS covered lines (sibling + tests) and leaves the shell excluded, so the aggregate only rises — except where a task un-excludes a file (Tasks A/B only), which must be checked.
- Gate command: `cargo llvm-cov --ignore-filename-regex '<regex>' --fail-under-lines 90` (`.github/workflows/ci.yml:51-54`). Single global aggregate; `--fail-under-file-lines` is NOT used.
- **Naming footgun:** sibling module names must NOT match an ignore-regex term, or they get excluded too. Safe: `daemon_logic.rs` (≠ `daemon_mode.rs`), `embedder_mode_logic.rs` (≠ `embedder_mode.rs`), `cli_format.rs`/`cli_dict_logic.rs`/`cli_rebuild_logic.rs`/`cli_args_logic.rs` (≠ `cli.rs`). AVOID `embedder_logic.rs` (collides w/ `embedder.rs` term + confusing).
- TDD required: Red → Green → Refactor. New pub(crate) fns need in-file `#[cfg(test)] mod tests`. Extraction is behavior-preserving (verbatim move + call-site rewire); add characterization tests that pin the moved behavior.
- `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
- `npx jscpd` ≤ 5%; `lizard src/ --language rust -Tcyclomatic_complexity=15 -w` no new warnings; `tests/file_line_limits.rs` (ADR-0018) green.
- **Coverage % is test-inclusive** (#317): the headline % counts `#[cfg(test)]` lines. A reviewer must confirm the PRODUCTION branches of the moved logic are exercised, not just that the number rose via inline test bodies.
- Pure fns must NOT carry side effects in: do FS/socket/clock checks in the shell, pass the result (`bool`/`enum`/value) into the pure fn.
- Docs/comments in English; no inline ADR/issue refs in source comments (#304/#305). CLAUDE.md exclusion list + rationale updated in the SAME PR that changes the regex or the excluded-shell set.
- `docs/command-reference.md` only needs updating if the clap surface changes (`tests/cli_docs.rs` gate). The cli.rs extractions (Tasks F1–F4) are internal refactors and DO NOT change it — run `cargo test --test cli_docs` to confirm it stays green.
- the-space-memory work in a worktree off LATEST origin/main; PRs target main; never direct-push; PR body pastes the measured global %.

## Current state (origin/main `3a1e626`, regex @ ci.yml:53)

Regex: `(ruri_model|main|cli|logging|daemon_mode|embedder_mode|watcher_mode|child|backfill)\.rs`

| Step | Target | Status |
|---|---|---|
| 1 regex hygiene | dead terms | ✅ #307 |
| 2 status.rs | lib → gate | ✅ #310 |
| 4 watcher_mode | extract pure | ✅ #308/#309 (shell excluded) |
| 5 backfill | extract pure | ✅ #311 (shell excluded) |
| 7 embedder | trait+mock | ✅ #312 (`ruri_model.rs` shell excluded) |
| 3 logging.rs | un-exclude? | ⬜ **Task A — DECISION** |
| 6 child.rs | extract/justify | ⬜ **Task B — DECISION** |
| 8 daemon_mode | extract decisions | ⬜ Task D |
| 8 embedder_mode | extract decode | ⬜ Task E |
| 9 cli.rs | split (4 PRs) | ⬜ Tasks F1–F4 |
| 10 main.rs render_* | move out | ⬜ Task C |

Per-file measured coverage (no regex): `src/main.rs` 18.88% (378 missed) · `cli.rs` 65.13% (848 missed) · `child.rs` 52.24% (64 missed) · `daemon_mode.rs` 0% (301) · `embedder_mode.rs` 0% (133) · `logging.rs` 72.29% (23) · `bin/tsmd/main.rs` 0% (19). cli.rs (848) + main.rs (378) are ~90% of the missed lines.

---

## Coordination protocol (dispatcher reads first)

- **Most tasks are parallel-safe.** D, E, F1–F4, C only ADD a covered sibling + register a `mod` line + rewire call sites in the shell. They do NOT touch the regex. The only shared edits are the `mod x;` registration (in `lib.rs` or `bin/tsmd/main.rs`) and possibly CLAUDE.md — trivial 1-line conflicts, resolve-on-rebase.
- **Tasks A and B are the only ones that might remove a regex term.** With the gate at 95.42% there is ~5.4pt of headroom, but un-excluding a low-coverage file dilutes. **Each term removal must be re-measured with its EXACT regex variant before merge** — the headroom numbers below are illustrative, not a substitute for measurement:
  - `logging` removal (exposes `logging.rs` only, 83 lines / 23 missed) → ~95.3% (safe).
  - `child` removal (exposes `bin/tsmd/child.rs` only, 134 / 64) → re-measure.
  - **`main` is NOT a removable term.** The regex term `main` matches BOTH `src/main.rs` (466 / 378 missed) AND `src/bin/tsmd/main.rs` (19 / 19, a true 0%-covered entry). Removing it exposes both — the earlier "~88.8%" figure counted only `src/main.rs` and UNDERSTATES the dilution. `main` can only be touched by first making the regex path-specific (e.g. `src/main\.rs` + `bin/tsmd/main\.rs` as explicit shells), which is out of scope here. Task C therefore moves render logic to a NEW counted module and leaves `main` in the regex untouched.
  - If A/B do un-exclude, serialize them (one regex-edit PR in flight; next rebases after merge) and re-measure with the exact post-removal regex.
- **Merge order:** land logic-extraction PRs (D/E/F/C) first — they only raise the aggregate — then any A/B un-exclusion last.
- **⚠ In-flight CLI reorg conflict:** a local branch `feat/cli-command-reorg` (commit `docs(spec): top-level CLI command reorganization (index/db noun groups)`, not yet pushed) proposes regrouping the clap surface into `index`/`db` noun groups. Tasks F1–F4 extract cli.rs *internals* and assume the clap surface is unchanged (so `tests/cli_docs.rs` stays green). If the reorg lands first, the cli.rs call sites and `cmd_*` names shift and the F-tasks must rebase onto it; if the F-tasks land first, the reorg rebases onto smaller, logic-thin `cmd_*`. **Decide ordering with the user before dispatching F1–F4.** D/E/C are unaffected.
- Each PR: `review-pr` (Claude multi-agent) + `/codex:adversarial-review` before merge (repo convention), and paste the measured global % in the PR body.

---

## Task A: logging.rs — DECISION (evidence-first, not keep-by-default)

**Files:** `src/logging.rs` (143 lines, 72.29%, 23 missed). Already tested: `short_module`, `default_log_spec`, `init_logger(Stderr)`.

**Mandatory evidence step (do this BEFORE choosing an outcome):** run a fresh `cargo llvm-cov` and enumerate EACH of the 23 missed regions, classifying it pure/testable vs irreducible I/O. Known so far: the `tsm_log_format` body IS testable (construct a `log::Record` via `RecordBuilder`, write into a `Vec<u8>`, assert the `[ts] [LEVEL] [module] msg` shape) — so it must be extracted/tested, NOT waved off. The genuinely hard residual is the `LogMode::Daemon` file-logger arm (`OnceLock` runs the mode closure once per process → unreachable after any prior init) and `flexi_logger`'s global UTC-offset state.

**Outcome rule:** extract and test every pure/testable missed region first. ONLY the residual that is provably irreducible (the OnceLock daemon arm) may stay excluded, and only if removing `logging` from the regex would drop that file below the bar OR the residual cannot be reached. Document the residual line-by-line in CLAUDE.md. "Keep excluded" is a conclusion you EARN per-region, not a starting recommendation.

**DoD:** every testable missed region in logging.rs is covered; the CLAUDE.md exclusion note (if logging stays excluded) enumerates the specific irreducible residual lines with reasons; global ≥ 90%; if logging is dropped from the regex, re-measure with the exact post-removal regex.

## Task B: child.rs — DECISION (evidence-first, not keep-by-default)

**Files:** `src/bin/tsmd/child.rs` (209 lines, 52.24%, 64 missed). Already tested: `read_pid_from_file`, `is_process_alive`, `reap_child(None)`, `remove_stale_socket`.

**Mandatory evidence step:** enumerate each of the 64 missed regions and classify. Known testable (must be covered, NOT waved off): `reap_child` with a real exited child (`Command::new("true").spawn()` then wait); `stop_child` against a short-lived child (a `sleep`-style process — bound the 2s-grace flake risk with a process that exits promptly so the grace loop's success branch is hit deterministically); the `spawn_child` PID-write-FAILURE branch (point `pid_path` at a read-only/unwritable path). Genuinely awkward residual: `start_child_self` / `spawn_child` HAPPY path (they spawn `current_exe` = the test binary with daemon args).

**Outcome rule:** extract/test every region that the list above shows is reachable. Only the `current_exe`-spawning happy path may remain as justified residual. Document residual lines specifically in CLAUDE.md.

**DoD:** the testable missed regions (reap with real child, stop_child success, PID-write failure) are covered; CLAUDE.md residual note enumerates the specific irreducible lines; global ≥ 90%.

## Task C: main.rs render_* → covered module

**Files:** Modify `src/main.rs` (954 lines, 18.88%); host moved renderers in a covered module. **Constraint:** the regex term `main` matches BOTH `src/main.rs` AND `src/bin/tsmd/main.rs` by substring — `main` CANNOT be dropped (bin/tsmd/main.rs is a true entry). So the render logic must move to an ALREADY-COUNTED module (e.g. a new `src/render.rs` registered in `lib.rs`), NOT stay in main.rs.

**Extract (verbatim move, refactor to write into `&mut impl Write` or return `String`):** `render_search`/`render_index`/`render_ingest`/`render_status`/`render_doctor`/`render_reload`/`render_reindex`/`render_import_wordnet` (`main.rs:546-657`). `fn main` keeps thin `render_x(resp, &mut io::stdout())?` calls. `format_daemon_failure`/`wait_for_pid_exit` are already tested in main.rs's test module — leave them.

**Steps (per renderer, TDD):** write a test asserting the rendered text for a representative `DaemonResponse`; move the fn into `render.rs` taking a writer; rewire main.rs call site; run tests; commit.

**DoD:** all `render_*` in a covered module + tested; `src/main.rs` stays a justified shell (render calls now one-liners); global ≥ 90%; `tests/cli_docs.rs` green (clap surface unchanged).

> NOTE FOR DISPATCHER: `render_doctor` in main.rs delegates to `cli::render_doctor_report`, which Task F1 also touches. Coordinate Task C and F1 (or order F1 first) so the doctor renderer isn't split across two in-flight PRs.

## Task D: daemon_mode.rs → daemon_logic.rs (+ #268 items 3 & 5 ridealong)

**Files:** Create `src/bin/tsmd/daemon_logic.rs` (counted); modify `src/bin/tsmd/daemon_mode.rs` (466 lines, 0%); register `mod daemon_logic;` in `src/bin/tsmd/main.rs`.

**Extract pure (recon-grounded):**
- `embedder_child_args(model_dir: Option<&Path>) -> Vec<String>` — child-spawn arg assembly (run ~165-195): full set vs `--model`-omitted. Pure.
- `reload_response(warnings, watcher_pid_present: bool, sighup_ok: bool) -> DaemonResponse` — Reload-branch response assembly (handle_client ~430-455); the `config::reload()`/`libc::kill` I/O stays in the shell.
- `reindex_steps(kind: ReindexKind) -> &'static [ReindexStep]` — `Fts`/`Vectors`/`All` → `[Fts]`/`[Vectors]`/`[Fts,Vectors]`; pins the All ordering. Actual passes stay socket-coupled in backfill.

**#268 item 5 ONLY (in-scope because it lives in daemon_mode):**
- Move inline `ReindexGuard(Arc<AtomicBool>)` (`daemon_mode.rs:388`) into `daemon_logic.rs` as a named struct with `#[derive(Debug)]` + a drop test (drop → flag false). (#268 item 5)
- Do NOT touch `backfill_logic::PendingWriteGuard` here — that is #268 item 3, a different module owned by backfill work; it would break this PR's daemon-only boundary and can conflict with backfill continuation. It is split into **Task G**. The "single shared guard type" idea is rejected (counter `AtomicUsize` vs reset-flag `AtomicBool` are genuinely different; one generic type is over-engineering for 2 sites) — note this in the PR body.

**Shell stays excluded:** `run` (DB probe/open, lock, socket bind, PID write, status update, signals, child spawn, threads, accept loop), `handle_client` dispatch (read/write request, reader-pool vs writer routing — `is_read_only` already in daemon_protocol; harvest already in backfill_logic).

**Honesty note for PR body:** daemon_mode's pure surface is small (much was already extracted in #301/#311), so coverage gain is modest. That's expected.

**DoD:** daemon_logic.rs created + tested; ReindexGuard named/Debug/tested in daemon_logic; NO change to backfill_logic; global ≥ 90%; daemon_mode.rs shell-only.

## Task G: #268 item 3 — PendingWriteGuard Debug + doc (standalone tiny PR)

**Files:** Modify `src/bin/tsmd/backfill_logic.rs` only (the `PendingWriteGuard` struct, ~line 17).

Split out of Task D per codex review: item 3 edits backfill-owned code, so it must NOT ride in the daemon PR. Add `#[derive(Debug)]` to `PendingWriteGuard` and a doc note that `mem::forget` leaves the counter elevated. Trivial; no coverage impact (backfill_logic is already counted). Serialize against any other in-flight backfill_logic change.

**DoD:** PendingWriteGuard has `#[derive(Debug)]` + doc note; `cargo test`/clippy/fmt green; no behavior change.

## Task E: embedder_mode.rs → embedder_mode_logic.rs

**Files:** Create `src/bin/tsmd/embedder_mode_logic.rs` (counted); modify `src/bin/tsmd/embedder_mode.rs` (183 lines, 0%); register `mod embedder_mode_logic;`.

**Extract pure (recon-grounded, cleaner surface than daemon_mode — real coverage gain):**
- `parse_texts(req: &serde_json::Value) -> Vec<String>` — texts extraction (handle_client 135-145); test empty/missing/non-array/non-string-element.
- `panic_message(info: &(dyn Any + Send)) -> String` — downcast String/&str/unknown (170-179). Pure.
- `encode_response(result: Result<Vec<Vec<f32>>>) -> serde_json::Value` — success `{"embeddings":…}` / error `{"error":…}` shaping (the `catch_unwind(embedder.encode)` stays in the shell; result→JSON is pure).
- `resolve_model_load(model_dir: Option<&Path>, all_files_present: bool) -> ModelLoadPlan` — load_model path decision (FromDir vs Default); the `MODEL_FILES.iter().all(is_file)` FS check stays in the shell, passes the bool in.

**Shell stays excluded:** `run` (env/logger), `run_daemon` (socket bind/remove, real model load, accept loop, watchdog spawn), `watchdog` (sleep loop, poke), `handle_client` read/write/`catch_unwind`/shutdown.

**DoD:** embedder_mode_logic.rs created + tested (decode/response shaping); global ≥ 90%; embedder_mode.rs shell-only.

## Tasks F1–F4: cli.rs split (4 independent PRs, separate modules)

cli.rs is 3617 lines (code 1–2301; tests 2302–3617, 51 existing tests), 848 missed — the bulk of #303's remaining un-gated logic. The four groups touch DISJOINT functions, but **every F task also rewires call sites inside the same `src/cli.rs`**, so they are NOT conflict-free in the way the sibling modules are — concurrent F PRs will collide on cli.rs call-site edits, and the unpushed `feat/cli-command-reorg` may rename/move those same `cmd_*` sites. cli.rs shell stays excluded. `tests/cli_docs.rs` stays green ONLY if the clap surface is unchanged — but if the reorg lands, characterization tests written against the pre-reorg command layout go stale.

**Dispatch gate for F1–F4 (codex-required): do NOT launch these in the parallel wave.** First resolve the `feat/cli-command-reorg` ordering with the user. Then run F1–F4 EITHER (a) serialized through a single integration branch, each rebasing + re-running `cargo test`/`cargo test --test cli_docs` after the previous merges, OR (b) with explicit, non-overlapping cli.rs call-site line ranges assigned per task and a mandatory rebase+test step after each merge. Pick (a) if the reorg is imminent; (b) only if the reorg is deferred.

### Task F1 (highest value first): `cli_format.rs` (~200 lines)
Extract the display/format logic, refactored to build `String`/write into a writer (no direct `println!` in the pure fn):
- `render_doctor_report` box drawing (strip_ansi, width calc, row assembly; `cli.rs:1485`, ~100 lines) — NB coordinate with Task C (main.rs `render_doctor` delegates here).
- `print_status_info` line assembly (pct, eta selection, branches; `cli.rs:1677`, ~85 lines).
- `format_since` (`cli.rs:1771`, RFC3339→HH:MM:SS), `estimate_eta` (`cli.rs:1777`; parameterize `now`), `print_candidate` formatting (`cli.rs:2078`).

### Task F2: `cli_dict_logic.rs` (~60–80 lines)
- `classify_reindex` (+`ReindexPlan`) (`cli.rs:1839`) — daemon response → plan; pure.
- `cmd_dict_add`/`rm`/`reject` (`cli.rs:1949-1999`) verdict-transition → user-message mapping (extract the message-selection, leave DB in shell).
- `report_reconcile` message generation (`cli.rs:1908`).

### Task F3: `cli_rebuild_logic.rs` (~40–60 lines)
- `decide_backfill_action(vecs, chunks, socket_exists, backfill_in_progress) -> Action` — extracted from `report_vectors_and_backfill` (`cli.rs:2255`).
- `rebuild_dry_run` caution-text assembly (`cli.rs:2119`).

### Task F4: `cli_args_logic.rs` (~80–100 lines)
- `normalize_path_filters` (`cli.rs:385`, already pure — add tests).
- `should_place_cache_resource` (`cli.rs:211`), `same_entry_location` (`cli.rs:236`).
- `run_search` (`cli.rs:416`) fallback/temporal resolution (extract the decision; searcher call stays in shell).

**Per-F-task steps (TDD):** write failing characterization test in the new module → move the fn verbatim, refactor to pure (writer/return) → rewire cli.rs call site → `cargo test` + `cargo test --test cli_docs` (green) → `clippy --all-targets`/`fmt` → measure global ≥ 90% → commit.

**DoD (each F):** new module + tests; cli.rs call sites rewired; cli.rs shell-only for that area; cli_docs green; global ≥ 90%.

---

## Self-review (spec coverage)

- #303 steps 3,6,8,9,10 → Tasks A,B,D+E,F1–F4,C. Steps 1,2,4,5,7 already shipped (#307/#310/#308/#309/#311/#312). ✓
- #268 item 5 (ReindexGuard) → Task D; item 3 (PendingWriteGuard) → Task G (split out per codex — different module). ✓
- Reframe captured: gate is at 95.42% (logic-protection, not rescue); shells stay excluded; only A/B might un-exclude. ✓
- Naming footgun, cli_docs gate, `main` regex-term collision, #317 test-inclusive caveat, render_doctor C↔F1 overlap — all flagged. ✓

### Codex adversarial-review (2026-06-30) — incorporated

Verdict was needs-attention/no-ship; all four findings applied:
- [high] F1–F4 not independent (shared cli.rs call sites + reorg) → pulled from parallel wave; gated on reorg ordering; serialize/range-assign. ✓
- [high] `main`-removal coverage math counted only one of two matched files → `main` declared non-removable; per-term re-measurement required. ✓
- [medium] A/B keep-excluded could close #303 without proving the boundary → A/B made evidence-first (enumerate→classify→extract testable→justify residual). ✓
- [medium] #268 item 3 ridealong broke Task D's daemon-only boundary → split into Task G. ✓

## Dispatch summary (revised per codex review)

- **Parallel wave — daemon/embedder/render only:** D (daemon_logic), E (embedder_mode_logic), C (main render_* → render.rs), G (PendingWriteGuard Debug/doc). Fan across agents, each its own worktree+PR off latest origin/main. These add covered siblings + small shell rewiring, do NOT touch the regex, and touch disjoint files → genuinely parallel-safe. Order C after F1 ONLY if F1 is being run (shared `render_doctor_report`); if F-group is deferred, C proceeds (it owns main.rs's response renderers, distinct from cli.rs's doctor box).
- **F1–F4 (cli.rs) — NOT in the parallel wave.** Blocked on the `feat/cli-command-reorg` ordering decision (user). Then serialized via an integration branch OR non-overlapping line ranges + mandatory rebase/test after each merge (see Task F intro).
- **Decision tasks A, B — evidence-first, run after the parallel wave.** Each must enumerate its missed regions, extract+test the pure/testable ones, and only then justify the irreducible residual line-by-line in CLAUDE.md. If either ends up dropping a regex term, serialize it (one regex-edit PR in flight) and re-measure with the exact post-removal regex. `main` is NOT removable.
- Every PR: review-pr + /codex:adversarial-review before merge; paste the measured global % in the PR body.

## Open questions for the user (resolve before full dispatch)

1. **CLI reorg ordering:** land `feat/cli-command-reorg` first (then F1–F4 rebase onto it), or freeze the reorg and run F1–F4 first? This gates the largest chunk of #303 (848 missed lines).
2. **A/B appetite:** willing to invest in the testable regions (logging formatter, child reap/stop/PID-fail) per the evidence-first rule, or accept a documented-residual exclusion for the parts that are reachable-but-costly? (Codex pushed back on keep-by-default.)
3. **Dispatch breadth now:** start the safe wave (D, E, C, G) immediately while 1–2 are decided, or hold everything until the whole sequence is settled?
