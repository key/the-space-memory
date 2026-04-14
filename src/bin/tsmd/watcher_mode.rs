use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest};
use the_space_memory::indexer::ContentWalker;

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

    // Single walker instance governs both directory-registration and
    // event-filtering so both views cannot drift under config reload.
    let mut walker = ContentWalker::from_env();
    let mut watched = setup_watches(&mut debouncer, &walker);

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
            // Rebuild the walker so .tsmignore / .gitignore / extensions
            // edits take effect without a daemon restart.
            walker = ContentWalker::from_env();
            update_watches(&mut debouncer, &mut watched, &walker);
            log::info!("now watching {} directories", watched.len());
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                let mut files_to_index: HashSet<String> = HashSet::new();
                for event in &events {
                    if event.kind != DebouncedEventKind::Any {
                        continue;
                    }
                    // Single predicate replaces the old hard-coded .md check.
                    // Ignore rules + extension allowlist now run through one
                    // code path shared with the full-index walker.
                    if walker.is_ignored(&event.path) || !walker.extension_allowed(&event.path) {
                        continue;
                    }
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

/// Set up watch targets and return the set of watched directories.
fn setup_watches(
    debouncer: &mut notify_debouncer_mini::Debouncer<
        notify_debouncer_mini::notify::RecommendedWatcher,
    >,
    walker: &ContentWalker,
) -> HashSet<PathBuf> {
    let mut watched = HashSet::new();
    for full_dir in walker.watch_dirs() {
        if full_dir.is_dir() {
            if let Err(e) = debouncer
                .watcher()
                .watch(&full_dir, RecursiveMode::Recursive)
            {
                log::warn!("cannot watch {}: {e}", full_dir.display());
            } else {
                watched.insert(full_dir);
            }
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
    walker: &ContentWalker,
) {
    let desired: HashSet<PathBuf> = walker
        .watch_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .collect();

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
    fn test_run_watcher_no_content_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        // Override index root to the empty temp dir
        unsafe { std::env::set_var("TSM_INDEX_ROOT", dir.path().as_os_str()) };
        the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
            name: "test",
        })
        .ok();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Duration::from_secs(2), tx).unwrap();
        let walker = ContentWalker::from_env();
        let watched = setup_watches(&mut debouncer, &walker);
        assert!(watched.is_empty());

        unsafe { std::env::remove_var("TSM_INDEX_ROOT") };
    }

    #[test]
    #[serial_test::serial]
    fn test_run_watcher_shutdown() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();

        SHUTDOWN.store(true, Ordering::SeqCst);

        unsafe { std::env::set_var("TSM_INDEX_ROOT", dir.path().as_os_str()) };
        the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
            name: "test",
        })
        .ok();

        // Watcher should exit immediately due to SHUTDOWN
        // We test the individual pieces rather than run() which also inits logger
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Duration::from_secs(2), tx).unwrap();
        let walker = ContentWalker::from_env();
        let watched = setup_watches(&mut debouncer, &walker);
        assert!(!watched.is_empty());

        // The main loop condition checks SHUTDOWN
        assert!(SHUTDOWN.load(Ordering::SeqCst));

        // Verify recv_timeout returns Timeout (no events)
        assert!(rx.recv_timeout(Duration::from_millis(10)).is_err());

        SHUTDOWN.store(false, Ordering::SeqCst);
        unsafe { std::env::remove_var("TSM_INDEX_ROOT") };
    }

    /// The event loop in `run()` drops an event iff
    /// `walker.is_ignored(path) || !walker.extension_allowed(path)`. Verify
    /// that predicate directly against realistic paths — covering the
    /// .tsmignore, forced-exclude, and extension-allowlist legs of the OR.
    #[test]
    #[serial_test::serial]
    fn test_event_filter_predicate_matches_implementation() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("daily")).unwrap();
        std::fs::create_dir_all(dir.path().join("private")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".tsmignore"), "private/\n").unwrap();

        std::env::set_current_dir(dir.path()).unwrap();
        unsafe { std::env::set_var("TSM_INDEX_ROOT", dir.path().as_os_str()) };
        let walker = ContentWalker::from_env();

        // Helper mirroring the condition at the top of the event loop.
        let drop_event =
            |p: &std::path::Path| -> bool { walker.is_ignored(p) || !walker.extension_allowed(p) };

        assert!(
            !drop_event(&dir.path().join("daily/keep.md")),
            "normal .md under a non-ignored dir must pass the event filter"
        );
        assert!(
            drop_event(&dir.path().join("private/secret.md")),
            ".tsmignore-excluded path must be dropped by the event filter"
        );
        assert!(
            drop_event(&dir.path().join(".git/HEAD.md")),
            "forced-excluded dir must be dropped by the event filter"
        );
        assert!(
            drop_event(&dir.path().join("daily/notes.csv")),
            "extension outside [index].extensions must be dropped"
        );

        unsafe { std::env::remove_var("TSM_INDEX_ROOT") };
    }
}
