# Lexer-based trailing test-module detection for the line-count gate

Issue: [#314](https://github.com/key/the-space-memory/issues/314)
Related: ADR-0018 (per-file line-count gate)

## Problem

`tests/file_line_limits.rs::code_line_count` strips the trailing
`#[cfg(test)] mod tests { … }` block from the counted production lines using a
**line-based heuristic**:

- boundary = the last column-0 `#[cfg(test)]` whose next non-blank line opens
  `mod tests`;
- production lines = `[0, boundary)`;
- the file must end at that module's column-0 `}` (an EOF-`}` check), otherwise
  it errors.

The interior of the module is deliberately never scanned, because test fixtures
legitimately contain raw strings (`r#"…"#`) with column-0 `{` / `}` (TOML/JSON),
which a naive brace-depth scan would misread. ADR-0018 documents this as an
honest known limitation and defers the lexer-based fix to a follow-up — this
work.

### The gap

Because the boundary is not lexically exact, production code placed **after**
the trailing `mod tests` is silently excluded from the count as long as the file
still ends in a column-0 `}`. The EOF-`}` check catches trailing prose/comments
but is not a complete guard: it relies on the repo convention that the inline
test module is the file's last item.

## Goal

Detect the trailing test module with a real Rust lexer so the boundary is exact.
Code before **and after** the module is counted; the raw-string false-positive
class disappears because a lexer treats each raw string as one literal token.

Non-goals (unchanged): the threshold / baseline / ratchet policy of ADR-0018,
the three invariants, the baseline file, and the CI wiring. This work changes
**only** the `code_line_count` mechanism.

## Approach

Replace the line heuristic in `code_line_count` with **proc-macro2 token
scanning**.

### Dependency

Add to `[dev-dependencies]` in `Cargo.toml`:

```toml
proc-macro2 = { version = "1.0.106", features = ["span-locations"] }
```

`proc-macro2`, `quote`, `syn`, and `unicode-ident` are already present
transitively in `Cargo.lock` (proc-macro2 resolves to `1.0.106`), so this adds
no new crate to the dependency graph — it only names an existing crate as a
direct dev-dependency and enables its `span-locations` feature. Pin the version
to the resolved `1.0.106` (a version whose fallback mode provides real span
line/column data) so the central span-location dependency cannot silently regress
under version skew.

The `span-locations` feature is required so that, in proc-macro2's fallback mode
(a normal `cargo test`, not a proc-macro context), `Span::start()` and
`Group::span_close()` return real source line numbers rather than the
`line = 0` placeholder the feature-less fallback yields. It is dev-only and never
ships in the released binary. `syn` (full parse) was considered and rejected:
token scanning is lighter, tokenizing is more lenient than parsing, and it
handles the raw-string concern natively.

Because fallback-mode span behavior is the algorithm's central correctness
dependency, it is characterized directly by tests (see Test plan) rather than
merely assumed.

### Detection semantics

- Tokenize `content` into a `proc_macro2::TokenStream`. **On a lex error, return
  `Err` (fail-closed).** The filesystem gate already attributes each error to its
  file (`Err(e) => "{rel}: {e}"`), so a lex failure names the offending file
  rather than blocking opaquely. In practice `src/**/*.rs` always tokenizes (it
  is the crate itself); this path is a safety net, not an expected outcome.
- Walk the **top-level** token sequence as a series of items. For each item,
  first consume its run of leading **outer attributes** — each is a `#` `Punct`
  followed by a `Group(Bracket)` — recording whether any of them is `cfg(test)`.
  Then inspect the item head: optional visibility (`pub` ident, optionally
  followed by a `Group(Paren)` for `pub(crate)` / `pub(super)`), then `mod`,
  `tests`, `{ … }`.
  - A match is a `mod tests` item **whose leading attribute run contains
    `cfg(test)` in any position** — `cfg(test)` need not be the first attribute,
    and preceding `#[doc = …]` (doc comments lower to this), `#[allow(…)]`, etc.
    do not defeat the match.
  - `cfg(test)` is matched as ident `cfg` + a paren `Group` holding the single
    ident `test`. Broader predicates (`all(test, …)`) and `cfg_attr(…, …)` are
    **not** matched (documented out of scope); such a module counts as
    production. This is fail-noisy (over-count → louder gate), not a silent pass.
  - The module name must be exactly `tests` (ident equality), so
    `mod tests_helpers` is not matched — same as `opens_test_module` today.
- Take the **last** matching item (ADR wording: "the last `#[cfg(test)]` …
  opening `mod tests`"). Non-`mod tests` `#[cfg(test)]` items
  (`#[cfg(test)] pub fn helper`, `#[cfg(test)] pub mod test_utils;`) are not
  matched and stay counted.
- The excluded region is that item's line span:
  - `start_line` = source line of the `#` token opening its **first** leading
    attribute (so the whole attribute run is excluded with the block);
  - `end_line` = source line of the block's closing `}` (`Group::span_close`).
  - **Boundary-isolation validation (fail-closed):** the excluded region is
    subtracted as whole physical lines, which is only correct when the boundary
    lines are not shared with production tokens. Validate that no top-level
    production token lies on `start_line` before the attribute run, and none lies
    on `end_line` after the closing `}`. If a boundary line is shared (e.g.
    `fn prod() {} #[cfg(test)] mod tests {}` on one line), return `Err` rather
    than silently undercounting. Real source keeps the module line-isolated, so
    this errs loudly only on the pathological case.
  - excluded = `end_line - start_line + 1`.
- **`count = total_physical_lines - excluded`**, where `total_physical_lines =
  content.lines().count()` (same line model as the current code; LF assumed, per
  repo convention). Lines before and after the module both count.
- No matching test module → `count = total_physical_lines`.

### Why the count is backward-compatible for the common case

When the module sits at EOF (the overwhelming convention),
`total_physical_lines - excluded == start_line - 1`, which equals the current
heuristic's `boundary`. So every baselined file's count is unchanged. The result
differs **only** when content follows the module — exactly the case #314 wants to
fix, where that content flips from "excluded" to "counted".

## Behavior changes (intended)

Two existing unit tests assert the *old* rejection behavior and must flip to the
new counting behavior:

| Test | Old | New |
|---|---|---|
| `count_rejects_production_code_after_test_module` | `is_err()` | `Ok(2)` (the trailing `fn hidden()` is counted) |
| `count_rejects_content_after_test_module` | `is_err()` | `Ok(2)` (the trailing comment counts as a production line) |

These renames/expectation changes are the observable point of the issue: code
after the module is counted rather than dropped or rejected.

## Test plan (TDD)

New tests to add:

Core behavior:

- `count_counts_code_after_test_module` — a production item after the trailing
  `mod tests` is included in the count.
- `count_strips_midfile_test_module_and_counts_rest` — a `#[cfg(test)] mod
  tests` not at EOF is excluded while surrounding production code is counted.

Raw-string correctness (the point of lexing):

- `count_strips_test_module_with_col0_raw_string` — a `mod tests` whose body
  contains a raw string (`r#" … "#`) with column-0 `{` / `}` is excluded
  correctly. The old heuristic's entire reason for "don't scan the interior" is
  resolved by lexing.

Attribute-matching robustness (finding #2):

- `count_matches_cfg_test_not_first_attribute` — `#[allow(dead_code)]
  #[cfg(test)] mod tests { … }` is still detected and stripped.
- `count_matches_cfg_test_after_doc_comment` — a doc comment (lowered to
  `#[doc = …]`) before `#[cfg(test)] mod tests` does not defeat the match.
- `count_does_not_match_cfg_attr_wrapped` — `#[cfg_attr(feature = "x", cfg(test))]
  mod tests { … }` is **not** matched (counts as production; documented scope).
- `count_takes_last_of_multiple_mod_tests` — two `#[cfg(test)] mod tests` blocks;
  only the last is the boundary.

Boundary-isolation + fail-closed (finding #1, #3):

- `count_errors_on_code_sharing_attribute_line` — production code on the same
  physical line as the `#[cfg(test)]` attribute returns `Err`.
- `count_errors_on_code_sharing_close_brace_line` — production code after the
  module's closing `}` on the same line returns `Err`.
- `count_errors_on_unlexable_input` — an unterminated string literal (or similar
  lexically-invalid input) returns `Err` (fail-closed).

Span-location characterization (finding #4) — pin the proc-macro2 fallback
behavior the algorithm depends on:

- `spans_report_real_lines_for_multiline_module` — a multiline `mod tests`
  yields `start_line`/`end_line` matching the physical source lines.
- `count_correct_without_trailing_newline` — a file whose last line has no
  trailing `\n` still counts correctly.
- `count_correct_with_raw_string_spanning_lines` — a multiline raw string inside
  the module does not shift the detected `end_line`.

Unchanged (must stay green, values identical):
`count_no_test_module_is_whole_file`, `count_excludes_trailing_test_module`,
`count_allows_visibility_on_test_module`,
`count_includes_cfg_test_helpers_before_trailing_module`,
`count_no_trailing_mod_tests_counts_whole_file`,
`count_does_not_match_mod_tests_prefix_name`, and all baseline/invariant tests.

The `enforce_per_file_line_limits` filesystem test must still pass against the
current tree: since every real file keeps its `mod tests` at EOF, all counts are
identical and the baseline file needs no change.

## ADR-0018 handling

ADR-0018's "境界検証と既知の限界" section, its honest known-limitation caveat,
and the rationale paragraph "なぜモジュール内部を走査しないか" (and the
"grep 一発で再現できる透明さ" argument) become stale: transparency now rests on
lexer-exactness rather than grep-reproducibility, and the known limitation is
resolved.

Decision: **amend ADR-0018 in place** (a new/superseding ADR is not warranted —
this fulfils the follow-up ADR-0018 itself named, not a policy reversal). Per the
repo's target-state ADR convention (`decisions/README.md`: target-state only, no
review-process attribution), the ADR describes the current mechanism rather than
a changelog; the before/after provenance lives in issue #314 and git history.

The amendment must:

- rewrite "境界検証と既知の限界" and the "なぜモジュール内部を走査しないか" /
  "grep 一発で再現できる透明さ" rationale to describe lexer-exact detection as
  the target state, replacing grep-reproducibility with lexer-exactness as the
  transparency basis;
- state plainly that content **after** the trailing `mod tests` is now counted
  (previously excluded/rejected by the heuristic's EOF-`}` guard) — so the
  semantic is explicit in the ADR, not silently dropped — and reference #314 as
  the resolved follow-up;
- keep the threshold / baseline / ratchet policy and the three invariants
  untouched;
- bump `updated:`.

Note that the ADR reference in `code_line_count`'s own doc comment (which
currently describes the heuristic and its known limitation) must be rewritten to
match.

## Definition of Done

- `cargo test` green (flipped + new + unchanged tests, and the filesystem gate).
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` clean.
- Coverage ≥ 90% on covered modules; `npx jscpd` ≤ 5%; `lizard` no new CCN
  warnings.
- ADR-0018 amended; CLAUDE.md/README updated only if a user-facing surface
  changes (none expected — this is an internal test gate).
- PR body and commits in English; work on a `feat/` branch; PR reviewed via
  `/codex:adversarial-review` before merge.
