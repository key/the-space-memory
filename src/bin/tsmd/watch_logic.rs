//! Pure, unit-testable logic for the fs-watcher: event-kind relevance, debounce
//! coalescing, and watch-target computation. Kept separate from `watcher_proc`
//! (the notify event loop and watch registration glue) so this logic is covered
//! by the coverage gate while the I/O shell stays excluded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::EventKind;

use the_space_memory::config;

/// Whether a notify event should trigger (re)indexing.
///
/// notify's inotify backend hardcodes `WatchMask::OPEN` (notify 8.x,
/// `inotify.rs`) with no config knob to disable it, so the daemon merely
/// *reading* a watched file to index it emits `Access(Open)` events — as do its
/// repeated reads of `tsm.toml` / `.tsmignore` during ingest-policy checks.
/// Forwarding those turned indexing into an infinite index→open→reindex loop.
/// We therefore drop the access events the daemon's reads emit (`Open`,
/// `Read`, `Close(Read)`) and metadata-only changes (atime/permissions), while
/// keeping creation, content writes, renames, and removals.
///
/// `Access(Close(Write))` (inotify `IN_CLOSE_WRITE`) is the one access event we
/// keep: it is the canonical "a writer finished and closed the file" signal. The
/// daemon opens files read-only, so its reads never emit `Close(Write)` — only
/// `Open` / `Close(Read)` — so keeping it cannot reopen the loop.
///
/// This is a denylist (drop specific access/metadata kinds, keep the rest)
/// rather than an allowlist, so coarse `Any` / `Modify(Any)` events that some
/// backends (e.g. macOS FSEvents) emit are preserved.
pub(crate) fn is_index_relevant(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        _ => true,
    }
}

/// Whether `path`'s extension is in the watcher's index-extension allowlist.
///
/// Mirrors `ContentWalker::extension_allowed`'s allowlist semantics (also
/// rejecting extension-less paths) without depending on the indexer's
/// `pub(crate)` internals, which are not visible across the binary/library
/// crate boundary. Duplicating this one predicate is preferred over widening
/// `ContentWalker`'s visibility or running the indexer's full ignore-rule
/// matching here: the watcher intentionally stays oblivious to `.tsmignore`
/// (so ignore-rule edits take effect without a watcher reload), and this
/// filter exists purely to drop obviously-irrelevant events — build
/// artifacts, object files, anything outside the index extension allowlist —
/// before they enter the debounce map and cross the IPC boundary. The
/// indexer's `IngestPolicy::accepts` remains the sole correctness authority
/// for what actually gets indexed; this is a caller-side optimization only.
pub(crate) fn extension_allowed(extensions: &[String], path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => extensions.iter().any(|e| e == ext),
        None => false,
    }
}

/// Coalesces file-change events by relative path: a path is emitted once it has
/// stayed quiet for its timeout. Replaces notify-debouncer-mini, whose
/// `DebouncedEvent` collapsed every notify event kind into an opaque "changed"
/// signal — discarding exactly the kind information needed to filter out the
/// `Access(Open)` events that caused the feedback loop.
pub(crate) struct Debounce {
    /// Relative path → instant of its most recent event.
    pending: HashMap<String, Instant>,
    timeout: Duration,
}

impl Debounce {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            timeout,
        }
    }

    /// Record an event for `key` observed at `now`, refreshing its quiet timer.
    pub(crate) fn record_at(&mut self, key: String, now: Instant) {
        self.pending.insert(key, now);
    }

    /// Drain and return every key quiet for `>= timeout` as of `now`, sorted for
    /// deterministic output. Keys still within their quiet window are retained.
    pub(crate) fn flush_ready(&mut self, now: Instant) -> Vec<String> {
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
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|&t| t + self.timeout).min()
    }
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
pub(crate) fn watch_targets(
    project_root: &Path,
    content_dirs: &[config::ContentDir],
) -> Vec<PathBuf> {
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

/// The watch-set transition computed by [`diff_watch_set`]: which directories to
/// unwatch, which to newly watch, and which are already watched and kept.
pub(crate) struct WatchDiff {
    /// In `current` but no longer `desired` — unwatch these.
    pub(crate) to_unwatch: Vec<PathBuf>,
    /// In `desired` but not yet in `current` — watch these.
    pub(crate) to_watch: Vec<PathBuf>,
    /// In both `current` and `desired` — already watched, keep as-is.
    pub(crate) kept: HashSet<PathBuf>,
}

/// Pure set diff between the currently-watched directories and the desired set:
/// splits them into unwatch / watch / kept. The caller applies the watch and
/// unwatch I/O (which can fail per directory), so this stays free of the notify
/// `Watcher` and is unit-testable on its own.
pub(crate) fn diff_watch_set(current: &HashSet<PathBuf>, desired: &HashSet<PathBuf>) -> WatchDiff {
    WatchDiff {
        to_unwatch: current.difference(desired).cloned().collect(),
        to_watch: desired.difference(current).cloned().collect(),
        kept: current.intersection(desired).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };

    // ── Root-cause fix: event-kind filter ────────────────────────────
    // The daemon's reads of watched files (and of tsm.toml / .tsmignore on
    // every ingest-policy check) surface as `Access(Open)` events under
    // notify's inotify backend. Forwarding them caused an infinite
    // index→open→reindex loop. These tests pin the predicate that breaks it.

    #[test]
    fn test_access_open_is_not_index_relevant() {
        // The exact kind observed dominating the feedback loop.
        let kind = EventKind::Access(AccessKind::Open(AccessMode::Any));
        assert!(!is_index_relevant(&kind), "Access(Open) must be dropped");
    }

    #[test]
    fn test_read_side_access_events_are_not_index_relevant() {
        // The access events the daemon's read-only indexing emits must be
        // dropped — these are what drove the feedback loop.
        for kind in [
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Any),
        ] {
            assert!(!is_index_relevant(&kind), "{kind:?} must be dropped");
        }
    }

    #[test]
    fn test_close_write_is_index_relevant() {
        // IN_CLOSE_WRITE is the canonical write-completion signal and is only
        // emitted by writers, never by the daemon's read-only opens — so it is
        // kept while every other Access(_) is dropped.
        let kind = EventKind::Access(AccessKind::Close(AccessMode::Write));
        assert!(
            is_index_relevant(&kind),
            "Access(Close(Write)) must be forwarded"
        );
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

    // ── Extension pre-filter ──────────────────────────────────────────

    #[test]
    fn test_extension_allowed_matches_allowlist() {
        let exts = vec!["md".to_string()];
        assert!(extension_allowed(&exts, Path::new("notes/a.md")));
    }

    #[test]
    fn test_extension_allowed_rejects_unlisted_extension() {
        // The exact shape of the incident noise: cargo build artifacts under
        // a watched external content_dir.
        let exts = vec!["md".to_string()];
        assert!(!extension_allowed(
            &exts,
            Path::new("the-space-memory/target/debug/build-script-build")
        ));
        assert!(!extension_allowed(
            &exts,
            Path::new("the-space-memory/target/debug/deps/foo.rcgu.o")
        ));
    }

    #[test]
    fn test_extension_allowed_rejects_no_extension() {
        let exts = vec!["md".to_string()];
        assert!(!extension_allowed(&exts, Path::new("notes/README")));
    }

    #[test]
    fn test_extension_allowed_honors_multiple_configured_extensions() {
        let exts = vec!["md".to_string(), "txt".to_string()];
        assert!(extension_allowed(&exts, Path::new("a.txt")));
        assert!(extension_allowed(&exts, Path::new("a.md")));
        assert!(!extension_allowed(&exts, Path::new("a.rs")));
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

    // ── Watch-set diff ─────────────────────────────────────────────────

    fn set(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
        v.sort();
        v
    }

    #[test]
    fn test_diff_watch_set_all_new() {
        // Empty current → everything desired is to_watch; nothing kept/unwatched.
        let diff = diff_watch_set(&set(&[]), &set(&["/a", "/b"]));
        assert_eq!(
            sorted(diff.to_watch),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(diff.to_unwatch.is_empty());
        assert!(diff.kept.is_empty());
    }

    #[test]
    fn test_diff_watch_set_unchanged_keeps_all() {
        // current == desired → nothing to watch/unwatch; all kept.
        let diff = diff_watch_set(&set(&["/a", "/b"]), &set(&["/a", "/b"]));
        assert!(diff.to_watch.is_empty());
        assert!(diff.to_unwatch.is_empty());
        assert_eq!(diff.kept, set(&["/a", "/b"]));
    }

    #[test]
    fn test_diff_watch_set_partial_overlap() {
        // /a kept, /b removed (unwatch), /c added (watch).
        let diff = diff_watch_set(&set(&["/a", "/b"]), &set(&["/a", "/c"]));
        assert_eq!(sorted(diff.to_watch), vec![PathBuf::from("/c")]);
        assert_eq!(sorted(diff.to_unwatch), vec![PathBuf::from("/b")]);
        assert_eq!(diff.kept, set(&["/a"]));
    }

    #[test]
    fn test_diff_watch_set_all_removed() {
        // Empty desired → everything current is to_unwatch; nothing kept.
        let diff = diff_watch_set(&set(&["/a", "/b"]), &set(&[]));
        assert!(diff.to_watch.is_empty());
        assert_eq!(
            sorted(diff.to_unwatch),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(diff.kept.is_empty());
    }
}
