use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::event::ModifyKind;
use notify::{recommended_watcher, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest};

use crate::SHUTDOWN;

/// Flag set by SIGHUP to trigger watch target reload.
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Quiet period a path must observe before its change is forwarded to the
/// daemon. Coalesces a burst of events for the same file into one index request.
const DEBOUNCE: Duration = Duration::from_secs(2);

extern "C" fn sighup_handler(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
}

/// Whether a notify event should trigger (re)indexing.
///
/// notify's inotify backend hardcodes `WatchMask::OPEN` (notify 8.x,
/// `inotify.rs`) with no config knob to disable it, so the daemon merely
/// *reading* a watched file to index it emits `Access(Open)` events — as do its
/// repeated reads of `tsm.toml` / `.tsmignore` during ingest-policy checks.
/// Forwarding those turned indexing into an infinite index→open→reindex loop
/// (#149). We therefore drop every `Access(_)` event (open/close/read) and
/// metadata-only changes (atime/permissions), keeping creation, content writes,
/// renames, and removals. A denylist — rather than an allowlist — preserves the
/// coarse `Any` / `Modify(Any)` events some backends (e.g. macOS FSEvents) emit.
fn is_index_relevant(kind: &EventKind) -> bool {
    !(kind.is_access() || matches!(kind, EventKind::Modify(ModifyKind::Metadata(_))))
}

/// Coalesces file-change events by relative path: a path is emitted once it has
/// stayed quiet for [`DEBOUNCE`]. Replaces notify-debouncer-mini, whose
/// `DebouncedEvent` collapsed every notify event kind into an opaque "changed"
/// signal — discarding exactly the kind information needed to filter out the
/// `Access(Open)` events behind the #149 feedback loop.
struct Debounce {
    /// Relative path → instant of its most recent event.
    pending: HashMap<String, Instant>,
    timeout: Duration,
}

impl Debounce {
    fn new(timeout: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            timeout,
        }
    }

    /// Record an event for `key` observed at `now`, refreshing its quiet timer.
    fn record_at(&mut self, key: String, now: Instant) {
        self.pending.insert(key, now);
    }

    /// Drain and return every key quiet for `>= timeout` as of `now`, sorted for
    /// deterministic output. Keys still within their quiet window are retained.
    fn flush_ready(&mut self, now: Instant) -> Vec<String> {
        let mut ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, &t)| now.duration_since(t) >= self.timeout)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &ready {
            self.pending.remove(k);
        }
        ready.sort();
        ready
    }

    /// Earliest instant at which some pending key becomes ready, if any.
    fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|&t| t + self.timeout).min()
    }
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
    // reaches our debounce, so `Access(Open)` noise (the #149 loop) never enters
    // the pipeline. Surviving events and watch errors are forwarded over the
    // channel; the main loop debounces and dispatches them.
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<Event, notify::Error>>();
    let mut watcher: RecommendedWatcher = recommended_watcher(
        move |res: std::result::Result<Event, notify::Error>| match res {
            Ok(event) if is_index_relevant(&event.kind) => {
                let _ = tx.send(Ok(event));
            }
            Ok(_) => {} // drop Access / metadata-only events (#149)
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        },
    )
    .context("Failed to create file watcher")?;

    // The watcher's scope comes purely from `project_root` + `content_dirs`
    // — it does NOT consult `.tsmignore`, `extensions`, or `respect_gitignore`.
    // Those are policy concerns owned by the daemon's indexer via
    // `IngestPolicy`. Keeping the watcher oblivious means the event stream
    // is independent of user policy edits (no SIGHUP needed to pick up
    // `.tsmignore` changes — the indexer's next gate call does that).
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

    while !SHUTDOWN.load(Ordering::SeqCst) {
        // Handle reload requests (SIGHUP from tsmd)
        if RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
            log::info!("reload notification received, updating watch targets");
            config::reload();
            // Only `content_dirs` can change the watch scope; a new
            // `.tsmignore` would not affect registration (the watcher
            // doesn't know about it by design).
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
                    match path.strip_prefix(&project_root) {
                        Ok(rel) => debounce.record_at(rel.to_string_lossy().into_owned(), now),
                        Err(_) => {
                            log::warn!("path {} outside project root, skipping", path.display());
                        }
                    }
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

/// Directories to watch recursively with inotify. Pure tsm.toml plumbing:
/// — if `content_dirs` is configured, watch those specific subdirs;
/// — otherwise watch `project_root` itself.
///
/// No policy consultation: this is scope-for-registration only. Events
/// from force-excluded or user-ignored paths will still arrive and be
/// filtered later by the daemon's `IngestPolicy`.
///
/// Takes `content_dirs` as a parameter rather than reading
/// `config::content_dirs()` internally so tests can exercise both
/// branches without touching the global `RESOLVED` singleton.
fn watch_targets(project_root: &Path, content_dirs: &[config::ContentDir]) -> Vec<PathBuf> {
    if content_dirs.is_empty() {
        vec![project_root.to_path_buf()]
    } else {
        content_dirs
            .iter()
            .map(|d| project_root.join(&d.path))
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

    // Unwatch removed dirs
    for dir in current.difference(&desired) {
        log::info!("unwatching {}", dir.display());
        if let Err(e) = watcher.unwatch(dir) {
            log::warn!("failed to unwatch {}: {e}", dir.display());
        }
    }

    // Watch new dirs (only include successfully watched dirs)
    let mut actually_watched: HashSet<PathBuf> = current.intersection(&desired).cloned().collect();
    for dir in desired.difference(current) {
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
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode,
    };
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

    // ── Root-cause fix: event-kind filter (#149) ──────────────────────
    // The daemon's reads of watched files (and of tsm.toml / .tsmignore on
    // every ingest-policy check) surface as `Access(Open)` events under
    // notify's inotify backend. Forwarding them caused an infinite
    // index→open→reindex loop. These tests pin the predicate that breaks it.

    #[test]
    fn test_access_open_is_not_index_relevant() {
        // The exact kind observed dominating the #149 feedback loop.
        let kind = EventKind::Access(AccessKind::Open(AccessMode::Any));
        assert!(!is_index_relevant(&kind), "Access(Open) must be dropped");
    }

    #[test]
    fn test_access_close_and_read_are_not_index_relevant() {
        for kind in [
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Any),
        ] {
            assert!(!is_index_relevant(&kind), "{kind:?} must be dropped");
        }
    }

    #[test]
    fn test_metadata_only_change_is_not_index_relevant() {
        // atime / permission changes must not trigger a content reindex.
        let kind = EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any));
        assert!(
            !is_index_relevant(&kind),
            "Modify(Metadata) must be dropped"
        );
    }

    #[test]
    fn test_content_events_are_index_relevant() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            EventKind::Remove(RemoveKind::File),
            // Coarse catch-alls some backends emit must be kept (denylist).
            EventKind::Any,
            EventKind::Modify(ModifyKind::Any),
        ] {
            assert!(is_index_relevant(&kind), "{kind:?} must be forwarded");
        }
    }

    // ── Debounce coalescing ───────────────────────────────────────────

    #[test]
    fn test_debounce_holds_until_quiet_then_flushes() {
        let mut d = Debounce::new(Duration::from_secs(2));
        let t0 = Instant::now();
        d.record_at("notes/a.md".to_string(), t0);

        // Still within the quiet window → nothing ready.
        assert!(d.flush_ready(t0 + Duration::from_millis(1900)).is_empty());
        // Past the window → the key drains exactly once.
        assert_eq!(
            d.flush_ready(t0 + Duration::from_secs(2)),
            vec!["notes/a.md".to_string()]
        );
        assert!(d.flush_ready(t0 + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn test_debounce_refreshes_quiet_timer_on_repeat() {
        // A second event for the same path extends its quiet window: the path
        // is forwarded once, after the *last* event settles.
        let mut d = Debounce::new(Duration::from_secs(2));
        let t0 = Instant::now();
        d.record_at("x.md".to_string(), t0);
        d.record_at("x.md".to_string(), t0 + Duration::from_secs(1));

        assert!(d.flush_ready(t0 + Duration::from_secs(2)).is_empty());
        assert_eq!(
            d.flush_ready(t0 + Duration::from_secs(3)),
            vec!["x.md".to_string()]
        );
    }

    #[test]
    fn test_debounce_flushes_only_ready_keys_and_sorts() {
        let mut d = Debounce::new(Duration::from_secs(2));
        let t0 = Instant::now();
        d.record_at("b.md".to_string(), t0);
        d.record_at("a.md".to_string(), t0);
        d.record_at("late.md".to_string(), t0 + Duration::from_secs(1));

        // At t0+2s the first two are ready (sorted), `late.md` is not.
        assert_eq!(
            d.flush_ready(t0 + Duration::from_secs(2)),
            vec!["a.md".to_string(), "b.md".to_string()]
        );
        assert_eq!(
            d.flush_ready(t0 + Duration::from_secs(3)),
            vec!["late.md".to_string()]
        );
    }

    #[test]
    fn test_debounce_next_deadline() {
        let mut d = Debounce::new(Duration::from_secs(2));
        assert!(d.next_deadline().is_none());
        let t0 = Instant::now();
        d.record_at("a".to_string(), t0);
        assert_eq!(d.next_deadline(), Some(t0 + Duration::from_secs(2)));
    }

    fn make_content_dir(path: &str) -> config::ContentDir {
        config::ContentDir {
            path: path.to_string(),
            weight: 1.0,
            half_life_days: 90.0,
        }
    }

    #[test]
    fn test_watch_targets_empty_content_dirs_returns_project_root() {
        // No content_dirs configured → watcher registers project_root itself.
        let dir = tempfile::TempDir::new().unwrap();
        let targets = watch_targets(dir.path(), &[]);
        assert_eq!(targets, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn test_watch_targets_with_content_dirs() {
        // Configured content_dirs → each resolved relative to project_root.
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
