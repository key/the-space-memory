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
    /// Take ownership of the reindex-active flag; drop clears it.
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        ReindexGuard(flag)
    }
}

impl Drop for ReindexGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
    fn test_reindex_guard_clears_flag_on_drop() {
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _guard = ReindexGuard::new(Arc::clone(&flag));
            assert!(flag.load(Ordering::Acquire));
        }
        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn test_reindex_guard_is_debug() {
        // The guard must derive Debug (#268 item 5): assert it formats.
        let flag = Arc::new(AtomicBool::new(true));
        let guard = ReindexGuard::new(flag);
        assert!(format!("{guard:?}").contains("ReindexGuard"));
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
