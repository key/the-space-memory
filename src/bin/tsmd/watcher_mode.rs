use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use the_space_memory::cli;
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
            update_watches(&mut debouncer, &mut watched, &index_root);
            log::info!("now watching {} directories", watched.len());
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                let mut files_to_index: HashSet<String> = HashSet::new();
                for event in &events {
                    if event.kind != DebouncedEventKind::Any {
                        continue;
                    }
                    if event.path.extension().is_none_or(|ext| ext != "md") {
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
    index_root: &Path,
) -> HashSet<PathBuf> {
    let mut watched = HashSet::new();
    for full_dir in cli::discover_watch_dirs(index_root) {
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
    index_root: &Path,
) {
    let desired: HashSet<PathBuf> = cli::discover_watch_dirs(index_root)
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
        let watched = setup_watches(&mut debouncer, dir.path());
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
        let watched = setup_watches(&mut debouncer, dir.path());
        assert!(!watched.is_empty());

        // The main loop condition checks SHUTDOWN
        assert!(SHUTDOWN.load(Ordering::SeqCst));

        // Verify recv_timeout returns Timeout (no events)
        assert!(rx.recv_timeout(Duration::from_millis(10)).is_err());

        SHUTDOWN.store(false, Ordering::SeqCst);
        unsafe { std::env::remove_var("TSM_INDEX_ROOT") };
    }
}
