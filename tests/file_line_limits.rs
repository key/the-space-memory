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

/// Flat code-line cap for any file without a (larger) frozen baseline.
const LIMIT: usize = 800;

/// Path of the checked-in baseline, relative to the crate root. Used both for
/// `include_str!` (current tree) and `git show` (base branch).
const BASELINE_REL: &str = "tests/file-line-baseline.txt";

// ── Counting (with fail-closed test-module validation) ───────────────────────

/// Count production code lines in a Rust source file: every physical line up to
/// the trailing inline `#[cfg(test)] mod tests { … }` block. Blank lines and
/// comments in production code are counted; only that trailing unit-test module
/// is excluded.
///
/// The boundary is the **last** top-level `#[cfg(test)]` whose next non-blank
/// line opens `mod tests`. Test-only support items that legitimately appear in
/// the production region — `#[cfg(test)] pub fn helper()`, `#[cfg(test)] pub mod
/// test_utils;` — stay counted as code; only the trailing test module is
/// stripped. A file with no such trailing module counts in full.
///
/// Fails closed against a silent undercount: if a trailing test module is found
/// but the file does not end with its closing `}`, code has leaked past it, so
/// we error rather than strip too much. The module *interior* is never scanned —
/// raw-string fixtures legitimately contain column-0 text — so the EOF brace is
/// the boundary guard.
fn code_line_count(content: &str) -> Result<usize, String> {
    let lines: Vec<&str> = content.lines().collect();

    // Boundary = the last column-0 `#[cfg(test)]` immediately followed by a
    // `mod tests` opener.
    let mut boundary: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("#[cfg(test)]")
            && lines[i + 1..]
                .iter()
                .find(|x| !x.trim().is_empty())
                .is_some_and(|x| opens_test_module(x))
        {
            boundary = Some(i);
        }
    }

    let Some(i) = boundary else {
        return Ok(lines.len()); // No inline test module — whole file is code.
    };

    let last = lines.iter().rev().find(|l| !l.trim().is_empty());
    if last.map(|l| l.trim_end()) != Some("}") {
        return Err(format!(
            "trailing `#[cfg(test)] mod tests` (line {}) is not the file's last item \
             (last non-blank line is not a column-0 `}}`) — code appears after it",
            i + 1
        ));
    }
    Ok(i) // Lines [0, i) are production code.
}

/// Whether `line` opens a `mod tests` block (optionally `pub` / `pub(crate)`),
/// not merely a name starting with `tests` (e.g. `mod tests_helpers`).
fn opens_test_module(line: &str) -> bool {
    let t = line.trim_start();
    let t = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub "))
        .unwrap_or(t);
    match t.strip_prefix("mod tests") {
        Some(rest) => rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == '{'),
        None => false,
    }
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

/// The base branch's baseline file content, or `None` when it cannot be read
/// (the introducing PR — no file on `origin/main` yet — or a local/shallow
/// checkout without the ref). Invariant 3 is skipped in that case.
fn base_baseline(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["show", &format!("origin/main:{BASELINE_REL}")])
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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

    match base_baseline(root) {
        Some(base) => {
            violations.extend(immutability_violations(&baseline, &parse_baseline(&base)));
        }
        None => eprintln!(
            "[file-lines] origin/main baseline unavailable (bootstrap or local/shallow \
             checkout); skipping invariant 3 (immutability)"
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
    fn count_rejects_production_code_after_test_module() {
        // Code must not hide past the trailing test module: the file must end at
        // the module's closing `}`.
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn hidden() {}\n";
        assert!(code_line_count(src).is_err());
    }

    #[test]
    fn count_rejects_content_after_test_module() {
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n// trailing\n";
        assert!(code_line_count(src).is_err());
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
