//! Public accessor + derived-path helpers that read the resolved config
//! singleton. Split out of `config.rs` so each file stays within the ADR-0018
//! per-file line gate; re-exported by `config` (`pub use`), so callers keep
//! using `config::<name>()`.

use std::path::PathBuf;

use crate::config::{resolved, ContentDir, LinkMode, SearchFallback};

// ─── Accessor functions (delegate to ResolvedConfig singleton) ───

pub fn state_dir() -> PathBuf {
    resolved().state_dir.clone()
}

pub fn cache_dir() -> PathBuf {
    resolved().cache_dir.clone()
}

pub fn embedder_socket_path() -> PathBuf {
    resolved().embedder_socket_path.clone()
}

pub fn daemon_socket_path() -> PathBuf {
    resolved().daemon_socket_path.clone()
}

pub fn log_dir() -> PathBuf {
    resolved().log_dir.clone()
}

pub fn embedder_idle_timeout_secs() -> u64 {
    resolved().embedder_idle_timeout_secs
}

pub fn embedder_backfill_interval_secs() -> u64 {
    resolved().embedder_backfill_interval_secs
}

pub fn reader_pool_size() -> usize {
    resolved().reader_pool_size
}

pub fn reindex_fts_batch_size() -> usize {
    resolved().reindex_fts_batch_size
}

/// Per-document chunk cap for the search result window (#299); `0` disables it.
pub fn max_chunks_per_document() -> usize {
    resolved().max_chunks_per_document
}

pub fn search_fallback() -> SearchFallback {
    resolved().search_fallback
}

pub fn content_dirs() -> Vec<ContentDir> {
    resolved().content_dirs
}

pub fn session_weight() -> f64 {
    resolved().session_weight
}

pub fn session_half_life_days() -> f64 {
    resolved().session_half_life_days
}

pub fn respect_gitignore() -> bool {
    resolved().respect_gitignore
}

pub fn ignore_file() -> String {
    resolved().ignore_file.clone()
}

pub fn index_extensions() -> Vec<String> {
    resolved().extensions.clone()
}

pub fn project_root() -> PathBuf {
    resolved().project_root.clone()
}

/// Return the legacy `index_root` value captured from the config file, if any.
///
/// When `Some`, a config file still uses the removed `index_root` key.
/// Callers are responsible for handling this condition:
/// - CLI startup: hard-exit with a migration message.
/// - Daemon startup: `anyhow::bail!` before socket bind.
/// - Config reload: push a warning and keep running.
pub fn legacy_index_root() -> Option<PathBuf> {
    resolved().rejected_index_root.clone()
}

// ─── Derived paths ───────────────────────────────────────────────

pub fn db_path() -> PathBuf {
    state_dir().join("tsm.db")
}

pub fn user_dict_path() -> PathBuf {
    resolved().user_dict_path.clone()
}

pub fn setup_link_mode() -> LinkMode {
    resolved().setup_link_mode
}

pub fn init_link_mode() -> LinkMode {
    resolved().init_link_mode
}

pub fn custom_terms_path() -> PathBuf {
    state_dir().join("custom_terms.toml")
}

pub fn stopwords_path() -> PathBuf {
    state_dir().join("stopwords.txt")
}

pub fn reject_words_path() -> PathBuf {
    state_dir().join("reject_words.txt")
}

pub fn wordnet_db_path() -> PathBuf {
    state_dir().join("wnjpn.db")
}

pub fn user_synonyms_path() -> PathBuf {
    state_dir().join("synonyms.csv")
}

pub fn daemon_pid_path() -> PathBuf {
    state_dir().join("tsmd.pid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_derived_paths_have_stable_filenames() {
        let sd = state_dir();
        // Each derived path is `state_dir()/<fixed name>` regardless of where
        // state_dir resolves, so assert the suffix (deterministic) and the root.
        for (path, name) in [
            (db_path(), "tsm.db"),
            (custom_terms_path(), "custom_terms.toml"),
            (stopwords_path(), "stopwords.txt"),
            (reject_words_path(), "reject_words.txt"),
            (wordnet_db_path(), "wnjpn.db"),
            (user_synonyms_path(), "synonyms.csv"),
            (daemon_pid_path(), "tsmd.pid"),
        ] {
            assert!(path.starts_with(&sd), "{path:?} not under state_dir {sd:?}");
            assert_eq!(path.file_name().unwrap(), name);
        }
    }

    #[test]
    fn delegating_accessors_execute() {
        // Smoke-exercise the thin `resolved()` delegators so the wrappers are
        // covered; values come from the resolved-config singleton.
        let _ = cache_dir();
        let _ = embedder_socket_path();
        let _ = daemon_socket_path();
        let _ = log_dir();
        let _ = user_dict_path();
        let _ = project_root();
        let _ = legacy_index_root();
        let _ = content_dirs();
        let _ = ignore_file();
        let _ = index_extensions();
        let _ = embedder_idle_timeout_secs();
        let _ = embedder_backfill_interval_secs();
        let _ = reader_pool_size();
        let _ = reindex_fts_batch_size();
        let _ = max_chunks_per_document();
        let _ = search_fallback();
        let _ = session_weight();
        let _ = session_half_life_days();
        let _ = respect_gitignore();
        let _ = setup_link_mode();
        let _ = init_link_mode();
    }
}
