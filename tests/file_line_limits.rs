//! Per-file code-line gate (ADR-0018). Every `src/**/*.rs` file's *code* line
//! count (the production lines, excluding the last `#[cfg(test)] mod tests`
//! block) must stay within `max(800, baseline[file])`, where `baseline` is the
//! checked-in `tests/file-line-baseline.txt`. The baseline is a frozen ratchet:
//! it may only shrink or lose entries, never grow — enforced against
//! `origin/main` so the limit cannot be raised in the same PR that grows a file.
//!
//! The counting and the three invariants are pure functions, unit-tested below;
//! the enforcement test wires them to the filesystem and git.

use std::collections::BTreeMap;
use std::fs;
use std::io;
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
    total.checked_sub(excluded).ok_or_else(|| {
        format!(
            "internal: excluded span {excluded} exceeds total lines {total} \
             (span-locations regression?)"
        )
    })
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
/// A line that is neither blank/comment nor parses as `path count` is returned
/// as a violation message instead of being dropped: `immutability_violations`
/// and `growth_and_tightness_violations` only detect entries that were *added*
/// or *raised* relative to what they're given, so a corrupted line that simply
/// vanishes from the parsed map is indistinguishable from a legitimately
/// shrunk-and-removed entry — it must not be able to hide a raised count.
///
/// `source` labels the bad-line messages (e.g. `BASELINE_REL` for the
/// working-tree copy vs. `origin/main:<BASELINE_REL>` for the base-branch
/// copy) so the two are distinguishable when both are parsed in the same run.
///
/// A second line for a path already seen earlier in `s` is also a violation
/// (not a silent overwrite): `BTreeMap::insert` would let the later line win
/// with no diagnostic, so a duplicate could smuggle a raised count past
/// invariant 2/3 the same way a corrupted line could. The first occurrence
/// wins in the returned map; later ones are reported and dropped.
fn parse_baseline(s: &str, source: &str) -> (BTreeMap<String, usize>, Vec<String>) {
    let mut m = BTreeMap::new();
    let mut bad = Vec::new();
    for (i, raw_line) in s.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = line
            .rsplit_once(char::is_whitespace)
            .and_then(|(path, count)| {
                count
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .map(|n| (path.trim().to_string(), n))
            });
        match parsed {
            Some((path, n)) => {
                if let Some(&existing) = m.get(&path) {
                    bad.push(format!(
                        "{source}:{}: duplicate baseline entry for `{path}` (already {existing}, \
                         this line has {n})",
                        i + 1
                    ));
                } else {
                    m.insert(path, n);
                }
            }
            None => bad.push(format!(
                "{source}:{}: cannot parse as `path count`: {raw_line:?}",
                i + 1
            )),
        }
    }
    (m, bad)
}

// ── Invariants ───────────────────────────────────────────────────────────────

/// Invariant 1 (no growth) + invariant 2 (tight baselines): every file stays
/// within `max(LIMIT, baseline)`, and every baseline entry is `> LIMIT` and
/// equals its file's current count (stale/oversized headroom is rejected).
///
/// `scan_complete` distinguishes "this baselined file is genuinely gone" from
/// "the file walk didn't finish, so we don't actually know": when the
/// filesystem walk that produced `current` failed partway (see
/// `collect_rs_files`), a baselined file inside the unscanned subtree is
/// simply absent from `current` too, and would otherwise be misreported as
/// "no longer exists" even though it's still there. The walk failure itself
/// is already a separate violation, so this only suppresses a misleading
/// message — it does not weaken fail-closed behavior.
fn growth_and_tightness_violations(
    current: &BTreeMap<String, usize>,
    baseline: &BTreeMap<String, usize>,
    scan_complete: bool,
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
            None if scan_complete => v.push(format!(
                "{f}: baseline entry for a file that no longer exists; remove it"
            )),
            None => {}
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

/// Recursively collect `.rs` file paths under `dir` into `out`. Propagates
/// I/O errors (an unreadable subtree, a directory entry that vanishes mid-scan)
/// instead of skipping them: a baseline-less (typically < 800 line) file that a
/// silently-skipped subtree hides would pass the gate with no diagnostic at all.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            collect_rs_files(&p, out)?;
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
    Ok(())
}

/// State of the base branch's baseline, used to drive invariant 3.
#[derive(Debug)]
enum BaseBaseline {
    /// `origin/main` has the baseline file — enforce invariant 3 against it.
    Content(String),
    /// `origin/main` resolves and positively confirmed (via `git ls-tree`) to
    /// carry no baseline file yet. This is the bootstrap window: only the
    /// introducing PR (and the commits before this gate first lands on `main`)
    /// hit it. Invariant 3 arms automatically once the baseline exists on
    /// `main`.
    BootstrapAbsent,
    /// `git show` failed for the baseline path, but its absence from
    /// `origin/main` could not be positively confirmed (the `git ls-tree`
    /// double-check also failed, or unexpectedly found the file present). This
    /// covers transient failures — object DB hiccups, stale fetches — that must
    /// not be silently treated as the bootstrap window.
    AbsenceUnconfirmed,
    /// `origin/main` could not be resolved at all (shallow/local checkout, or a
    /// CI job that failed to fetch it). Invariant 3 cannot be checked.
    RefUnavailable,
}

/// Classify a failed `git show origin/main:<baseline>` using a probe of
/// whether the path is actually absent from `origin/main`, expressed as data
/// (`Some(true)` = `git ls-tree` succeeded with empty output, i.e. confirmed
/// absent; `Some(false)` = it succeeded but reported the path present;
/// `None` = the probe command itself failed to answer) so the bootstrap vs.
/// indeterminate-failure decision is unit-testable without invoking git.
fn classify_show_failure(ls_tree_confirmed_absent: Option<bool>) -> BaseBaseline {
    match ls_tree_confirmed_absent {
        Some(true) => BaseBaseline::BootstrapAbsent,
        Some(false) | None => BaseBaseline::AbsenceUnconfirmed,
    }
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
    match Command::new("git")
        .args(["show", &format!("origin/main:{BASELINE_REL}")])
        .current_dir(root)
        .output()
    {
        Ok(o) if o.status.success() => {
            BaseBaseline::Content(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        // `git show` failed. Rather than assuming that means the file is simply
        // not in that tree yet (bootstrap), positively confirm the absence with
        // `git ls-tree` before drawing that conclusion.
        _ => {
            let ls_tree_confirmed_absent = Command::new("git")
                .args(["ls-tree", "origin/main", "--", BASELINE_REL])
                .current_dir(root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| o.stdout.iter().all(u8::is_ascii_whitespace));
            classify_show_failure(ls_tree_confirmed_absent)
        }
    }
}

/// Whether invariant 3 must run fail-closed: the dedicated `file-lines` CI job
/// sets this, having fetched `origin/main`. There, an unresolvable base is a
/// setup failure and errors rather than silently skipping. Other contexts (the
/// general `test` job, local `cargo test`) leave invariant 3 to that job.
fn invariant3_is_strict() -> bool {
    std::env::var_os("TSM_FILE_LINE_GATE_STRICT").is_some()
}

/// The two ways invariant 3 can end up without a usable base baseline —
/// `origin/main` doesn't resolve at all, or it resolves but the baseline
/// file's absence couldn't be positively confirmed. Both are fail-closed in
/// the strict `file-lines` job and an informational skip otherwise.
enum UnresolvedBase {
    RefUnavailable,
    AbsenceUnconfirmed,
}

/// Decide invariant 3's outcome for an unresolved base state: `Err` is a gate
/// violation (fail-closed, pushed into `violations`); `Ok` means skip (the
/// caller logs an explanatory `eprintln!`). Pure so the strict × state matrix
/// is unit-testable without invoking git.
fn unresolved_base_gate_result(state: UnresolvedBase, strict: bool) -> Result<(), String> {
    if !strict {
        return Ok(());
    }
    Err(match state {
        UnresolvedBase::RefUnavailable => {
            "invariant 3 (immutability) could not run: origin/main is unavailable in the \
             strict file-lines job — it must fetch the base branch before running"
                .to_string()
        }
        UnresolvedBase::AbsenceUnconfirmed => {
            "invariant 3 (immutability) could not run: origin/main resolved, but the baseline \
             file's absence could not be positively confirmed (git show and git ls-tree both \
             failed to establish it) — the strict file-lines job must not fail open on a \
             transient git failure"
                .to_string()
        }
    })
}

/// Compute a `(crate-relative path, code-line count)` pair for one file, or a
/// violation message naming the file. Used in place of `unwrap()` so that one
/// unreadable file (or a path outside `root`, which should not occur but is
/// handled rather than assumed) fails closed as a named violation instead of
/// panicking the whole gate and losing every other file's diagnostics.
fn line_count_for_file(root: &Path, f: &Path) -> Result<(String, usize), String> {
    let rel = f
        .strip_prefix(root)
        .map_err(|e| format!("{}: not under crate root: {e}", f.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let content = fs::read_to_string(f).map_err(|e| format!("{rel}: failed to read file: {e}"))?;
    code_line_count(&content)
        .map(|c| (rel.clone(), c))
        .map_err(|e| format!("{rel}: {e}"))
}

/// Parse the base branch's baseline content and compute invariant 3's
/// contribution to the violations list against it: malformed/duplicate lines
/// (labeled with the `origin/main:<BASELINE_REL>` source, distinct from the
/// working-tree copy) plus any entry added or raised relative to
/// `current_baseline`.
fn content_base_violations(
    current_baseline: &BTreeMap<String, usize>,
    base_content: &str,
) -> Vec<String> {
    let (base_baseline, mut v) =
        parse_baseline(base_content, &format!("origin/main:{BASELINE_REL}"));
    v.extend(immutability_violations(current_baseline, &base_baseline));
    v
}

/// Compute invariant 3's contribution to the violations list for an already
/// resolved `BaseBaseline` state. `BaseBaseline` variants carry no I/O
/// handles — they're plain data — so every state (including `Content` with
/// arbitrary, possibly corrupted text) is directly constructible in tests,
/// making the full state × strict-mode dispatch unit-testable without
/// invoking git.
fn base_baseline_violations(
    state: BaseBaseline,
    current_baseline: &BTreeMap<String, usize>,
    strict: bool,
) -> Vec<String> {
    match state {
        BaseBaseline::Content(base) => content_base_violations(current_baseline, &base),
        BaseBaseline::BootstrapAbsent => {
            eprintln!(
                "[file-lines] no baseline on origin/main yet (bootstrap); invariant 3 \
                 (immutability) arms once this lands on main"
            );
            Vec::new()
        }
        BaseBaseline::AbsenceUnconfirmed => {
            match unresolved_base_gate_result(UnresolvedBase::AbsenceUnconfirmed, strict) {
                Err(msg) => vec![msg],
                Ok(()) => {
                    eprintln!(
                        "[file-lines] could not positively confirm the baseline file is absent \
                         from origin/main (git show and git ls-tree both failed); invariant 3 \
                         (immutability) is enforced by the strict file-lines CI job"
                    );
                    Vec::new()
                }
            }
        }
        BaseBaseline::RefUnavailable => {
            match unresolved_base_gate_result(UnresolvedBase::RefUnavailable, strict) {
                Err(msg) => vec![msg],
                Ok(()) => {
                    eprintln!(
                        "[file-lines] origin/main unavailable (local/shallow checkout); \
                         invariant 3 (immutability) is enforced by the strict file-lines CI job"
                    );
                    Vec::new()
                }
            }
        }
    }
}

#[test]
fn enforce_per_file_line_limits() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut violations: Vec<String> = Vec::new();

    let mut files = Vec::new();
    let scan_complete = match collect_rs_files(&root.join("src"), &mut files) {
        Ok(()) => true,
        Err(e) => {
            violations.push(format!("failed to walk src/ for the line-count gate: {e}"));
            false
        }
    };
    files.sort();

    let mut current: BTreeMap<String, usize> = BTreeMap::new();
    for f in &files {
        match line_count_for_file(root, f) {
            Ok((rel, c)) => {
                current.insert(rel, c);
            }
            Err(e) => violations.push(e),
        }
    }

    let (baseline, bad_baseline_lines) =
        parse_baseline(include_str!("file-line-baseline.txt"), BASELINE_REL);
    violations.extend(bad_baseline_lines);
    violations.extend(growth_and_tightness_violations(
        &current,
        &baseline,
        scan_complete,
    ));

    // Invariant 3 (immutability) needs the base branch. It is authoritative only
    // in the strict `file-lines` job; elsewhere it is skipped (that job covers
    // it). In strict mode an unresolvable base is fail-closed, not fail-open.
    let strict = invariant3_is_strict();
    violations.extend(base_baseline_violations(
        base_baseline(root),
        &baseline,
        strict,
    ));

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
        // Production = the 3 lines before `#[cfg(test)]`.
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
    fn count_allows_pub_crate_visibility_on_test_module() {
        // pub(crate) exercises the Group(Parenthesis) walk-back branch that bare `pub` does not.
        let src = "fn a() {}\n#[cfg(test)]\npub(crate) mod tests {\n    fn t() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(1));
    }

    #[test]
    fn count_zero_when_file_is_only_test_module() {
        // No production code before the module: run_start == 0 (skips the prev-token
        // boundary check) and the count floors at 0.
        let src = "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        assert_eq!(code_line_count(src), Ok(0));
    }

    #[test]
    fn count_whole_file_when_test_module_is_nested() {
        // A nested `mod tests` is invisible to the top-level scan (documented
        // out-of-scope, fail-noisy): the whole file counts as production.
        let src = "fn a() {}\nmod outer {\n    #[cfg(test)]\n    mod tests {\n        fn t() {}\n    }\n}\n";
        assert_eq!(code_line_count(src), Ok(7));
    }

    #[test]
    fn parse_baseline_ignores_comments_and_blanks() {
        let s = "# header\n\nsrc/cli.rs 2301\nsrc/config.rs 1212\n";
        let (m, bad) = parse_baseline(s, BASELINE_REL);
        assert_eq!(m.get("src/cli.rs"), Some(&2301));
        assert_eq!(m.get("src/config.rs"), Some(&1212));
        assert_eq!(m.len(), 2);
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn parse_baseline_flags_line_with_non_numeric_count() {
        let s = "src/cli.rs 2301\nsrc/broken.rs notanumber\n";
        let (m, bad) = parse_baseline(s, BASELINE_REL);
        assert_eq!(m.len(), 1, "the malformed line must not silently vanish");
        assert_eq!(bad.len(), 1);
        assert!(bad[0].contains("src/broken.rs notanumber"), "{bad:?}");
    }

    #[test]
    fn parse_baseline_flags_line_without_whitespace_separator() {
        let s = "src/cli.rs 2301\nsrc/broken.rs-no-space-2000\n";
        let (m, bad) = parse_baseline(s, BASELINE_REL);
        assert_eq!(m.len(), 1);
        assert_eq!(bad.len(), 1);
    }

    #[test]
    fn parse_baseline_bad_line_message_is_labeled_with_source() {
        let s = "notanumber\n";
        let (_, bad) = parse_baseline(s, BASELINE_REL);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].starts_with(&format!("{BASELINE_REL}:1:")), "{bad:?}");
    }

    #[test]
    fn parse_baseline_disambiguates_working_tree_vs_base_branch_source() {
        // The gate parses the same corrupted content from two sources — the
        // working-tree baseline and origin/main's copy — in the same run. Their
        // bad-line messages must be distinguishable, not just both say
        // "tests/file-line-baseline.txt:1: ...".
        let s = "notanumber\n";
        let (_, working_tree_bad) = parse_baseline(s, BASELINE_REL);
        let (_, base_branch_bad) = parse_baseline(s, &format!("origin/main:{BASELINE_REL}"));
        assert_ne!(working_tree_bad[0], base_branch_bad[0]);
        assert!(
            base_branch_bad[0].starts_with("origin/main:"),
            "{base_branch_bad:?}"
        );
    }

    #[test]
    fn parse_baseline_flags_duplicate_path() {
        // Two lines for the same path: the first must not silently vanish under
        // the second (`BTreeMap::insert` overwrite would drop it without a trace).
        let s = "src/cli.rs 2301\nsrc/cli.rs 2400\n";
        let (m, bad) = parse_baseline(s, BASELINE_REL);
        assert_eq!(
            m.get("src/cli.rs"),
            Some(&2301),
            "first entry must win, not the silently-overwriting duplicate"
        );
        assert_eq!(bad.len(), 1);
        assert!(bad[0].contains("src/cli.rs"), "{bad:?}");
        assert!(bad[0].contains("duplicate"), "{bad:?}");
    }

    #[test]
    fn parse_baseline_empty_content_yields_no_entries_or_violations() {
        let (m, bad) = parse_baseline("", BASELINE_REL);
        assert!(m.is_empty());
        assert!(bad.is_empty());
    }

    #[test]
    fn growth_flags_new_file_over_limit() {
        let current = map(&[("src/new.rs", 850)]);
        let v = growth_and_tightness_violations(&current, &BTreeMap::new(), true);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("src/new.rs"));
    }

    #[test]
    fn growth_allows_baselined_file_at_baseline() {
        let current = map(&[("src/cli.rs", 2301)]);
        let baseline = map(&[("src/cli.rs", 2301)]);
        assert!(growth_and_tightness_violations(&current, &baseline, true).is_empty());
    }

    #[test]
    fn growth_flags_baselined_file_that_grew() {
        let current = map(&[("src/cli.rs", 2302)]);
        let baseline = map(&[("src/cli.rs", 2301)]);
        let v = growth_and_tightness_violations(&current, &baseline, true);
        assert!(v.iter().any(|m| m.contains("exceed")), "{v:?}");
    }

    #[test]
    fn tightness_flags_shrunk_file_needing_lower_baseline() {
        let current = map(&[("src/cli.rs", 2000)]);
        let baseline = map(&[("src/cli.rs", 2301)]);
        let v = growth_and_tightness_violations(&current, &baseline, true);
        assert!(v.iter().any(|m| m.contains("lower its baseline")), "{v:?}");
    }

    #[test]
    fn tightness_flags_baseline_at_or_below_limit() {
        let current = map(&[("src/x.rs", 700)]);
        let baseline = map(&[("src/x.rs", 800)]);
        let v = growth_and_tightness_violations(&current, &baseline, true);
        assert!(v.iter().any(|m| m.contains("remove this entry")), "{v:?}");
    }

    #[test]
    fn tightness_flags_baseline_for_missing_file() {
        let baseline = map(&[("src/gone.rs", 1500)]);
        let v = growth_and_tightness_violations(&BTreeMap::new(), &baseline, true);
        assert!(v.iter().any(|m| m.contains("no longer exists")), "{v:?}");
    }

    #[test]
    fn tightness_suppresses_missing_file_violation_when_scan_incomplete() {
        // A baselined file whose subtree failed to scan (collect_rs_files
        // returned Err) is indistinguishable from a genuinely deleted file by
        // `current` alone. When the scan didn't complete, don't claim removal —
        // the walk-failure violation (pushed separately by the caller) already
        // fails the gate.
        let baseline = map(&[("src/gone.rs", 1500)]);
        let v = growth_and_tightness_violations(&BTreeMap::new(), &baseline, false);
        assert!(v.iter().all(|m| !m.contains("no longer exists")), "{v:?}");
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

    // ── collect_rs_files: read-failure propagation ────────────────────────────

    #[test]
    fn collect_rs_files_propagates_read_dir_failure() {
        let mut out = Vec::new();
        let result = collect_rs_files(Path::new("/nonexistent/does-not-exist-xyz"), &mut out);
        assert!(result.is_err());
        assert!(out.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn collect_rs_files_propagates_mid_walk_unreadable_subdirectory() {
        // A root that itself reads fine, but whose subdirectory doesn't — the
        // failure must surface even though it isn't the top-level read_dir call.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let sub = dir.path().join("locked");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("b.rs"), "fn b() {}\n").unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();

        // Running as root (common in some container images) ignores permission
        // bits, making this case impossible to trigger; skip rather than flake.
        if fs::read_dir(&sub).is_ok() {
            fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: running as a user that bypasses directory permissions");
            return;
        }

        let mut out = Vec::new();
        let result = collect_rs_files(dir.path(), &mut out);

        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn collect_rs_files_collects_rs_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.txt"), "not rust\n").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("c.rs"), "fn c() {}\n").unwrap();

        let mut out = Vec::new();
        collect_rs_files(dir.path(), &mut out).unwrap();
        out.sort();

        assert_eq!(out, vec![dir.path().join("a.rs"), sub.join("c.rs")]);
    }

    // ── base_baseline: bootstrap vs indeterminate classification ──────────────

    #[test]
    fn classify_show_failure_confirmed_absent_is_bootstrap() {
        assert!(matches!(
            classify_show_failure(Some(true)),
            BaseBaseline::BootstrapAbsent
        ));
    }

    #[test]
    fn classify_show_failure_ls_tree_command_failed_is_unconfirmed() {
        // `git ls-tree` itself could not answer the question (e.g. object DB
        // hiccup, stale fetch) — absence was never positively confirmed.
        assert!(matches!(
            classify_show_failure(None),
            BaseBaseline::AbsenceUnconfirmed
        ));
    }

    #[test]
    fn classify_show_failure_ls_tree_shows_content_is_unconfirmed() {
        // `git ls-tree` succeeded but reports the file exists — contradicts the
        // earlier `git show` failure, so treat as unconfirmed rather than as a
        // confident "the file is absent" bootstrap window.
        assert!(matches!(
            classify_show_failure(Some(false)),
            BaseBaseline::AbsenceUnconfirmed
        ));
    }

    // ── unresolved_base_gate_result: strict fail-closed decision ──────────────

    #[test]
    fn unresolved_base_strict_ref_unavailable_is_violation() {
        assert!(unresolved_base_gate_result(UnresolvedBase::RefUnavailable, true).is_err());
    }

    #[test]
    fn unresolved_base_strict_absence_unconfirmed_is_violation() {
        assert!(unresolved_base_gate_result(UnresolvedBase::AbsenceUnconfirmed, true).is_err());
    }

    #[test]
    fn unresolved_base_non_strict_is_skip() {
        assert!(unresolved_base_gate_result(UnresolvedBase::RefUnavailable, false).is_ok());
        assert!(unresolved_base_gate_result(UnresolvedBase::AbsenceUnconfirmed, false).is_ok());
    }

    // ── content_base_violations: corrupted origin/main-side baseline ──────────

    #[test]
    fn content_base_violations_surfaces_corrupted_base_line_with_prefix() {
        let current = map(&[("src/cli.rs", 2301)]);
        let v = content_base_violations(&current, "src/cli.rs notanumber\n");
        assert!(
            v.iter()
                .any(|m| m.starts_with(&format!("origin/main:{BASELINE_REL}:1:"))),
            "{v:?}"
        );
    }

    #[test]
    fn content_base_violations_still_checks_immutability_alongside_bad_lines() {
        // A corrupted line and a raised entry in the same base content must
        // both surface — the bad-line detour must not short-circuit invariant 3.
        let current = map(&[("src/cli.rs", 2500)]);
        let v = content_base_violations(&current, "src/cli.rs 2301\nbroken line\n");
        assert!(v.iter().any(|m| m.contains("raised")), "{v:?}");
        assert!(v.iter().any(|m| m.starts_with("origin/main:")), "{v:?}");
    }

    // ── base_baseline_violations: full state × strict dispatch, no git needed ─
    //
    // `BaseBaseline` variants are plain data, so every state (including
    // `Content` with corrupted text) is constructible directly — these tests
    // exercise the whole invariant-3 dispatch end to end without touching git.

    #[test]
    fn base_baseline_violations_strict_absence_unconfirmed_fails_closed() {
        let v = base_baseline_violations(BaseBaseline::AbsenceUnconfirmed, &BTreeMap::new(), true);
        assert_eq!(v.len(), 1, "{v:?}");
    }

    #[test]
    fn base_baseline_violations_non_strict_absence_unconfirmed_skips() {
        let v = base_baseline_violations(BaseBaseline::AbsenceUnconfirmed, &BTreeMap::new(), false);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn base_baseline_violations_strict_ref_unavailable_fails_closed() {
        let v = base_baseline_violations(BaseBaseline::RefUnavailable, &BTreeMap::new(), true);
        assert_eq!(v.len(), 1, "{v:?}");
    }

    #[test]
    fn base_baseline_violations_bootstrap_absent_is_always_empty() {
        let v = base_baseline_violations(BaseBaseline::BootstrapAbsent, &BTreeMap::new(), true);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn base_baseline_violations_content_surfaces_corrupted_base_line() {
        let current = map(&[("src/cli.rs", 2301)]);
        let v = base_baseline_violations(
            BaseBaseline::Content("src/cli.rs notanumber\n".to_string()),
            &current,
            true,
        );
        assert!(v.iter().any(|m| m.starts_with("origin/main:")), "{v:?}");
    }

    // ── Multi-violation aggregation ────────────────────────────────────────────

    #[test]
    fn violations_aggregate_from_multiple_sources_simultaneously() {
        // Mirrors enforce_per_file_line_limits' aggregation pipeline: a
        // malformed baseline line, a growth violation, and (via
        // base_baseline_violations) invariant 3 must all land in the same
        // violations list rather than the first problem masking the rest.
        let current = map(&[("src/new.rs", 900), ("src/cli.rs", 2500)]);
        let (baseline, mut violations) =
            parse_baseline("src/cli.rs 2301\nbroken line\n", BASELINE_REL);
        violations.extend(growth_and_tightness_violations(&current, &baseline, true));
        violations.extend(base_baseline_violations(
            BaseBaseline::Content("src/cli.rs 2301\n".to_string()),
            &baseline,
            true,
        ));

        assert!(violations.len() >= 3, "{violations:?}");
        assert!(
            violations.iter().any(|m| m.contains("broken line")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|m| m.contains("src/new.rs")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|m| m.contains("src/cli.rs")),
            "{violations:?}"
        );
    }

    // ── line_count_for_file: read-failure violation aggregation ───────────────

    #[test]
    fn line_count_for_file_reports_missing_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let missing = root.join("src/does-not-exist-xyz.rs");
        assert!(line_count_for_file(root, &missing).is_err());
    }

    #[test]
    fn line_count_for_file_reports_path_outside_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let outside = Path::new("/completely/unrelated/path.rs");
        assert!(line_count_for_file(root, outside).is_err());
    }

    #[test]
    fn line_count_for_file_succeeds_for_real_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (rel, count) = line_count_for_file(root, &root.join("src/lib.rs")).unwrap();
        assert_eq!(rel, "src/lib.rs");
        assert!(count > 0);
    }
}
