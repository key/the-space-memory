use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest};

use crate::SHUTDOWN;

/// Flag set by SIGHUP to trigger watch target reload.
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn sighup_handler(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
}

/// Entry point for `tsmd --fs-watcher`.
pub fn run() -> Result<()> {
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
        name: "tsmd-watcher",
    })?;
    let index_root = config::index_root();

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

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer =
        new_debouncer(Duration::from_secs(2), tx).context("Failed to create file watcher")?;

    // The watcher's scope comes purely from `index_root` + `content_dirs`
    // — it does NOT consult `.tsmignore`, `extensions`, or `respect_gitignore`.
    // Those are policy concerns owned by the daemon's indexer via
    // `IngestPolicy`. Keeping the watcher oblivious means the event stream
    // is independent of user policy edits (no SIGHUP needed to pick up
    // `.tsmignore` changes — the indexer's next gate call does that).
    let mut watched = setup_watches(&mut debouncer, &index_root);

    if watched.is_empty() {
        anyhow::bail!(
            "No content directories found to watch under {}",
            index_root.display()
        );
    }

    log::info!(
        "watching {} directories under {}",
        watched.len(),
        index_root.display()
    );

    let daemon_socket = config::daemon_socket_path();

    while !SHUTDOWN.load(Ordering::SeqCst) {
        // Handle reload requests (SIGHUP from tsmd)
        if RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
            log::info!("reload notification received, updating watch targets");
            config::reload();
            // Only `content_dirs` can change the watch scope; a new
            // `.tsmignore` would not affect registration (the watcher
            // doesn't know about it by design).
            update_watches(&mut debouncer, &mut watched, &index_root);
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

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                let mut files_to_index: HashSet<String> = HashSet::new();
                for event in &events {
                    if event.kind != DebouncedEventKind::Any {
                        continue;
                    }
                    // No per-event filter here: the daemon's indexer
                    // applies the IngestPolicy at ingest time. Keeping a
                    // duplicate predicate in the watcher was a bug-magnet
                    // (see #134 review), so every event is forwarded and
                    // the indexer makes the final call.
                    match event.path.strip_prefix(&index_root) {
                        Ok(rel) => {
                            files_to_index.insert(rel.to_string_lossy().into_owned());
                        }
                        Err(_) => {
                            log::warn!(
                                "path {} outside project root, skipping",
                                event.path.display()
                            );
                        }
                    }
                }

                if !files_to_index.is_empty() {
                    let files: Vec<String> = files_to_index.into_iter().collect();
                    let count = files.len();
                    log::info!("detected {count} changed file(s), sending index request");

                    match daemon_protocol::send_request(
                        &daemon_socket,
                        &DaemonRequest::Index {
                            files: files.clone(),
                        },
                    ) {
                        Ok(resp) => {
                            if !resp.ok {
                                log::warn!(
                                    "index request failed: {}",
                                    resp.error.unwrap_or_default()
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!("failed to send index request to daemon: {e}");
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                log::warn!("watch error: {e}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                log::warn!("watcher channel disconnected");
                break;
            }
        }
    }

    Ok(())
}

/// Directories to watch recursively with inotify. Pure tsm.toml plumbing:
/// — if `content_dirs` is configured, watch those specific subdirs;
/// — otherwise watch `index_root` itself.
///
/// No policy consultation: this is scope-for-registration only. Events
/// from force-excluded or user-ignored paths will still arrive and be
/// filtered later by the daemon's `IngestPolicy`.
///
/// Takes `content_dirs` as a parameter rather than reading
/// `config::content_dirs()` internally so tests can exercise both
/// branches without touching the global `RESOLVED` singleton.
fn watch_targets(index_root: &Path, content_dirs: &[config::ContentDir]) -> Vec<PathBuf> {
    if content_dirs.is_empty() {
        vec![index_root.to_path_buf()]
    } else {
        content_dirs
            .iter()
            .map(|d| index_root.join(&d.path))
            .filter(|p| {
                let ok = p.is_dir();
                if !ok {
                    log::warn!("content_dir {} not found; will not be watched", p.display());
                }
                ok
            })
            .collect()
    }
}

/// Set up watch targets and return the set of watched directories.
fn setup_watches(
    debouncer: &mut notify_debouncer_mini::Debouncer<
        notify_debouncer_mini::notify::RecommendedWatcher,
    >,
    index_root: &Path,
) -> HashSet<PathBuf> {
    let mut watched = HashSet::new();
    let dirs = config::content_dirs();
    for full_dir in watch_targets(index_root, &dirs) {
        if let Err(e) = debouncer
            .watcher()
            .watch(&full_dir, RecursiveMode::Recursive)
        {
            log::warn!("cannot watch {}: {e}", full_dir.display());
        } else {
            watched.insert(full_dir);
        }
    }
    watched
}

/// Update watch targets: unwatch removed dirs, watch added dirs.
fn update_watches(
    debouncer: &mut notify_debouncer_mini::Debouncer<
        notify_debouncer_mini::notify::RecommendedWatcher,
    >,
    current: &mut HashSet<PathBuf>,
    index_root: &Path,
) {
    let dirs = config::content_dirs();
    let desired: HashSet<PathBuf> = watch_targets(index_root, &dirs).into_iter().collect();

    // Unwatch removed dirs
    for dir in current.difference(&desired) {
        log::info!("unwatching {}", dir.display());
        if let Err(e) = debouncer.watcher().unwatch(dir) {
            log::warn!("failed to unwatch {}: {e}", dir.display());
        }
    }

    // Watch new dirs (only include successfully watched dirs)
    let mut actually_watched: HashSet<PathBuf> = current.intersection(&desired).cloned().collect();
    for dir in desired.difference(current) {
        log::info!("watching {}", dir.display());
        if let Err(e) = debouncer.watcher().watch(dir, RecursiveMode::Recursive) {
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

    fn make_content_dir(path: &str) -> config::ContentDir {
        config::ContentDir {
            path: path.to_string(),
            weight: 1.0,
            half_life_days: 90.0,
        }
    }

    #[test]
    fn test_watch_targets_empty_content_dirs_returns_index_root() {
        // No content_dirs configured → watcher registers index_root itself.
        let dir = tempfile::TempDir::new().unwrap();
        let targets = watch_targets(dir.path(), &[]);
        assert_eq!(targets, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn test_watch_targets_with_content_dirs() {
        // Configured content_dirs → each resolved relative to index_root.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("daily")).unwrap();
        std::fs::create_dir(dir.path().join("company")).unwrap();

        let dirs = vec![make_content_dir("daily"), make_content_dir("company")];
        let targets = watch_targets(dir.path(), &dirs);

        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&dir.path().join("daily")));
        assert!(targets.contains(&dir.path().join("company")));
    }

    #[test]
    fn test_watch_targets_drops_nonexistent_content_dir() {
        // Nonexistent entries are filtered out; this prevents inotify
        // registration errors from fabricated paths in tsm.toml.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("exists")).unwrap();
        // "ghost" is not created on disk.

        let dirs = vec![make_content_dir("exists"), make_content_dir("ghost")];
        let targets = watch_targets(dir.path(), &dirs);

        assert_eq!(targets, vec![dir.path().join("exists")]);
    }

    #[test]
    #[serial_test::serial]
    fn test_run_watcher_registers_index_root() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();

        SHUTDOWN.store(true, Ordering::SeqCst);

        let _cwd = CwdGuard::change_to(dir.path());
        unsafe { std::env::set_var("TSM_INDEX_ROOT", dir.path().as_os_str()) };
        the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
            name: "test",
        })
        .ok();

        // Watcher should exit immediately due to SHUTDOWN
        // We test the individual pieces rather than run() which also inits logger
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Duration::from_secs(2), tx).unwrap();
        let watched = setup_watches(&mut debouncer, dir.path());
        assert!(!watched.is_empty());
        assert!(watched.contains(dir.path()));

        // The main loop condition checks SHUTDOWN
        assert!(SHUTDOWN.load(Ordering::SeqCst));

        // Verify recv_timeout returns Timeout (no events)
        assert!(rx.recv_timeout(Duration::from_millis(10)).is_err());

        SHUTDOWN.store(false, Ordering::SeqCst);
        unsafe { std::env::remove_var("TSM_INDEX_ROOT") };
    }
}
