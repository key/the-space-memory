use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{recommended_watcher, Event, RecommendedWatcher, RecursiveMode, Watcher};

use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest};

use crate::watch_logic::{
    diff_watch_set, extension_allowed, is_index_relevant, watch_targets, Debounce,
};
use crate::SHUTDOWN;

/// Flag set by SIGHUP to trigger watch target reload.
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Quiet period a path must observe before its change is forwarded to the
/// daemon. Coalesces a burst of events for the same file into one index request.
const DEBOUNCE: Duration = Duration::from_secs(2);

extern "C" fn sighup_handler(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
}

/// Entry point for `tsmd --fs-watcher`.
pub fn run() -> Result<()> {
    // Log to stderr (inherited from the daemon, captured into tsmd-stderr.log);
    // children do not manage their own log files.
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::DaemonStderr)?;
    let project_root = config::project_root();

    // Install signal handlers
    unsafe {
        libc::signal(
            libc::SIGHUP,
            sighup_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            crate::signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            crate::signal_handler as *const () as libc::sighandler_t,
        );
    }

    // Raw notify watcher. The handler filters by event kind *before* the event
    // reaches our debounce, so `Access(Open)` noise never enters
    // the pipeline. Surviving events and watch errors are forwarded over the
    // channel; the main loop debounces and dispatches them.
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<Event, notify::Error>>();
    let mut watcher: RecommendedWatcher = recommended_watcher(
        move |res: std::result::Result<Event, notify::Error>| match res {
            Ok(event) if is_index_relevant(&event.kind) => {
                let _ = tx.send(Ok(event));
            }
            Ok(_) => {} // drop Access / metadata-only events
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        },
    )
    .context("Failed to create file watcher")?;

    // The watcher's *registration* scope comes purely from `project_root` +
    // `content_dirs` — it does NOT consult `.tsmignore` or
    // `respect_gitignore`. Those are policy concerns owned by the daemon's
    // indexer via `IngestPolicy`. Keeping registration oblivious to ignore
    // rules means the watched directory set is independent of user policy
    // edits (no SIGHUP needed to pick up `.tsmignore` changes — the
    // indexer's next gate call does that).
    //
    // The event *forwarding* loop below does apply the index-extension
    // allowlist (but still not `.tsmignore`) as a caller-side pre-filter —
    // see `extension_allowed`'s doc comment for why that one check is safe
    // to duplicate here without reintroducing a reload dependency.
    let mut watched = setup_watches(&mut watcher, &project_root);

    if watched.is_empty() {
        anyhow::bail!(
            "No content directories found to watch under {}",
            project_root.display()
        );
    }

    log::info!(
        "watching {} directories under {}",
        watched.len(),
        project_root.display()
    );

    let daemon_socket = config::daemon_socket_path();
    let mut debounce = Debounce::new(DEBOUNCE);
    let mut extensions = config::index_extensions();

    while !SHUTDOWN.load(Ordering::SeqCst) {
        // Handle reload requests (SIGHUP from tsmd)
        if RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
            log::info!("reload notification received, updating watch targets");
            config::reload();
            // Only `content_dirs` can change the watch scope; a new
            // `.tsmignore` would not affect registration (the watcher
            // doesn't know about it by design). `extensions` is refreshed
            // here too, since the forwarding pre-filter reads it.
            extensions = config::index_extensions();
            update_watches(&mut watcher, &mut watched, &project_root);
            if watched.is_empty() {
                // Config edit left us with nothing to watch (e.g. every
                // `content_dirs` entry points at a nonexistent path).
                // Startup would have bailed with anyhow::bail! but the
                // reload path can't — surface the dead state at ERROR so
                // the operator sees it without tailing "0 directories" in
                // info logs.
                log::error!(
                    "no content directories registered after reload; \
                     file changes will NOT be detected until `tsm restart`"
                );
            } else {
                log::info!("now watching {} directories", watched.len());
            }
        }

        // Wake at least every 500ms (to re-check SHUTDOWN/SIGHUP), and sooner if
        // a pending path is closer to its debounce deadline.
        let wait = debounce
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(500))
            .min(Duration::from_millis(500));

        match rx.recv_timeout(wait) {
            Ok(Ok(event)) => {
                let now = Instant::now();
                for path in &event.paths {
                    // Debounce keys and forwarded paths are absolute
                    // (lexically folded, symlinks not resolved), matching
                    // `tsm index --files-from-stdin`'s wire convention: the
                    // daemon's `project_root.join` passes an absolute path
                    // through unchanged, so no project_root-relative framing
                    // is needed. This also covers `content_dirs` registered
                    // above project_root via `..` — those events are just as
                    // real as project_root-local ones and must not be
                    // dropped.
                    let abs = the_space_memory::paths::absolutize(path, &project_root);
                    if !extension_allowed(&extensions, &abs) {
                        continue;
                    }
                    debounce.record_at(abs.to_string_lossy().into_owned(), now);
                }
            }
            Ok(Err(e)) => {
                log::warn!("watch error: {e}");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                log::warn!("watcher channel disconnected");
                break;
            }
        }

        // Flush every iteration — including after a Timeout — so a burst that
        // goes quiet always drains even when no further event arrives to wake
        // `recv_timeout`.
        let ready = debounce.flush_ready(Instant::now());
        if !ready.is_empty() {
            let count = ready.len();
            log::info!("detected {count} changed file(s), sending index request");

            match daemon_protocol::send_request(
                &daemon_socket,
                &DaemonRequest::Index { files: ready },
            ) {
                Ok(resp) => {
                    if !resp.ok {
                        log::warn!("index request failed: {}", resp.error.unwrap_or_default());
                    }
                }
                Err(e) => {
                    log::warn!("failed to send index request to daemon: {e}");
                }
            }
        }
    }

    Ok(())
}

/// Set up watch targets and return the set of watched directories.
fn setup_watches(watcher: &mut RecommendedWatcher, project_root: &Path) -> HashSet<PathBuf> {
    let mut watched = HashSet::new();
    let dirs = config::content_dirs();
    for full_dir in watch_targets(project_root, &dirs) {
        if let Err(e) = watcher.watch(&full_dir, RecursiveMode::Recursive) {
            log::warn!("cannot watch {}: {e}", full_dir.display());
        } else {
            watched.insert(full_dir);
        }
    }
    watched
}

/// Update watch targets: unwatch removed dirs, watch added dirs.
fn update_watches(
    watcher: &mut RecommendedWatcher,
    current: &mut HashSet<PathBuf>,
    project_root: &Path,
) {
    let dirs = config::content_dirs();
    let desired: HashSet<PathBuf> = watch_targets(project_root, &dirs).into_iter().collect();
    let diff = diff_watch_set(current, &desired);

    // Unwatch removed dirs
    for dir in &diff.to_unwatch {
        log::info!("unwatching {}", dir.display());
        if let Err(e) = watcher.unwatch(dir) {
            log::warn!("failed to unwatch {}: {e}", dir.display());
        }
    }

    // Watch new dirs (keep the unchanged set, add only successfully watched ones)
    let mut actually_watched = diff.kept;
    for dir in &diff.to_watch {
        log::info!("watching {}", dir.display());
        if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
            log::warn!("cannot watch {}: {e}", dir.display());
        } else {
            actually_watched.insert(dir.clone());
        }
    }

    *current = actually_watched;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// RAII guard restoring CWD on drop. The walker reads `.tsmignore`
    /// relative to CWD and loads `tsm.toml` from CWD; tests that want a
    /// clean default config must CD into a directory where no `tsm.toml`
    /// exists. Using a guard keeps later tests from inheriting this CWD.
    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn change_to(new_cwd: &Path) -> Self {
            let original = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
            std::env::set_current_dir(new_cwd).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original)
                .or_else(|_| std::env::set_current_dir("/"));
        }
    }

    #[test]
    fn test_sighup_handler_sets_flag() {
        RELOAD_REQUESTED.store(false, Ordering::SeqCst);
        sighup_handler(libc::SIGHUP);
        assert!(RELOAD_REQUESTED.load(Ordering::SeqCst));
        // Reset
        RELOAD_REQUESTED.store(false, Ordering::SeqCst);
    }

    #[test]
    #[serial_test::serial]
    fn test_run_watcher_registers_project_root() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();

        SHUTDOWN.store(true, Ordering::SeqCst);

        let _cwd = CwdGuard::change_to(dir.path());
        the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::DaemonStderr)
            .ok();

        // We test the individual pieces rather than run() which also inits logger.
        // `_rx` is held only to keep the channel open for the watcher's handler;
        // we do not assert on its timing. Raw notify delivers events immediately
        // (no debounce buffer), and on Linux registering a recursive watch can
        // surface a startup event, so asserting channel silence here would be
        // flaky. Startup-event handling (debounce + kind filter) is exercised by
        // the debounce/predicate unit tests and the e2e race regression instead.
        let (tx, _rx) = std::sync::mpsc::channel::<std::result::Result<Event, notify::Error>>();
        let mut watcher: RecommendedWatcher =
            recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                let _ = tx.send(res);
            })
            .unwrap();
        let watched = setup_watches(&mut watcher, dir.path());
        assert!(!watched.is_empty());
        assert!(watched.contains(dir.path()));

        // The main loop condition checks SHUTDOWN
        assert!(SHUTDOWN.load(Ordering::SeqCst));

        SHUTDOWN.store(false, Ordering::SeqCst);
    }
}
