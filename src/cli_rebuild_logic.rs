//! Pure decision/text logic for `tsm rebuild`.
//!
//! The FS probes (DB existence, file size), the DB queries, the embedder-socket
//! check, the status read, and the actual background spawn all stay in the
//! `cli.rs` shell. This module turns the already-gathered counts and booleans
//! into a `BackfillAction` and the dry-run report text, so every branch is
//! exercisable as a pure value.

use std::fmt::Write as _;

/// What `report_vectors_and_backfill` should do after a rebuild, given the
/// vector/chunk counts and the embedder/backfill state.
#[derive(Debug, PartialEq, Eq)]
pub enum BackfillAction {
    /// Every chunk already has a vector — report completion.
    VectorsMatch,
    /// Vectors are missing but a backfill is already running.
    AlreadyInProgress,
    /// Vectors are missing and the embedder is up — start a background backfill.
    StartBackfill,
    /// Vectors are missing and the embedder is down — skip with a warning.
    EmbedderDown,
}

/// Decide the post-rebuild backfill action. `socket_exists` and
/// `backfill_in_progress` are sampled by the shell (FS + status read). When
/// `vecs < chunks` the counts come from `COUNT(*)` (both `>= 0`), so `chunks`
/// is necessarily `> 0` in the final arm — no separate "no chunks" case exists.
pub fn decide_backfill_action(
    vecs: i64,
    chunks: i64,
    socket_exists: bool,
    backfill_in_progress: bool,
) -> BackfillAction {
    if vecs >= chunks {
        BackfillAction::VectorsMatch
    } else if socket_exists {
        if backfill_in_progress {
            BackfillAction::AlreadyInProgress
        } else {
            BackfillAction::StartBackfill
        }
    } else {
        BackfillAction::EmbedderDown
    }
}

/// DB facts gathered by the shell for the dry-run report. `size_mb` is `None`
/// when the file metadata could not be read.
pub struct DryRunStats {
    pub size_mb: Option<f64>,
    pub docs: i64,
    pub chunks: i64,
    pub vecs: i64,
    pub sessions: i64,
}

/// Render the `tsm rebuild` (no `--apply`) dry-run report. `stats` is `None`
/// when the DB does not exist yet; `db_path_display` labels the size line.
pub fn rebuild_dry_run_report(stats: Option<&DryRunStats>, db_path_display: &str) -> String {
    let mut out = String::new();
    match stats {
        Some(s) => {
            if let Some(mb) = s.size_mb {
                let _ = writeln!(out, "DB: {db_path_display} ({mb:.1} MB)");
            }
            let _ = writeln!(
                out,
                "Documents: {}, Chunks: {}, Vectors: {}",
                s.docs, s.chunks, s.vecs
            );
            if s.sessions > 0 {
                let _ = writeln!(
                    out,
                    "Sessions: {} (ingested session documents, not re-walked from disk)",
                    s.sessions
                );
            }
        }
        None => {
            let _ = writeln!(out, "DB does not exist yet.");
        }
    }
    // One writeln per original eprintln, preserving the exact bytes (the `\`
    // continuations join each note onto a single line, as before).
    let _ = writeln!(out, "\nThis will delete the DB and rebuild from scratch.");
    let _ = writeln!(
        out,
        "Note: ingested session documents (session:*) and their chunks are carried \
         forward (re-indexed from the existing chunks in the DB)."
    );
    let _ = writeln!(
        out,
        "Note: dictionary verdicts (dictionary_candidates) will be lost."
    );
    let _ = writeln!(
        out,
        "      The accepted set and reject list are gone until restored from \
         reject_words.txt (`tsm dict reject --apply`) / `tsm dict import` (once available); \
         a `dict add`/`rm` before then regenerates user_dict.simpledic from the empty DB."
    );
    let _ = writeln!(out, "Run with --apply to proceed.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_vectors_match_when_complete() {
        assert_eq!(
            decide_backfill_action(10, 10, true, false),
            BackfillAction::VectorsMatch
        );
        // chunks == 0: vecs (0) >= chunks (0) → match, regardless of socket.
        assert_eq!(
            decide_backfill_action(0, 0, false, false),
            BackfillAction::VectorsMatch
        );
    }

    #[test]
    fn backfill_already_in_progress() {
        assert_eq!(
            decide_backfill_action(5, 10, true, true),
            BackfillAction::AlreadyInProgress
        );
    }

    #[test]
    fn backfill_starts_when_embedder_up_and_idle() {
        assert_eq!(
            decide_backfill_action(5, 10, true, false),
            BackfillAction::StartBackfill
        );
    }

    #[test]
    fn backfill_embedder_down_skips() {
        assert_eq!(
            decide_backfill_action(5, 10, false, false),
            BackfillAction::EmbedderDown
        );
        // backfill_in_progress is ignored when the socket is down.
        assert_eq!(
            decide_backfill_action(5, 10, false, true),
            BackfillAction::EmbedderDown
        );
    }

    #[test]
    fn dry_run_report_db_absent() {
        let got = rebuild_dry_run_report(None, "/x/tsm.db");
        assert!(got.starts_with("DB does not exist yet.\n"));
        assert!(got.contains("This will delete the DB and rebuild from scratch."));
        assert!(got.contains("dictionary verdicts (dictionary_candidates) will be lost."));
        assert!(got.ends_with("Run with --apply to proceed.\n"));
        assert!(!got.contains("Documents:"));
    }

    #[test]
    fn dry_run_report_with_stats_and_sessions() {
        let stats = DryRunStats {
            size_mb: Some(12.34),
            docs: 100,
            chunks: 200,
            vecs: 150,
            sessions: 3,
        };
        let got = rebuild_dry_run_report(Some(&stats), "/x/tsm.db");
        assert!(got.contains("DB: /x/tsm.db (12.3 MB)"));
        assert!(got.contains("Documents: 100, Chunks: 200, Vectors: 150"));
        assert!(got.contains("Sessions: 3 (ingested session documents, not re-walked from disk)"));
        assert!(got.ends_with("Run with --apply to proceed.\n"));
    }

    #[test]
    fn dry_run_report_stats_without_size_or_sessions() {
        let stats = DryRunStats {
            size_mb: None,
            docs: 1,
            chunks: 2,
            vecs: 0,
            sessions: 0,
        };
        let got = rebuild_dry_run_report(Some(&stats), "/x/tsm.db");
        assert!(!got.contains("MB)"));
        assert!(!got.contains("Sessions:"));
        assert!(got.contains("Documents: 1, Chunks: 2, Vectors: 0"));
    }
}
