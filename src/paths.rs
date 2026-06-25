//! Shared path resolution: lexical absolutization (no symlink resolution),
//! `--path` filter normalization, and directory-boundary matching.
//! All functions are pure (no filesystem syscalls) so they unit-test cleanly
//! and behave identically across CLI and daemon. See ADR-0017.
//!
//! Unix-only: `/` is the sole path separator (tsm uses UNIX domain sockets and
//! does not run on Windows). Tests use Unix roots by design.

use std::path::{Component, Path, PathBuf};

/// Lexically absolutize `input` against `base`, folding `.`/`..` without
/// touching the filesystem (symlinks are NOT resolved — ADR-0017 option A).
/// `std::path::absolute` deliberately does not fold `..`; we do, accepting the
/// documented symlink caveat.
pub fn absolutize(input: &Path, base: &Path) -> PathBuf {
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        base.join(input)
    };
    let mut out: Vec<Component> = Vec::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a normal segment; keep `..` only if there is nothing to
                // pop and we are not already at a root/prefix.
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !matches!(out.last(), Some(Component::RootDir | Component::Prefix(_))) {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Normalize a `--path` argument to a CWD-anchored absolute path.
/// Empty string is the only error; everything else resolves.
pub fn normalize_filter_path(arg: &str, cwd: &Path) -> anyhow::Result<PathBuf> {
    if arg.is_empty() {
        anyhow::bail!("--path cannot be empty");
    }
    Ok(absolutize(Path::new(arg), cwd))
}

/// True if `candidate` equals `dir` or sits under it at a component boundary.
pub fn is_within(candidate: &Path, dir: &Path) -> bool {
    candidate == dir || candidate.starts_with(dir)
}

/// Build the SQL operands for a directory-boundary match against
/// `documents.file_path`: `(eq_value, like_pattern)`. Caller uses
/// `d.file_path = ?eq COLLATE NOCASE OR d.file_path LIKE ?like ESCAPE '\'`.
pub fn boundary_like(dir: &Path) -> (String, String) {
    let s = dir.to_string_lossy();
    let escaped = s
        .replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_");
    (s.to_string(), format!("{escaped}/%"))
}

/// Build a path-scope SQL fragment + bind params over an aliased `d.file_path`
/// (callers must alias the `documents` table as `d`). Returns `("", [])` when
/// there is no filter. Each `--path` prefix becomes a case-insensitive
/// directory-boundary clause; multiple prefixes are OR-joined (ADR-0017). The
/// fragment is appended to an existing WHERE, e.g. `... WHERE 1=1{frag}`.
pub fn scope_clause(path_prefixes: Option<&[String]>) -> (String, Vec<String>) {
    match path_prefixes {
        Some(prefixes) if !prefixes.is_empty() => {
            let mut conds = Vec::new();
            let mut params = Vec::new();
            for p in prefixes {
                let (eq, like) = boundary_like(Path::new(p));
                conds.push("(d.file_path = ? COLLATE NOCASE OR d.file_path LIKE ? ESCAPE '\\')");
                params.push(eq);
                params.push(like);
            }
            (format!(" AND ({})", conds.join(" OR ")), params)
        }
        _ => (String::new(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn absolutize_relative_joins_base() {
        assert_eq!(
            absolutize(Path::new("daily"), Path::new("/root")),
            PathBuf::from("/root/daily")
        );
    }
    #[test]
    fn absolutize_folds_dotdot() {
        assert_eq!(
            absolutize(Path::new("../sib/x"), Path::new("/root/a")),
            PathBuf::from("/root/sib/x")
        );
    }
    #[test]
    fn absolutize_folds_dot_and_trailing() {
        assert_eq!(
            absolutize(Path::new("./daily/"), Path::new("/root")),
            PathBuf::from("/root/daily")
        );
    }
    #[test]
    fn absolutize_absolute_input_ignores_base() {
        assert_eq!(
            absolutize(Path::new("/abs/x"), Path::new("/root")),
            PathBuf::from("/abs/x")
        );
    }
    #[test]
    fn normalize_filter_empty_errors() {
        assert!(normalize_filter_path("", Path::new("/cwd")).is_err());
    }
    #[test]
    fn normalize_filter_dot_is_cwd() {
        assert_eq!(
            normalize_filter_path(".", Path::new("/cwd")).unwrap(),
            PathBuf::from("/cwd")
        );
    }
    #[test]
    fn is_within_boundary_not_substring() {
        assert!(is_within(Path::new("/r/daily/x.md"), Path::new("/r/daily")));
        assert!(is_within(Path::new("/r/daily"), Path::new("/r/daily")));
        assert!(!is_within(
            Path::new("/r/daily-report/x.md"),
            Path::new("/r/daily")
        ));
    }
    #[test]
    fn boundary_like_escapes_metachars() {
        let (eq, like) = boundary_like(Path::new("/r/daily_notes"));
        assert_eq!(eq, "/r/daily_notes");
        assert_eq!(like, r"/r/daily\_notes/%");
    }
    #[test]
    fn scope_clause_none_is_empty() {
        let (sql, params) = scope_clause(None);
        assert!(sql.is_empty());
        assert!(params.is_empty());
        let (sql, params) = scope_clause(Some(&[]));
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }
    #[test]
    fn scope_clause_multiple_or_joined_with_params() {
        let prefixes = vec!["/r/daily".to_string(), "/r/docs".to_string()];
        let (sql, params) = scope_clause(Some(&prefixes));
        assert!(sql.starts_with(" AND ("));
        assert_eq!(sql.matches("d.file_path = ?").count(), 2);
        assert_eq!(sql.matches(" OR ").count(), 3); // 1 between branches per cond (2) + 1 joining conds
        assert_eq!(
            params,
            vec!["/r/daily", "/r/daily/%", "/r/docs", "/r/docs/%"]
        );
    }
}
