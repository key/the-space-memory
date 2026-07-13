//! Pure daemon-side helpers extracted from the daemon process shell.
//!
//! These functions take values/bools and return values — the filesystem,
//! socket, signal, and clock effects stay in `daemon_proc.rs`. Keeping them
//! here makes the decision logic unit-testable (and counted) without a live
//! daemon.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use the_space_memory::daemon_protocol::{DaemonResponse, ReindexKind};
use the_space_memory::status::StatusFile;

/// Build the argv for spawning the embedder child.
///
/// `model_dir` is `Some` when the local model files are complete (the caller
/// performs that filesystem check); a `--model=<dir>` arg is then appended so
/// the embedder loads from disk instead of falling back to the HF Hub cache.
pub fn embedder_child_args(model_dir: Option<&Path>) -> Vec<String> {
    let mut args = vec!["--embedder".to_string(), "--no-idle-timeout".to_string()];
    if let Some(dir) = model_dir {
        args.push(format!("--model={}", dir.display()));
    }
    args
}

/// Which stale progress entries a startup clear removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StaleProgressCleared {
    pub backfill: bool,
    pub reindex: bool,
}

/// Clear `backfill`/`reindex` progress left over from a previous daemon's
/// interrupted run. A fresh daemon start means no such pass can still be
/// executing, so a populated entry is always a leftover, never live progress —
/// left alone, `backfill` renders in `tsm status` (and both render in `tsm
/// doctor`) as an indefinitely "running" operation with an ever-worsening
/// ETA. Other status fields (`daemon`, `embedder`, `watcher`) are untouched;
/// the caller sets those separately for the current process.
pub fn clear_stale_progress(sf: &mut StatusFile) -> StaleProgressCleared {
    StaleProgressCleared {
        backfill: sf.backfill.take().is_some(),
        reindex: sf.reindex.take().is_some(),
    }
}

/// One step in a reindex sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexStep {
    Fts,
    Vectors,
}

/// Expand a `ReindexKind` into the ordered passes it runs. `All` is `Fts` then
/// `Vectors`; the caller checks the shutdown flag between steps.
pub fn reindex_steps(kind: ReindexKind) -> &'static [ReindexStep] {
    match kind {
        ReindexKind::Fts => &[ReindexStep::Fts],
        ReindexKind::Vectors => &[ReindexStep::Vectors],
        // The order here is the contract. Adding a new pass to `All` means
        // adding a `ReindexStep` variant (the match below is exhaustive) AND
        // extending this slice — the compiler catches the former, not the latter.
        ReindexKind::All => &[ReindexStep::Fts, ReindexStep::Vectors],
    }
}

/// Build the daemon's response to a `Reload` request.
///
/// `watcher_pid_present` is whether a watcher PID was known; `sighup_ok` is
/// whether the SIGHUP delivery succeeded (only meaningful when the PID is
/// present — the caller passes `true` otherwise). The base `warnings` come from
/// `config::reload()`. A response with no warnings is `success_empty`; otherwise
/// the warnings travel back in the payload.
pub fn reload_response(
    mut warnings: Vec<String>,
    watcher_pid_present: bool,
    sighup_ok: bool,
) -> DaemonResponse {
    match (watcher_pid_present, sighup_ok) {
        (true, false) => {
            warnings.push("failed to send SIGHUP to watcher (may have exited)".to_string());
        }
        (false, _) => {
            warnings.push("watcher is not running; watch targets not updated".to_string());
        }
        (true, true) => {}
    }
    if warnings.is_empty() {
        DaemonResponse::success_empty()
    } else {
        DaemonResponse::success(serde_json::json!({ "warnings": warnings }))
    }
}

/// RAII guard that clears the reindex-active flag on drop, even on panic, so a
/// crashed reindex thread never wedges the daemon into "reindex already in
/// progress".
///
/// Deliberately NOT shared with `backfill_logic::PendingWriteGuard`: that guard
/// decrements an `AtomicUsize` counter (several writes can be in flight at once),
/// while this one resets a single `AtomicBool` (at most one reindex runs at a
/// time). The state types differ, so a single shared guard would be wrong.
///
/// `mem::forget`-ing this guard leaves the flag set and blocks all future
/// reindexes — never do so.
#[derive(Debug)]
pub struct ReindexGuard(Arc<AtomicBool>);

impl ReindexGuard {
    /// Atomically claim the reindex-active flag. Returns `Some(guard)` if the
    /// flag was clear (this caller now owns the reindex; drop clears it), or
    /// `None` if a reindex is already in progress.
    ///
    /// Claim and guard creation are one operation — like its sibling
    /// `backfill_logic::PendingWriteGuard::new` — so the type, not a two-step
    /// caller protocol, owns the "a guard implies the flag is set" invariant.
    pub fn try_acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ReindexGuard(Arc::clone(flag)))
    }
}

impl Drop for ReindexGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Whether `tsmd-stderr.log` has grown past its configured cap and should be
/// truncated.
///
/// A pure size comparison so the decision is unit-tested without a real fd;
/// the truncation itself — an `lseek` + `ftruncate` pair applied directly to
/// the daemon's own stderr descriptor, which `embedder`/`watcher` share via
/// inherited-fd semantics — is `daemon_proc.rs`'s job.
pub fn stderr_log_over_cap(current_size: u64, cap_bytes: u64) -> bool {
    current_size >= cap_bytes
}

/// Read a PID from a PID file. Returns `None` if the file is missing or unreadable.
pub fn read_pid_from_file(pid_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Check if a PID file points to a running process.
pub fn is_process_alive(pid_path: &Path) -> bool {
    let Some(pid) = read_pid_from_file(pid_path) else {
        return false;
    };
    // kill(pid, 0) checks process existence without sending a signal.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_embedder_child_args_without_model() {
        let args = embedder_child_args(None);
        assert_eq!(args, vec!["--embedder", "--no-idle-timeout"]);
    }

    #[test]
    fn test_embedder_child_args_with_model() {
        let dir = PathBuf::from("/cache/ruri-v3-30m");
        let args = embedder_child_args(Some(&dir));
        assert_eq!(
            args,
            vec![
                "--embedder",
                "--no-idle-timeout",
                "--model=/cache/ruri-v3-30m"
            ]
        );
    }

    #[test]
    fn test_clear_stale_progress_clears_both_when_both_present() {
        let mut sf = StatusFile {
            backfill: Some(the_space_memory::status::BackfillStatus {
                total: 100,
                filled: 10,
                errors: 0,
                started_at: "2026-06-26T06:52:01Z".to_string(),
            }),
            reindex: Some(the_space_memory::status::ReindexStatus {
                kind: ReindexKind::Fts,
                total: 50,
                processed: 5,
                errors: 0,
                started_at: "2026-06-26T06:52:01Z".to_string(),
            }),
            ..Default::default()
        };
        let cleared = clear_stale_progress(&mut sf);
        assert_eq!(
            cleared,
            StaleProgressCleared {
                backfill: true,
                reindex: true
            }
        );
        assert!(sf.backfill.is_none());
        assert!(sf.reindex.is_none());
    }

    #[test]
    fn test_clear_stale_progress_reports_false_when_absent() {
        let mut sf = StatusFile::default();
        let cleared = clear_stale_progress(&mut sf);
        assert_eq!(cleared, StaleProgressCleared::default());
    }

    #[test]
    fn test_clear_stale_progress_only_backfill_present() {
        let mut sf = StatusFile {
            backfill: Some(the_space_memory::status::BackfillStatus {
                total: 1,
                filled: 0,
                errors: 0,
                started_at: "t0".to_string(),
            }),
            ..Default::default()
        };
        let cleared = clear_stale_progress(&mut sf);
        assert!(cleared.backfill);
        assert!(!cleared.reindex);
        assert!(sf.backfill.is_none());
    }

    #[test]
    fn test_clear_stale_progress_only_reindex_present() {
        let mut sf = StatusFile {
            reindex: Some(the_space_memory::status::ReindexStatus {
                kind: ReindexKind::Vectors,
                total: 1,
                processed: 0,
                errors: 0,
                started_at: "t0".to_string(),
            }),
            ..Default::default()
        };
        let cleared = clear_stale_progress(&mut sf);
        assert!(!cleared.backfill);
        assert!(cleared.reindex);
        assert!(sf.reindex.is_none());
    }

    #[test]
    fn test_clear_stale_progress_leaves_other_fields_untouched() {
        let mut sf = StatusFile {
            daemon: Some(the_space_memory::status::DaemonStatus {
                started_at: "t0".to_string(),
                pid: 1,
                socket: "/tmp/d.sock".to_string(),
            }),
            embedder: Some(the_space_memory::status::EmbedderStatus {
                started_at: "t0".to_string(),
                pid: 2,
            }),
            watcher: Some(the_space_memory::status::WatcherStatus {
                started_at: "t0".to_string(),
                pid: 3,
            }),
            reindex: Some(the_space_memory::status::ReindexStatus {
                kind: ReindexKind::Vectors,
                total: 1,
                processed: 0,
                errors: 0,
                started_at: "t0".to_string(),
            }),
            ..Default::default()
        };
        clear_stale_progress(&mut sf);
        assert!(sf.daemon.is_some());
        assert!(sf.embedder.is_some());
        assert!(sf.watcher.is_some());
    }

    #[test]
    fn test_reindex_steps_fts() {
        assert_eq!(reindex_steps(ReindexKind::Fts), &[ReindexStep::Fts]);
    }

    #[test]
    fn test_reindex_steps_vectors() {
        assert_eq!(reindex_steps(ReindexKind::Vectors), &[ReindexStep::Vectors]);
    }

    #[test]
    fn test_reindex_steps_all_is_fts_then_vectors() {
        assert_eq!(
            reindex_steps(ReindexKind::All),
            &[ReindexStep::Fts, ReindexStep::Vectors]
        );
    }

    #[test]
    fn test_reload_response_no_warnings_when_sighup_ok() {
        let resp = reload_response(Vec::new(), true, true);
        assert!(resp.ok);
        assert!(resp.payload.is_none());
    }

    #[test]
    fn test_reload_response_warns_on_sighup_failure() {
        let resp = reload_response(Vec::new(), true, false);
        assert!(resp.ok);
        let warnings = resp.payload.unwrap()["warnings"].clone();
        assert_eq!(
            warnings,
            serde_json::json!(["failed to send SIGHUP to watcher (may have exited)"])
        );
    }

    #[test]
    fn test_reload_response_warns_when_watcher_absent() {
        let resp = reload_response(Vec::new(), false, true);
        assert!(resp.ok);
        let warnings = resp.payload.unwrap()["warnings"].clone();
        assert_eq!(
            warnings,
            serde_json::json!(["watcher is not running; watch targets not updated"])
        );
    }

    #[test]
    fn test_reload_response_preserves_base_warnings() {
        let resp = reload_response(vec!["config drifted".to_string()], true, true);
        let warnings = resp.payload.unwrap()["warnings"].clone();
        assert_eq!(warnings, serde_json::json!(["config drifted"]));
    }

    #[test]
    fn test_reload_response_combines_base_and_sighup_warning() {
        let resp = reload_response(vec!["config drifted".to_string()], true, false);
        let warnings = resp.payload.unwrap()["warnings"].clone();
        assert_eq!(
            warnings,
            serde_json::json!([
                "config drifted",
                "failed to send SIGHUP to watcher (may have exited)"
            ])
        );
    }

    #[test]
    fn test_reload_response_combines_base_and_watcher_absent_warning() {
        let resp = reload_response(vec!["config drifted".to_string()], false, true);
        let warnings = resp.payload.unwrap()["warnings"].clone();
        assert_eq!(
            warnings,
            serde_json::json!([
                "config drifted",
                "watcher is not running; watch targets not updated"
            ])
        );
    }

    #[test]
    fn test_reindex_guard_try_acquire_claims_clear_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let guard = ReindexGuard::try_acquire(&flag);
        assert!(guard.is_some());
        assert!(flag.load(Ordering::Acquire), "flag is claimed while held");
    }

    #[test]
    fn test_reindex_guard_try_acquire_rejects_when_busy() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(ReindexGuard::try_acquire(&flag).is_none());
        assert!(
            flag.load(Ordering::Acquire),
            "a rejected claim leaves it set"
        );
    }

    #[test]
    fn test_reindex_guard_clears_flag_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = ReindexGuard::try_acquire(&flag).expect("clear flag claims");
            assert!(flag.load(Ordering::Acquire));
        }
        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn test_reindex_guard_clears_flag_on_panic() {
        // Panic-safety is the whole point of the RAII guard: a worker thread
        // that crashes must not wedge the daemon into "reindex in progress".
        let flag = Arc::new(AtomicBool::new(false));
        let flag_in = Arc::clone(&flag);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = ReindexGuard::try_acquire(&flag_in).expect("clear flag claims");
            panic!("simulated worker crash");
        }));
        assert!(result.is_err());
        assert!(
            !flag.load(Ordering::Acquire),
            "flag must be cleared even after a panic"
        );
    }

    #[test]
    fn test_reindex_guard_is_debug() {
        // The guard must derive Debug (#268 item 5): assert it formats.
        let flag = Arc::new(AtomicBool::new(false));
        let guard = ReindexGuard::try_acquire(&flag).expect("clear flag claims");
        assert!(format!("{guard:?}").contains("ReindexGuard"));
    }

    #[test]
    fn test_stderr_log_over_cap_below_cap_is_false() {
        assert!(!stderr_log_over_cap(999, 1000));
    }

    #[test]
    fn test_stderr_log_over_cap_at_cap_is_true() {
        // At-cap truncates too, rather than waiting for the next check to
        // find it one byte over: a check that only fires strictly-above
        // could let the file sit exactly at the cap indefinitely on a daemon
        // that stops growing right at the boundary.
        assert!(stderr_log_over_cap(1000, 1000));
    }

    #[test]
    fn test_stderr_log_over_cap_above_cap_is_true() {
        assert!(stderr_log_over_cap(5_000_000, 1000));
    }

    #[test]
    fn test_is_process_alive_missing_file() {
        assert!(!is_process_alive(Path::new("/tmp/nonexistent.pid")));
    }

    #[test]
    fn test_is_process_alive_invalid_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("bad.pid");
        std::fs::write(&pid_path, "not-a-number").unwrap();
        assert!(!is_process_alive(&pid_path));
    }

    #[test]
    fn test_is_process_alive_self() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("self.pid");
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        assert!(is_process_alive(&pid_path));
    }

    #[test]
    fn test_is_process_alive_dead_process() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("dead.pid");
        // PID 99999999 is almost certainly not running
        std::fs::write(&pid_path, "99999999").unwrap();
        assert!(!is_process_alive(&pid_path));
    }

    #[test]
    fn test_read_pid_from_file_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("test.pid");
        std::fs::write(&pid_path, "12345").unwrap();
        assert_eq!(read_pid_from_file(&pid_path), Some(12345));
    }

    #[test]
    fn test_read_pid_from_file_trailing_newline() {
        // PID files written by other tools commonly end in a newline; `.trim()`
        // must absorb it (the watcher child writes its own PID file).
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("nl.pid");
        std::fs::write(&pid_path, "12345\n").unwrap();
        assert_eq!(read_pid_from_file(&pid_path), Some(12345));
    }

    #[test]
    fn test_read_pid_from_file_missing() {
        assert_eq!(read_pid_from_file(Path::new("/tmp/nonexistent.pid")), None);
    }

    #[test]
    fn test_read_pid_from_file_invalid() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid_path = dir.path().join("bad.pid");
        std::fs::write(&pid_path, "not-a-number").unwrap();
        assert_eq!(read_pid_from_file(&pid_path), None);
    }
}
