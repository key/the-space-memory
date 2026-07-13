//! Pure decision/message logic for the `tsm dict` and dict-triggered reindex
//! commands.
//!
//! These helpers map already-fetched values (a daemon response, a verdict
//! transition, a reconcile outcome) to a `ReindexPlan` or a user-facing
//! message. The I/O — opening the DB, sending the IPC request, printing — stays
//! in the `cli.rs` shell, so each branch here is exercisable as a pure value.

use crate::daemon_protocol::DaemonResponse;
use crate::user_dict::{ReconcileOutcome, Verdict};

/// What to do with the daemon's response to a dict-triggered FTS reindex.
#[derive(Debug, PartialEq, Eq)]
pub enum ReindexPlan {
    /// No daemon is running — rebuild the FTS index locally.
    Local,
    /// The daemon accepted the request and is reindexing in the background.
    BackgroundStarted,
    /// The daemon was reachable but the reindex failed; surface this rather than
    /// silently falling back to a local rebuild that races the daemon's writer.
    Failed(String),
}

/// Classify a daemon reindex response. Only a missing daemon (`None`) warrants a
/// local rebuild; an IPC error (`Some(Err)`) or an explicit rejection
/// (`Some(Ok)` with `ok == false`) is surfaced so the caller does not quietly
/// run a competing local rebuild against the daemon-owned DB.
pub fn classify_reindex(resp: Option<anyhow::Result<DaemonResponse>>) -> ReindexPlan {
    match resp {
        None => ReindexPlan::Local,
        Some(Ok(r)) if r.ok => ReindexPlan::BackgroundStarted,
        Some(Ok(r)) => ReindexPlan::Failed(
            r.error
                .unwrap_or_else(|| "daemon rejected the reindex request".to_string()),
        ),
        Some(Err(e)) => ReindexPlan::Failed(e.to_string()),
    }
}

/// Message for `tsm dict add`: the term is freshly accepted, or already was.
pub fn accept_message(from: Option<Verdict>, surface: &str) -> String {
    match from {
        Some(Verdict::Accepted) => format!("'{surface}' is already accepted."),
        _ => format!("Accepted '{surface}'."),
    }
}

/// Message for `tsm dict rm`: the term is reset to pending, or already was.
pub fn reset_message(from: Option<Verdict>, word: &str) -> String {
    match from {
        Some(Verdict::Pending) => format!("'{word}' is already pending."),
        _ => format!("Reset '{word}' to pending."),
    }
}

/// Message for `tsm dict reject`, distinguishing a new entry, a no-op, and the
/// accepted→rejected demotion that regenerates the dictionary.
pub fn reject_message(from: Option<Verdict>, word: &str) -> String {
    match from {
        None => format!("Rejected '{word}' (new entry)."),
        Some(Verdict::Rejected) => format!("'{word}' is already rejected."),
        Some(Verdict::Accepted) => {
            format!("Rejected '{word}' (was accepted; dictionary regenerated).")
        }
        Some(Verdict::Pending) => format!("Rejected '{word}'."),
    }
}

/// The stderr note surfacing the implicit DB change when the reconcile recovered
/// on-disk terms, or `None` when nothing was healed (no message).
pub fn reconcile_message(r: &ReconcileOutcome) -> Option<String> {
    if r.is_empty() {
        return None;
    }
    Some(format!(
        "Recovered {} accepted and {} rejected term(s) from the on-disk files into the database.",
        r.accepted_healed, r.rejected_healed
    ))
}

/// Error message when `regenerate_user_dict` fails after the DB was already
/// committed: the DB is correct, but the on-disk file may now be stale.
pub fn regen_failed_message(csv_path: &std::path::Path, e: &anyhow::Error) -> String {
    format!(
        "the database was updated, but regenerating {} failed: {e}. \
         Re-run the command (or `tsm dict export`) to retry.",
        csv_path.display()
    )
}

/// Status line for a successful `regenerate_user_dict` that changed the file.
pub fn regenerated_message(csv_path: &std::path::Path, written: usize) -> String {
    format!(
        "Regenerated {} ({written} accepted term{}).",
        csv_path.display(),
        if written == 1 { "" } else { "s" }
    )
}

/// Warning for `tsm dict add` when no reading was given for a kanji surface —
/// the surface itself becomes a substitute reading.
pub fn no_reading_warning(surface: &str) -> String {
    format!(
        "warning: no reading for '{surface}'; storing the surface as a substitute. \
         Provide one with `tsm dict add {surface} <yomi>`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(ok: bool, error: Option<&str>) -> DaemonResponse {
        DaemonResponse {
            ok,
            error: error.map(str::to_string),
            payload: None,
        }
    }

    #[test]
    fn classify_reindex_none_is_local() {
        assert_eq!(classify_reindex(None), ReindexPlan::Local);
    }

    #[test]
    fn classify_reindex_ok_true_is_background() {
        assert_eq!(
            classify_reindex(Some(Ok(resp(true, None)))),
            ReindexPlan::BackgroundStarted
        );
    }

    #[test]
    fn classify_reindex_daemon_rejection_is_failed_with_message() {
        // Some(Ok{ok:false}) must surface, not fall back to a local rebuild.
        assert_eq!(
            classify_reindex(Some(Ok(resp(false, Some("queue full"))))),
            ReindexPlan::Failed("queue full".to_string())
        );
        // Missing error string still fails (with a default message), never Local.
        assert_eq!(
            classify_reindex(Some(Ok(resp(false, None)))),
            ReindexPlan::Failed("daemon rejected the reindex request".to_string())
        );
    }

    #[test]
    fn classify_reindex_ipc_error_is_failed() {
        let plan = classify_reindex(Some(Err(anyhow::anyhow!("broken pipe"))));
        assert_eq!(plan, ReindexPlan::Failed("broken pipe".to_string()));
    }

    #[test]
    fn accept_message_new_vs_already() {
        assert_eq!(accept_message(None, "foo"), "Accepted 'foo'.");
        assert_eq!(
            accept_message(Some(Verdict::Pending), "foo"),
            "Accepted 'foo'."
        );
        assert_eq!(
            accept_message(Some(Verdict::Accepted), "foo"),
            "'foo' is already accepted."
        );
    }

    #[test]
    fn reset_message_new_vs_already() {
        assert_eq!(reset_message(None, "bar"), "Reset 'bar' to pending.");
        assert_eq!(
            reset_message(Some(Verdict::Accepted), "bar"),
            "Reset 'bar' to pending."
        );
        assert_eq!(
            reset_message(Some(Verdict::Pending), "bar"),
            "'bar' is already pending."
        );
    }

    #[test]
    fn reject_message_covers_every_transition() {
        assert_eq!(reject_message(None, "x"), "Rejected 'x' (new entry).");
        assert_eq!(
            reject_message(Some(Verdict::Rejected), "x"),
            "'x' is already rejected."
        );
        assert_eq!(
            reject_message(Some(Verdict::Accepted), "x"),
            "Rejected 'x' (was accepted; dictionary regenerated)."
        );
        assert_eq!(reject_message(Some(Verdict::Pending), "x"), "Rejected 'x'.");
    }

    #[test]
    fn reconcile_message_none_when_empty() {
        let outcome = ReconcileOutcome {
            accepted_healed: 0,
            rejected_healed: 0,
        };
        assert_eq!(reconcile_message(&outcome), None);
    }

    #[test]
    fn reconcile_message_reports_healed_counts() {
        let outcome = ReconcileOutcome {
            accepted_healed: 2,
            rejected_healed: 1,
        };
        assert_eq!(
            reconcile_message(&outcome).as_deref(),
            Some("Recovered 2 accepted and 1 rejected term(s) from the on-disk files into the database.")
        );
    }

    #[test]
    fn regen_failed_message_names_path_and_retry_hint() {
        let path = std::path::Path::new("/tmp/user_dict.simpledic");
        let err = anyhow::anyhow!("disk full");
        let msg = regen_failed_message(path, &err);
        assert!(msg.contains("/tmp/user_dict.simpledic"));
        assert!(msg.contains("disk full"));
        assert!(msg.contains("tsm dict export"));
    }

    #[test]
    fn regenerated_message_pluralizes_by_count() {
        let path = std::path::Path::new("/tmp/user_dict.simpledic");
        assert_eq!(
            regenerated_message(path, 1),
            "Regenerated /tmp/user_dict.simpledic (1 accepted term)."
        );
        assert_eq!(
            regenerated_message(path, 2),
            "Regenerated /tmp/user_dict.simpledic (2 accepted terms)."
        );
        assert_eq!(
            regenerated_message(path, 0),
            "Regenerated /tmp/user_dict.simpledic (0 accepted terms)."
        );
    }

    #[test]
    fn no_reading_warning_names_surface_and_the_retry_command() {
        let msg = no_reading_warning("漢字");
        assert!(msg.contains("漢字"));
        assert!(msg.contains("tsm dict add 漢字 <yomi>"));
    }
}
