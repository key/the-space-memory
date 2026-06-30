//! Pure argument/decision helpers for CLI commands.
//!
//! Path-filter normalization, search-fallback resolution, and the cache
//! placement decision are pure: they take already-fetched values (CWD, the
//! configured fallback, the results of FS probes) and return a value. The FS
//! probes (`exists`, `canonicalize`), config reads, and logging stay in the
//! `cli.rs` shell.

use std::path::{Path, PathBuf};

use crate::config::SearchFallback;

/// Normalize `--path` args to deduped, CWD-anchored absolute paths.
/// Accepts absolute or relative input; an empty string is the only error.
pub fn normalize_path_filters(args: &[String], cwd: &Path) -> anyhow::Result<Option<Vec<String>>> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for a in args {
        let p = crate::paths::normalize_filter_path(a, cwd)?
            .to_string_lossy()
            .to_string();
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    Ok(Some(out))
}

/// Resolve the search fallback policy: an explicit CLI flag wins, otherwise the
/// configured default. An unparseable flag is surfaced as an error.
pub fn resolve_fallback(
    flag: Option<&str>,
    configured: SearchFallback,
) -> anyhow::Result<SearchFallback> {
    match flag {
        Some(s) => s
            .parse::<SearchFallback>()
            .map_err(|e| anyhow::anyhow!("{e}")),
        None => Ok(configured),
    }
}

/// Whether the search must have a live vector backend (no FTS-only fallback).
pub fn require_vector(fallback: SearchFallback) -> bool {
    fallback == SearchFallback::Error
}

/// Whether (and why not) a cached resource should be materialized at `dst`.
#[derive(Debug, PartialEq, Eq)]
pub enum PlacementDecision {
    /// Materialize `src` at `dst`.
    Place,
    /// The cache entry is absent (pre-`tsm setup`); skip and warn.
    SkipMissing,
    /// `src` and `dst` are the same entry; skip to avoid destroying the cache.
    SkipSameLocation,
}

/// Decide placement from the two FS facts the shell gathers (`src` existence and
/// whether `src`/`dst` resolve to the same entry). Missing-source takes
/// precedence over same-location, matching the original guard order.
pub fn placement_decision(src_exists: bool, same_location: bool) -> PlacementDecision {
    if !src_exists {
        PlacementDecision::SkipMissing
    } else if same_location {
        PlacementDecision::SkipSameLocation
    } else {
        PlacementDecision::Place
    }
}

/// Compare two already-resolved entry locations. The caller resolves each path's
/// parent (canonicalizing symlinked ancestors) without dereferencing the final
/// component; `None` (an unresolvable parent) compares as distinct so placement
/// proceeds normally.
pub fn same_resolved_location(a: Option<PathBuf>, b: Option<PathBuf>) -> bool {
    match (a, b) {
        (Some(la), Some(lb)) => la == lb,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_filters_abs_and_rel_and_dedup() {
        let cwd = Path::new("/root/repoA");
        let got = normalize_path_filters(
            &[
                "daily".into(),
                "/root/repoA/daily".into(),
                "../repoB".into(),
            ],
            cwd,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            got,
            vec!["/root/repoA/daily".to_string(), "/root/repoB".to_string()]
        );
    }

    #[test]
    fn normalize_path_filters_empty_arg_errors() {
        assert!(normalize_path_filters(&["".into()], Path::new("/c")).is_err());
    }

    #[test]
    fn normalize_path_filters_none_when_no_args() {
        assert!(normalize_path_filters(&[], Path::new("/c"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolve_fallback_flag_wins_over_config() {
        assert_eq!(
            resolve_fallback(Some("fts_only"), SearchFallback::Error).unwrap(),
            SearchFallback::FtsOnly
        );
        assert_eq!(
            resolve_fallback(Some("error"), SearchFallback::FtsOnly).unwrap(),
            SearchFallback::Error
        );
    }

    #[test]
    fn resolve_fallback_uses_config_when_absent() {
        assert_eq!(
            resolve_fallback(None, SearchFallback::FtsOnly).unwrap(),
            SearchFallback::FtsOnly
        );
        assert_eq!(
            resolve_fallback(None, SearchFallback::Error).unwrap(),
            SearchFallback::Error
        );
    }

    #[test]
    fn resolve_fallback_rejects_unknown_flag() {
        let err = resolve_fallback(Some("bogus"), SearchFallback::Error).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn require_vector_only_for_error_policy() {
        assert!(require_vector(SearchFallback::Error));
        assert!(!require_vector(SearchFallback::FtsOnly));
    }

    #[test]
    fn placement_missing_source_takes_precedence() {
        // Missing wins even if the (irrelevant) same-location flag is set.
        assert_eq!(
            placement_decision(false, true),
            PlacementDecision::SkipMissing
        );
        assert_eq!(
            placement_decision(false, false),
            PlacementDecision::SkipMissing
        );
    }

    #[test]
    fn placement_same_location_skips() {
        assert_eq!(
            placement_decision(true, true),
            PlacementDecision::SkipSameLocation
        );
    }

    #[test]
    fn placement_places_when_present_and_distinct() {
        assert_eq!(placement_decision(true, false), PlacementDecision::Place);
    }

    #[test]
    fn same_resolved_location_compares_some_and_treats_none_as_distinct() {
        let a = PathBuf::from("/x/models");
        assert!(same_resolved_location(Some(a.clone()), Some(a.clone())));
        assert!(!same_resolved_location(
            Some(a.clone()),
            Some(PathBuf::from("/y/models"))
        ));
        assert!(!same_resolved_location(Some(a.clone()), None));
        assert!(!same_resolved_location(None, Some(a)));
        assert!(!same_resolved_location(None, None));
    }
}
