//! Cache of surfaces already accepted into `user_dict.simpledic`, used to
//! filter out already-known words when collecting new dictionary candidates.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::config;

/// `None` is stale: never loaded, or invalidated by `reset_existing_surfaces`.
/// The next access reloads from disk and repopulates it.
static SURFACES: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// Bumped by every `reset_existing_surfaces` call. A reload in flight when a
/// reset lands captures the generation before reloading, and only writes its
/// result back if the generation is still current — see `with_existing_surfaces`.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Invalidate the cache so the next lookup reloads `user_dict.simpledic`. The
/// dict changes at runtime without a daemon restart, so call this wherever the
/// segmenter is also reset — otherwise a long-lived daemon keeps re-collecting
/// already-accepted words as candidates.
pub fn reset_existing_surfaces() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    *SURFACES.write().expect("existing surfaces lock poisoned") = None;
}

pub(super) fn with_existing_surfaces<T>(f: impl FnOnce(&HashSet<String>) -> T) -> T {
    let guard = SURFACES.read().expect("existing surfaces lock poisoned");
    if let Some(s) = guard.as_ref() {
        return f(s);
    }
    drop(guard);
    let generation = GENERATION.load(Ordering::SeqCst);
    let loaded = load_existing_surfaces(&config::user_dict_path())
        .inspect_err(|e| log::warn!("could not read user dict: {e}"))
        .unwrap_or_default();
    let result = f(&loaded);
    store_if_current(generation, loaded);
    result
}

/// Cache `loaded` only if no reset happened since `generation` was captured
/// (right before the reload started). Otherwise a reset landing while this
/// reload was in flight would be silently undone by this now-stale
/// write-back — the exact bug this generation check exists to prevent.
fn store_if_current(generation: u64, loaded: HashSet<String>) {
    if GENERATION.load(Ordering::SeqCst) == generation {
        *SURFACES.write().expect("existing surfaces lock poisoned") = Some(loaded);
    }
}

/// Load existing surface forms from a user dictionary file (IPAdic format).
/// The first column (comma-separated) is the surface form.
pub fn load_existing_surfaces(csv_path: &Path) -> anyhow::Result<HashSet<String>> {
    let mut surfaces = HashSet::new();
    if !csv_path.exists() {
        return Ok(surfaces);
    }
    let content = std::fs::read_to_string(csv_path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(surface) = line.split(',').next() {
            let surface = crate::normalize::nfc_lower(surface);
            if !surface.is_empty() {
                surfaces.insert(surface);
            }
        }
    }
    Ok(surfaces)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── load_existing_surfaces tests ────────────────────────

    #[test]
    fn test_load_existing_surfaces_missing_file() {
        let result = load_existing_surfaces(Path::new("/nonexistent/dict.csv")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_existing_surfaces_reads_csv() {
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("dict.csv");
        std::fs::write(&csv_path, "candle,名詞,candle\nlindera,名詞,lindera\n").unwrap();

        let surfaces = load_existing_surfaces(&csv_path).unwrap();
        assert!(surfaces.contains("candle"));
        assert!(surfaces.contains("lindera"));
        assert_eq!(surfaces.len(), 2);
    }

    #[test]
    fn test_load_existing_surfaces_skips_comments_and_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("dict.csv");
        std::fs::write(&csv_path, "# comment\n\ncandle,名詞,candle\n").unwrap();

        let surfaces = load_existing_surfaces(&csv_path).unwrap();
        assert_eq!(surfaces.len(), 1);
        assert!(surfaces.contains("candle"));
    }

    /// An NFD-encoded surface (decomposed dakuten) in the CSV must be stored
    /// under its NFC form, so an NFC-typed lookup matches it.
    #[test]
    fn test_load_existing_surfaces_normalizes_nfd_to_nfc() {
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("dict.csv");
        let nfd_worker = "\u{30ef}\u{30fc}\u{30ab}\u{3099}\u{30fc}"; // ワーガー decomposed
        std::fs::write(&csv_path, format!("{nfd_worker},名詞,{nfd_worker}\n")).unwrap();

        let surfaces = load_existing_surfaces(&csv_path).unwrap();

        assert!(surfaces.contains("\u{30ef}\u{30fc}\u{30ac}\u{30fc}")); // ワーガー precomposed
    }

    /// The dict changes at runtime (`dict add`/`reject`/`rm`) without a daemon
    /// restart. Before `reset_existing_surfaces`, `with_existing_surfaces` must
    /// keep returning the stale cached content; after, it must reload and
    /// reflect the new file.
    #[test]
    #[serial_test::serial]
    fn test_reset_existing_surfaces_reflects_new_content() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", dir.path());
        config::reload();
        reset_existing_surfaces(); // clean slate: another test may have warmed the cache

        std::fs::write(config::user_dict_path(), "candle,名詞,candle\n").unwrap();
        assert!(with_existing_surfaces(|s| s.contains("candle")));

        std::fs::write(config::user_dict_path(), "lindera,名詞,lindera\n").unwrap();
        assert!(
            with_existing_surfaces(|s| s.contains("candle")),
            "cache must still be stale before reset"
        );

        reset_existing_surfaces();

        assert!(
            with_existing_surfaces(|s| s.contains("lindera")),
            "after reset, the new file content must be reflected"
        );
        assert!(!with_existing_surfaces(|s| s.contains("candle")));

        std::env::remove_var("TSM_STATE_DIR");
        config::reload();
    }

    // ─── generation-counter (lost-update) regression ─────────

    /// A reload started before a concurrent `reset_existing_surfaces` must NOT
    /// overwrite the reset with its now-stale result. Without the generation
    /// check, `store_if_current` would store unconditionally and silently
    /// resurrect the pre-reset content — this test fails against that
    /// unconditional-store implementation.
    #[test]
    #[serial_test::serial]
    fn test_store_if_current_discards_stale_write_after_concurrent_reset() {
        reset_existing_surfaces();
        let generation_before_reset = GENERATION.load(Ordering::SeqCst);

        reset_existing_surfaces(); // simulates a reset landing mid-reload

        let mut stale = HashSet::new();
        stale.insert("stale".to_string());
        store_if_current(generation_before_reset, stale);

        assert!(
            SURFACES.read().unwrap().is_none(),
            "a write-back captured before a concurrent reset must not resurrect stale content"
        );
    }

    /// The normal (no concurrent reset) path must still cache the loaded set.
    #[test]
    #[serial_test::serial]
    fn test_store_if_current_stores_when_generation_unchanged() {
        reset_existing_surfaces();
        let generation = GENERATION.load(Ordering::SeqCst);

        let mut fresh = HashSet::new();
        fresh.insert("fresh".to_string());
        store_if_current(generation, fresh);

        assert!(SURFACES.read().unwrap().as_ref().unwrap().contains("fresh"));
    }
}
