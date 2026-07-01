//! Per-file code-line gate (ADR-0018). Every `src/**/*.rs` file's *code* line
//! count (the production lines, excluding the trailing `#[cfg(test)] mod tests`
//! block) must stay within `max(800, baseline[file])`, where `baseline` is the
//! checked-in `tests/file-line-baseline.txt`. The baseline is a frozen ratchet:
//! it may only shrink or lose entries, never grow — enforced against
//! `origin/main` so the limit cannot be raised in the same PR that grows a file.
//!
//! The counting and the three invariants are pure functions, unit-tested below;
//! the enforcement test wires them to the filesystem and git.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};

/// Flat code-line cap for any file without a (larger) frozen baseline.
const LIMIT: usize = 800;

/// Path of the checked-in baseline, relative to the crate root. Used both for
/// `include_str!` (current tree) and `git show` (base branch).
const BASELINE_REL: &str = "tests/file-line-baseline.txt";

// ── Counting (lexer-based test-module detection) ─────────────────────────────

/// Count production code lines in a Rust source file using proc-macro2 token
/// scanning. Every physical line is counted except the span of the **last**
/// top-level `#[cfg(test)] mod tests { … }` item, including its leading
/// attribute run. Lines before **and** after the module are both counted.
///
/// Detection: the top-level token sequence is scanned for the anchor pattern
/// `mod tests Group(Brace)`. Each anchor is walked backward over optional
/// visibility (`pub` / `pub(…)`) and a contiguous run of outer attributes
/// (`# Group(Bracket)`). If any attribute in the run is exactly `cfg(test)`
/// (ident `cfg` + parenthesised single ident `test`), the item is a match.
/// Broader predicates (`cfg(all(test,…))`) and `cfg_attr(…)` are out of scope
/// and count as production. The **last** match determines the excluded region.
///
/// Span: `start_line` = source line of the first `#` in the attribute run;
/// `end_line` = source line of the block's closing `}` (`Group::span_close`).
/// The `span-locations` feature of proc-macro2 is required for real line numbers.
///
/// Boundary-isolation (fail-closed): if a top-level production token ends on
/// `start_line` or starts on `end_line`, the boundary is ambiguous and the
/// function returns `Err` rather than silently miscount. A lex error also
/// returns `Err` (fail-closed; in practice `src/**/*.rs` always tokenises).
///
/// No matching module → count equals `content.lines().count()` (whole file).
fn code_line_count(content: &str) -> Result<usize, String> {
    let stream = content
        .parse::<TokenStream>()
        .map_err(|e| format!("lex error: {e}"))?;

    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let total = content.lines().count();
    let n = tokens.len();

    // Span of the last matching #[cfg(test)] mod tests { … } item.
    struct Candidate {
        start_line: usize, // 1-based; line of the first `#` in the attribute run
        end_line: usize,   // 1-based; line of the block's closing `}`
        run_start: usize,  // token index of the first `#` in the attribute run
        brace_idx: usize,  // token index of the block Group
    }

    let mut last: Option<Candidate> = None;
    let mut i = 0;

    while i < n {
        // Locate the anchor: Ident("mod") Ident("tests") Group(Brace).
        let brace_group = if i + 2 < n {
            match (&tokens[i], &tokens[i + 1], &tokens[i + 2]) {
                (TokenTree::Ident(m), TokenTree::Ident(t), TokenTree::Group(g))
                    if m == "mod" && t == "tests" && g.delimiter() == Delimiter::Brace =>
                {
                    Some(g)
                }
                _ => None,
            }
        } else {
            None
        };

        let Some(brace_group) = brace_group else {
            i += 1;
            continue;
        };

        let brace_idx = i + 2;
        let mut j = i; // walk backward from `mod`

        // Walk back over optional visibility: pub / pub(…).
        if j > 0 {
            match &tokens[j - 1] {
                TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                    // Could be the `(crate)` in `pub(crate)`.
                    if j >= 2 {
                        if let TokenTree::Ident(id) = &tokens[j - 2] {
                            if id == "pub" {
                                j -= 2;
                            }
                        }
                    }
                }
                TokenTree::Ident(id) if id == "pub" => {
                    j -= 1;
                }
                _ => {}
            }
        }

        // Walk back over a contiguous run of outer attributes: each is `#` + `Group(Bracket)`.
        let mut run_start = j;
        let mut has_cfg_test = false;
        while j >= 2 {
            match (&tokens[j - 2], &tokens[j - 1]) {
                (TokenTree::Punct(p), TokenTree::Group(g))
                    if p.as_char() == '#' && g.delimiter() == Delimiter::Bracket =>
                {
                    if is_cfg_test(g) {
                        has_cfg_test = true;
                    }
                    run_start = j - 2;
                    j -= 2;
                }
                _ => break,
            }
        }

        if has_cfg_test {
            last = Some(Candidate {
                start_line: tokens[run_start].span().start().line,
                end_line: brace_group.span_close().start().line,
                run_start,
                brace_idx,
            });
        }

        i = brace_idx + 1;
    }

    let Some(c) = last else {
        return Ok(total);
    };

    // Boundary-isolation: the excluded span covers whole physical lines, which is
    // only correct when no production token shares a boundary line.
    if c.run_start > 0 {
        let prev = &tokens[c.run_start - 1];
        if prev.span().end().line == c.start_line {
            return Err(format!(
                "a production token ends on the same line as the `#[cfg(test)]` \
                 attribute (line {}) — the attribute must open on its own line",
                c.start_line
            ));
        }
    }
    if c.brace_idx + 1 < n {
        let next = &tokens[c.brace_idx + 1];
        if next.span().start().line == c.end_line {
            return Err(format!(
                "a production token starts on the same line as the `mod tests` \
                 closing `}}` (line {}) — the closing brace must be on its own line",
                c.end_line
            ));
        }
    }

    let excluded = c.end_line - c.start_line + 1;
    Ok(total - excluded)
}

/// Return `true` if `bracket` is the `[…]` content of a `#[cfg(test)]` attribute:
/// ident `cfg` followed by a parenthesised group containing the single ident `test`.
/// Broader predicates (`cfg(all(test,…))`) and `cfg_attr(…)` return `false`.
fn is_cfg_test(bracket: &Group) -> bool {
    let inner: Vec<TokenTree> = bracket.stream().into_iter().collect();
    // Must be at least `cfg(test)` — two tokens.
    if inner.len() < 2 {
        return false;
    }
    // First token must be ident `cfg` (not `cfg_attr`, `allow`, `doc`, …).
    let TokenTree::Ident(name) = &inner[0] else {
        return false;
    };
    if name != "cfg" {
        return false;
    }
    // Second token must be a parenthesised group containing a single ident `test`.
    let TokenTree::Group(paren) = &inner[1] else {
        return false;
    };
    if paren.delimiter() != Delimiter::Parenthesis {
        return false;
    }
    let paren_tokens: Vec<TokenTree> = paren.stream().into_iter().collect();
    if paren_tokens.len() != 1 {
        return false;
    }
    matches!(&paren_tokens[0], TokenTree::Ident(id) if id == "test")
}

// ── Baseline parsing ─────────────────────────────────────────────────────────

/// Parse `path<space>count` lines; `#` comments and blank lines are ignored.
fn parse_baseline(s: &str) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((path, count)) = line.rsplit_once(char::is_whitespace) {
            if let Ok(n) = count.trim().parse::<usize>() {
                m.insert(path.trim().to_string(), n);
            }
        }
    }
    m
}

// ── Invariants ───────────────────────────────────────────────────────────────

/// Invariant 1 (no growth) + invariant 2 (tight baselines): every file stays
/// within `max(LIMIT, baseline)`, and every baseline entry is `> LIMIT` and
/// equals its file's current count (stale/oversized headroom is rejected).
fn growth_and_tightness_violations(
    current: &BTreeMap<String, usize>,
    baseline: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut v = Vec::new();

    for (f, &c) in current {
        let allowance = baseline.get(f).copied().unwrap_or(LIMIT).max(LIMIT);
        if c > allowance {
            v.push(format!(
                "{f}: {c} code lines exceed the limit {allowance} \
                 (split the file; new files are capped at {LIMIT})"
            ));
        }
    }

    for (f, &b) in baseline {
        if b <= LIMIT {
            v.push(format!(
                "{f}: baseline {b} is <= {LIMIT}; remove this entry (it needs no baseline)"
            ));
            continue;
        }
        match current.get(f) {
            None => v.push(format!(
                "{f}: baseline entry for a file that no longer exists; remove it"
            )),
            Some(&c) if c < b => v.push(format!(
                "{f}: shrank to {c}; lower its baseline from {b} to {c}"
            )),
            Some(_) => {}
        }
    }

    v
}

/// Invariant 3 (immutable ratchet): relative to the base branch's baseline, no
/// entry may be added or raised. Lowering or removing entries is allowed.
fn immutability_violations(
    current_baseline: &BTreeMap<String, usize>,
    base_baseline: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut v = Vec::new();
    for (f, &c) in current_baseline {
        match base_baseline.get(f) {
            None => v.push(format!(
                "{f}: baseline entry added vs origin/main; baselines may only shrink or be \
                 removed (a new oversized file must be split to <= {LIMIT}, not baselined)"
            )),
            Some(&b) if c > b => v.push(format!(
                "{f}: baseline raised {b} -> {c} vs origin/main; baselines may only shrink"
            )),
            Some(_) => {}
        }
    }
    v
}

// ── Filesystem / git glue (thin; the logic above is what's tested) ───────────

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// State of the base branch's baseline, used to drive invariant 3.
enum BaseBaseline {
    /// `origin/main` has the baseline file — enforce invariant 3 against it.
    Content(String),
    /// `origin/main` resolves but carries no baseline file yet. This is the
    /// bootstrap window: only the introducing PR (and the commits before this
    /// gate first lands on `main`) hit it. Invariant 3 arms automatically once
    /// the baseline exists on `main`.
    BootstrapAbsent,
    /// `origin/main` could not be resolved at all (shallow/local checkout, or a
    /// CI job that failed to fetch it). Invariant 3 cannot be checked.
    RefUnavailable,
}

fn base_baseline(root: &Path) -> BaseBaseline {
    let ref_ok = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "origin/main"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ref_ok {
        return BaseBaseline::RefUnavailable;
    }
    // `origin/main` exists; a failed `git show` here means the baseline file is
    // simply not in that tree yet (bootstrap), not a missing ref.
    match Command::new("git")
        .args(["show", &format!("origin/main:{BASELINE_REL}")])
        .current_dir(root)
        .output()
    {
        Ok(o) if o.status.success() => {
            BaseBaseline::Content(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        _ => BaseBaseline::BootstrapAbsent,
    }
}

/// Whether invariant 3 must run fail-closed: the dedicated `file-lines` CI job
/// sets this, having fetched `origin/main`. There, an unresolvable base is a
/// setup failure and errors rather than silently skipping. Other contexts (the
/// general `test` job, local `cargo test`) leave invariant 3 to that job.
fn invariant3_is_strict() -> bool {
    std::env::var_os("TSM_FILE_LINE_GATE_STRICT").is_some()
}

#[test]
fn enforce_per_file_line_limits() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut files = Vec::new();
    collect_rs_files(&root.join("src"), &mut files);
    files.sort();

    let mut violations: Vec<String> = Vec::new();
    let mut current: BTreeMap<String, usize> = BTreeMap::new();
    for f in &files {
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read_to_string(f).unwrap();
        match code_line_count(&content) {
            Ok(c) => {
                current.insert(rel, c);
            }
            Err(e) => violations.push(format!("{rel}: {e}")),
        }
    }

    let baseline = parse_baseline(include_str!("file-line-baseline.txt"));
    violations.extend(growth_and_tightness_violations(&current, &baseline));

    // Invariant 3 (immutability) needs the base branch. It is authoritative only
    // in the strict `file-lines` job; elsewhere it is skipped (that job covers
    // it). In strict mode an unresolvable base is fail-closed, not fail-open.
    let strict = invariant3_is_strict();
    match base_baseline(root) {
        BaseBaseline::Content(base) => {
            violations.extend(immutability_violations(&baseline, &parse_baseline(&base)));
        }
        BaseBaseline::BootstrapAbsent => eprintln!(
            "[file-lines] no baseline on origin/main yet (bootstrap); invariant 3 \
             (immutability) arms once this lands on main"
        ),
        BaseBaseline::RefUnavailable if strict => violations.push(
            "invariant 3 (immutability) could not run: origin/main is unavailable in the \
             strict file-lines job — it must fetch the base branch before running"
                .to_string(),
        ),
        BaseBaseline::RefUnavailable => eprintln!(
            "[file-lines] origin/main unavailable (local/shallow checkout); invariant 3 \
             (immutability) is enforced by the strict file-lines CI job"
        ),
    }

    assert!(
        violations.is_empty(),
        "per-file line-count gate (ADR-0018) failed:\n  {}",
        violations.join("\n  ")
    );
}

// ── Unit tests for the pure logic ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, usize)]) -> BTreeMap<String, usize> {
        entries.iter().map(|(p, n)| (p.to_string(), *n)).collect()
    }

    #[test]
    fn count_no_test_module_is_whole_file() {
        let src = "fn a() {}\n\nfn b() {}\n";
        assert_eq!(code_line_count(src), Ok(3));
    }

    #[test]
    fn count_excludes_trailing_test_module() {
        let src = "use x;\n\nfn a() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
        // Production = lines before `#[cfg(test)]` (index 3) = 3 lines.
        assert_eq!(code_line_count(src), Ok(3));
    }

    #[test]
    fn count_allows_visibility_on_test_module() {
        let src = "fn a() {}\n#[cfg(test)]\npub mod tests {\n    fn t() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn count_includes_cfg_test_helpers_before_trailing_module() {
        // `#[cfg(test)] pub fn helper` is test-only support code in the production
        // region — it is counted; only the trailing `mod tests` is stripped.
        let src = "fn a() {}\n#[cfg(test)]\npub fn helper() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(3));
    }

    #[test]
    fn count_no_trailing_mod_tests_counts_whole_file() {
        // A `#[cfg(test)]` that is not a trailing `mod tests` (e.g. a test_utils
        // module declaration, as in lib.rs) does not strip anything.
        let src = "pub mod a;\n#[cfg(test)]\npub mod test_utils;\n";
        assert_eq!(code_line_count(src), Ok(3));
    }

    #[test]
    fn count_does_not_match_mod_tests_prefix_name() {
        // `mod tests_helpers` is not the test module.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests_helpers {\n    fn t() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(5));
    }

    #[test]
    fn count_counts_production_code_after_test_module() {
        // Production code after the trailing test module is now counted (not rejected).
        // fn a (line 1) + fn hidden (line 6) = 2 production lines.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn hidden() {}\n";
        assert_eq!(code_line_count(src), Ok(2));
    }

    #[test]
    fn count_counts_comment_after_test_module() {
        // A trailing comment counts as a production line (comments are not tokens;
        // no boundary error, but content.lines() counts the line).
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n// trailing\n";
        assert_eq!(code_line_count(src), Ok(2));
    }

    // ── New tests: core behavior ──────────────────────────────────────────────

    #[test]
    fn count_strips_midfile_test_module_and_counts_rest() {
        // A mod tests that is not at EOF: production code before and after is counted.
        // fn a + fn b before (2 lines) and fn c after (1 line) = 3 production lines.
        let src = "fn a() {}\nfn b() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn c() {}\n";
        assert_eq!(code_line_count(src), Ok(3));
    }

    // ── New tests: raw-string correctness ─────────────────────────────────────

    #[test]
    fn count_strips_test_module_with_col0_raw_string() {
        // The module body contains a raw string with column-0 `{` / `}`.
        // A lexer treats the whole raw string as one token, so the boundary is exact.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    let s = r#\"\n{\n}\n\"#;\n}\n";
        // 8 total lines; mod tests spans lines 2-8 (7 lines excluded) → count = 1.
        assert_eq!(code_line_count(src), Ok(1));
    }

    // ── New tests: attribute-matching robustness ──────────────────────────────

    #[test]
    fn count_matches_cfg_test_not_first_attribute() {
        // cfg(test) need not be the first attribute in the run; the whole run
        // (from #[allow(dead_code)]) is excluded with the block.
        let src = "fn a() {}\n#[allow(dead_code)]\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        // mod tests spans lines 2-6 (5 lines excluded from 6 total) → count = 1.
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn count_matches_cfg_test_after_doc_comment() {
        // A doc comment lowers to #[doc = "..."] in the token stream and is treated
        // as a leading attribute; the whole run (doc + cfg(test)) is excluded.
        let src = "fn a() {}\n/// Tests module.\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        // mod tests spans lines 2-6 (5 lines excluded from 6 total) → count = 1.
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn count_does_not_match_cfg_attr_wrapped() {
        // cfg_attr(…) is not matched (documented out of scope); the mod counts as
        // production, so the whole file is counted.
        let src =
            "fn a() {}\n#[cfg_attr(feature = \"x\", cfg(test))]\nmod tests {\n    fn t() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(5));
    }

    #[test]
    fn count_takes_last_of_multiple_mod_tests() {
        // Two #[cfg(test)] mod tests blocks; only the last is the boundary.
        // fn a + #[cfg(test)] + mod tests { + fn t1 + } + fn b = 6 production lines.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t1() {}\n}\nfn b() {}\n\
                   #[cfg(test)]\nmod tests {\n    fn t2() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(6));
    }

    // ── New tests: boundary-isolation + fail-closed ───────────────────────────

    #[test]
    fn count_errors_on_code_sharing_attribute_line() {
        // A production token on the same physical line as the #[cfg(test)] attribute
        // returns Err (boundary-isolation fail-closed).
        let src = "fn a() {} #[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        assert!(code_line_count(src).is_err());
    }

    #[test]
    fn count_errors_on_code_sharing_close_brace_line() {
        // A production token on the same physical line as the closing `}` returns Err.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n} fn after() {}\n";
        assert!(code_line_count(src).is_err());
    }

    #[test]
    fn count_errors_on_unlexable_input() {
        // An unterminated string literal (or any lexically-invalid input) returns Err
        // (fail-closed: we cannot count what we cannot lex).
        let src = "fn a() {}\nlet x = \"unterminated";
        assert!(code_line_count(src).is_err());
    }

    // ── New tests: span-location characterization ─────────────────────────────

    #[test]
    fn spans_report_real_lines_for_multiline_module() {
        // Characterizes that proc-macro2 fallback-mode spans return real line numbers
        // (requires the `span-locations` feature; without it, spans would return
        // line = 0 and the exclusion would be computed incorrectly).
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t1() {}\n    fn t2() {}\n\
                   fn t3() {}\n}\n";
        // 7 total lines; mod tests spans lines 2-7 (6 excluded) → count = 1.
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn count_correct_without_trailing_newline() {
        // A file whose last line has no trailing '\n' still counts correctly.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}";
        // 5 total lines (no trailing newline); mod tests spans lines 2-5 → count = 1.
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn count_correct_with_raw_string_spanning_lines() {
        // A multiline raw string inside the module does not shift the detected end_line.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    let s = r#\"\nline2\nline3\n\"#;\n}\n";
        // 8 total lines; mod tests spans lines 2-8 (7 excluded) → count = 1.
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn parse_baseline_ignores_comments_and_blanks() {
        let s = "# header\n\nsrc/cli.rs 2301\nsrc/config.rs 1212\n";
        let m = parse_baseline(s);
        assert_eq!(m.get("src/cli.rs"), Some(&2301));
        assert_eq!(m.get("src/config.rs"), Some(&1212));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn growth_flags_new_file_over_limit() {
        let current = map(&[("src/new.rs", 850)]);
        let v = growth_and_tightness_violations(&current, &BTreeMap::new());
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("src/new.rs"));
    }

    #[test]
    fn growth_allows_baselined_file_at_baseline() {
        let current = map(&[("src/cli.rs", 2301)]);
        let baseline = map(&[("src/cli.rs", 2301)]);
        assert!(growth_and_tightness_violations(&current, &baseline).is_empty());
    }

    #[test]
    fn growth_flags_baselined_file_that_grew() {
        let current = map(&[("src/cli.rs", 2302)]);
        let baseline = map(&[("src/cli.rs", 2301)]);
        let v = growth_and_tightness_violations(&current, &baseline);
        assert!(v.iter().any(|m| m.contains("exceed")), "{v:?}");
    }

    #[test]
    fn tightness_flags_shrunk_file_needing_lower_baseline() {
        let current = map(&[("src/cli.rs", 2000)]);
        let baseline = map(&[("src/cli.rs", 2301)]);
        let v = growth_and_tightness_violations(&current, &baseline);
        assert!(v.iter().any(|m| m.contains("lower its baseline")), "{v:?}");
    }

    #[test]
    fn tightness_flags_baseline_at_or_below_limit() {
        let current = map(&[("src/x.rs", 700)]);
        let baseline = map(&[("src/x.rs", 800)]);
        let v = growth_and_tightness_violations(&current, &baseline);
        assert!(v.iter().any(|m| m.contains("remove this entry")), "{v:?}");
    }

    #[test]
    fn tightness_flags_baseline_for_missing_file() {
        let baseline = map(&[("src/gone.rs", 1500)]);
        let v = growth_and_tightness_violations(&BTreeMap::new(), &baseline);
        assert!(v.iter().any(|m| m.contains("no longer exists")), "{v:?}");
    }

    #[test]
    fn immutability_flags_added_entry() {
        let current = map(&[("src/new.rs", 1200)]);
        let v = immutability_violations(&current, &BTreeMap::new());
        assert!(
            v.iter().any(|m| m.contains("added vs origin/main")),
            "{v:?}"
        );
    }

    #[test]
    fn immutability_flags_raised_entry() {
        let current = map(&[("src/cli.rs", 2400)]);
        let base = map(&[("src/cli.rs", 2301)]);
        let v = immutability_violations(&current, &base);
        assert!(v.iter().any(|m| m.contains("raised")), "{v:?}");
    }

    #[test]
    fn immutability_allows_lowered_or_removed_entries() {
        let current = map(&[("src/cli.rs", 2000)]);
        let base = map(&[("src/cli.rs", 2301), ("src/config.rs", 1212)]);
        assert!(immutability_violations(&current, &base).is_empty());
    }
}
