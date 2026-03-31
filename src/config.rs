use std::path::PathBuf;
use std::sync::OnceLock;

use directories::ProjectDirs;

// ─── Internal constants (not user-configurable) ──────────────────

pub const MAX_CHUNK_CHARS: usize = 800;
pub const RRF_K: f64 = 60.0;
pub const SCORE_THRESHOLD: f64 = 0.005;
pub const MAX_RESULTS: usize = 5;
pub const EMBEDDING_DIM: usize = 256;
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 90.0;
pub const SNIPPET_MAX_CHARS: usize = 200;
pub const MIN_SESSION_MESSAGE_LEN: usize = 10;
pub const BACKFILL_BATCH_SIZE: usize = 8;
pub const MAX_QUERY_EXPANSIONS: usize = 5;
pub const RECENT_DAYS: i64 = 30;
pub const DICT_CANDIDATE_FREQ_THRESHOLD: i64 = 5;
pub const WORKER_ENCODE_TIMEOUT_PER_ITEM_SECS: u64 = 5;
pub const WORKER_ENCODE_TIMEOUT_BASE_SECS: u64 = 10;
pub const MAX_WORKER_RESTARTS: usize = 3;
pub const MIN_QUERY_KEYWORDS: usize = 1;

const DEFAULT_DATA_DIR: &str = ".tsm";
const DEFAULT_PROJECT_ROOT: &str = "/workspaces";
const DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS: u64 = 600;
const DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS: u64 = 300;

/// Content directories with score weights. (directory, weight)
pub const CONTENT_DIRS: &[(&str, f64)] = &[
    // daily
    ("daily/notes", 1.0),
    ("daily/daily/research", 1.1),
    ("daily/daily/intel", 0.7),
    // company
    ("company/knowledge", 1.3),
    ("company/ideas", 0.9),
    ("company/updates", 0.8),
    ("company/research", 1.2),
    ("company/products", 1.2),
    ("company/decisions", 1.1),
    ("company/retrospectives", 0.9),
];
pub const SESSION_WEIGHT: f64 = 0.3;

// ─── Config struct ───────────────────────────────────────────────

/// Shape of tsm.toml — all fields optional for partial config files.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ConfigFile {
    data_dir: Option<PathBuf>,
    project_root: Option<PathBuf>,
    embedder_socket_path: Option<PathBuf>,
    daemon_socket_path: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    embedder_idle_timeout_secs: Option<u64>,
    embedder_backfill_interval_secs: Option<u64>,
}

static CONFIG: OnceLock<ConfigFile> = OnceLock::new();

/// Get the lazily-loaded config singleton.
fn cfg() -> &'static ConfigFile {
    CONFIG.get_or_init(|| load_config_from(&config_file_candidates()))
}

/// Merge config values from `candidates` in order; first non-None value for each field wins.
fn load_config_from(candidates: &[PathBuf]) -> ConfigFile {
    // Determine which path was explicitly requested via TSM_CONFIG (if any)
    let explicit_config = std::env::var_os("TSM_CONFIG").map(PathBuf::from);

    let mut merged = ConfigFile::default();

    // Iterate in priority order (highest first); `.or()` keeps first-seen value
    for path in candidates {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                if explicit_config.as_deref() == Some(path.as_path()) {
                    log::error!(
                        "Cannot read TSM_CONFIG file '{}': {e}",
                        path.display()
                    );
                }
                continue;
            }
        };
        let file: ConfigFile = match toml::from_str(&content) {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "Config file '{}' has a parse error and will be ignored: {e}",
                    path.display()
                );
                continue;
            }
        };
        merged.data_dir = merged.data_dir.or(file.data_dir);
        merged.project_root = merged.project_root.or(file.project_root);
        merged.embedder_socket_path = merged.embedder_socket_path.or(file.embedder_socket_path);
        merged.daemon_socket_path = merged.daemon_socket_path.or(file.daemon_socket_path);
        merged.log_dir = merged.log_dir.or(file.log_dir);
        merged.embedder_idle_timeout_secs = merged.embedder_idle_timeout_secs.or(file.embedder_idle_timeout_secs);
        merged.embedder_backfill_interval_secs = merged.embedder_backfill_interval_secs.or(file.embedder_backfill_interval_secs);
    }
    merged
}

fn config_file_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("TSM_CONFIG") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(PathBuf::from("tsm.toml"));
    if let Some(dirs) = ProjectDirs::from("", "", "tsm") {
        candidates.push(dirs.config_dir().join("config.toml"));
    }
    candidates
}

// ─── Accessor functions (env var > config file > default) ────────

/// Resolve data_dir: TSM_DATA_DIR env > config file > .tsm/
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TSM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(ref dir) = cfg().data_dir {
        return dir.clone();
    }
    PathBuf::from(DEFAULT_DATA_DIR)
}

/// Resolve project_root: TSM_PROJECT_ROOT env > config file > /workspaces
pub fn project_root() -> PathBuf {
    if let Ok(root) = std::env::var("TSM_PROJECT_ROOT") {
        return PathBuf::from(root);
    }
    if let Some(ref root) = cfg().project_root {
        return root.clone();
    }
    PathBuf::from(DEFAULT_PROJECT_ROOT)
}

/// Resolve embedder socket path: TSM_EMBEDDER_SOCKET env > config file > .tsm/embedder.sock
pub fn embedder_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("TSM_EMBEDDER_SOCKET") {
        return PathBuf::from(p);
    }
    if let Some(ref p) = cfg().embedder_socket_path {
        return p.clone();
    }
    data_dir().join("embedder.sock")
}

/// Resolve daemon socket path: TSM_DAEMON_SOCKET env > config file > .tsm/daemon.sock
pub fn daemon_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("TSM_DAEMON_SOCKET") {
        return PathBuf::from(p);
    }
    if let Some(ref p) = cfg().daemon_socket_path {
        return p.clone();
    }
    data_dir().join("daemon.sock")
}

/// Resolve log directory: TSM_LOG_DIR env > config file > .tsm/logs
pub fn log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TSM_LOG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(ref dir) = cfg().log_dir {
        return dir.clone();
    }
    data_dir().join("logs")
}

/// Load embedder idle timeout.
/// TSM_EMBEDDER_IDLE_TIMEOUT env > config file > default (600s). 0 = disable.
pub fn embedder_idle_timeout_secs() -> u64 {
    if let Ok(val) = std::env::var("TSM_EMBEDDER_IDLE_TIMEOUT") {
        match val.parse::<u64>() {
            Ok(n) => return n,
            Err(e) => log::warn!(
                "TSM_EMBEDDER_IDLE_TIMEOUT='{val}' is not a valid integer ({e}); using default"
            ),
        }
    }
    if let Some(n) = cfg().embedder_idle_timeout_secs {
        return n;
    }
    DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS
}

/// Load embedder backfill interval.
/// TSM_EMBEDDER_BACKFILL_INTERVAL env > config file > default (300s). 0 = disable.
pub fn embedder_backfill_interval_secs() -> u64 {
    if let Ok(val) = std::env::var("TSM_EMBEDDER_BACKFILL_INTERVAL") {
        match val.parse::<u64>() {
            Ok(n) => return n,
            Err(e) => log::warn!(
                "TSM_EMBEDDER_BACKFILL_INTERVAL='{val}' is not a valid integer ({e}); using default"
            ),
        }
    }
    if let Some(n) = cfg().embedder_backfill_interval_secs {
        return n;
    }
    DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS
}

// ─── Derived paths ───────────────────────────────────────────────

pub fn db_path() -> PathBuf {
    data_dir().join("tsm.db")
}

pub fn user_dict_path() -> PathBuf {
    data_dir().join("user_dict.csv")
}

pub fn custom_terms_path() -> PathBuf {
    data_dir().join("custom_terms.toml")
}

pub fn stopwords_path() -> PathBuf {
    data_dir().join("stopwords.txt")
}

pub fn daemon_pid_path() -> PathBuf {
    data_dir().join("tsmd.pid")
}

// ─── Model cache (XDG) ──────────────────────────────────────────

/// Resolve model cache directory: HF_HUB_CACHE env > $XDG_CACHE_HOME/tsm/models/
pub fn model_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HF_HUB_CACHE") {
        return PathBuf::from(p);
    }
    ProjectDirs::from("", "", "tsm")
        .map(|d| d.cache_dir().join("models"))
        .unwrap_or_else(|| PathBuf::from(".tsm/cache/models"))
}

/// Set HF_HUB_CACHE env var if not already set so hf_hub uses XDG cache.
///
/// # Safety
/// Must be called before any threads are spawned (including the logger).
/// `std::env::set_var` is unsound if concurrent reads or writes to the
/// environment exist. Call this as the very first thing in `main()`.
pub fn ensure_model_cache_env() {
    if std::env::var_os("HF_HUB_CACHE").is_none() {
        let cache_dir = model_cache_dir();
        // SAFETY: called single-threaded before init_logger() and any thread spawn
        unsafe { std::env::set_var("HF_HUB_CACHE", cache_dir) };
    }
}

// ─── Pure functions (no config dependency) ───────────────────────

pub fn status_penalty(status: Option<&str>) -> f64 {
    match status {
        Some("superseded") => 0.2,
        Some("rejected") | Some("dropped") => 0.3,
        Some("outdated") => 0.4,
        _ => 1.0,
    }
}

pub fn half_life_days(source_type: &str) -> f64 {
    match source_type {
        "note" => 120.0,
        "research" => 60.0,
        "session" => 30.0,
        _ => DEFAULT_HALF_LIFE_DAYS,
    }
}

pub fn source_type_from_dir(directory: &str) -> String {
    let last = directory.rsplit('/').next().unwrap_or(directory);
    match last {
        "notes" => "note",
        "research" => "research",
        "intel" => "intel",
        "knowledge" => "knowledge",
        "ideas" => "idea",
        "updates" => "update",
        "products" => "product",
        "decisions" => "decision",
        "retrospectives" => "retrospective",
        other => other,
    }
    .to_string()
}

/// Score weight based on directory prefix of file_path.
pub fn directory_weight(file_path: &str) -> f64 {
    if file_path.starts_with("session:") {
        return SESSION_WEIGHT;
    }
    for &(dir, weight) in CONTENT_DIRS {
        if file_path.starts_with(dir) && file_path.as_bytes().get(dir.len()) == Some(&b'/') {
            return weight;
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_dirs_count() {
        assert_eq!(CONTENT_DIRS.len(), 10);
    }

    #[test]
    fn test_directory_weight_known() {
        assert_eq!(directory_weight("company/knowledge/foo.md"), 1.3);
        assert_eq!(directory_weight("daily/daily/intel/2026-01.md"), 0.7);
        assert_eq!(directory_weight("company/products/ks.md"), 1.2);
        assert_eq!(directory_weight("daily/notes/test.md"), 1.0);
    }

    #[test]
    fn test_directory_weight_session() {
        assert_eq!(directory_weight("session:abc123"), SESSION_WEIGHT);
    }

    #[test]
    fn test_directory_weight_unknown() {
        assert_eq!(directory_weight("unknown/path/file.md"), 1.0);
    }

    #[test]
    fn test_directory_weight_boundary() {
        // "daily/notes_extra/foo.md" must NOT match "daily/notes"
        assert_eq!(directory_weight("daily/notes_extra/foo.md"), 1.0);
    }

    #[test]
    fn test_no_prefix_shadowing() {
        for (i, &(a, _)) in CONTENT_DIRS.iter().enumerate() {
            for (j, &(b, _)) in CONTENT_DIRS.iter().enumerate() {
                if i != j {
                    assert!(
                        !b.starts_with(a) || !a.starts_with(b),
                        "CONTENT_DIRS[{i}]=\"{a}\" and [{j}]=\"{b}\" overlap — reorder longest-first"
                    );
                }
            }
        }
    }

    #[test]
    fn test_status_penalty_values() {
        assert_eq!(status_penalty(None), 1.0);
        assert_eq!(status_penalty(Some("current")), 1.0);
        assert_eq!(status_penalty(Some("outdated")), 0.4);
        assert_eq!(status_penalty(Some("rejected")), 0.3);
        assert_eq!(status_penalty(Some("dropped")), 0.3);
        assert_eq!(status_penalty(Some("superseded")), 0.2);
    }

    #[test]
    fn test_half_life_days_values() {
        assert_eq!(half_life_days("note"), 120.0);
        assert_eq!(half_life_days("research"), 60.0);
        assert_eq!(half_life_days("session"), 30.0);
        assert_eq!(half_life_days("unknown"), DEFAULT_HALF_LIFE_DAYS);
    }

    #[test]
    fn test_source_type_from_dir() {
        assert_eq!(source_type_from_dir("daily/notes"), "note");
        assert_eq!(source_type_from_dir("company/knowledge"), "knowledge");
        assert_eq!(source_type_from_dir("company/products"), "product");
        assert_eq!(source_type_from_dir("novels/novels"), "novels");
        assert_eq!(
            source_type_from_dir("company/retrospectives"),
            "retrospective"
        );
        assert_eq!(source_type_from_dir("unknown_dir"), "unknown_dir");
    }

    #[test]
    fn test_data_dir_env() {
        std::env::set_var("TSM_DATA_DIR", "/tmp/tsm-test-data");
        let dir = data_dir();
        assert_eq!(dir, PathBuf::from("/tmp/tsm-test-data"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_project_root_env() {
        std::env::set_var("TSM_PROJECT_ROOT", "/tmp/tsm-test-root");
        let root = project_root();
        assert_eq!(root, PathBuf::from("/tmp/tsm-test-root"));
        std::env::remove_var("TSM_PROJECT_ROOT");
    }

    #[test]
    fn test_project_root_default_constant() {
        assert_eq!(DEFAULT_PROJECT_ROOT, "/workspaces");
    }

    #[test]
    fn test_db_path_uses_data_dir() {
        std::env::set_var("TSM_DATA_DIR", "/tmp/tsm-db-test");
        let path = db_path();
        assert_eq!(path, PathBuf::from("/tmp/tsm-db-test/tsm.db"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_user_dict_path_uses_data_dir() {
        std::env::set_var("TSM_DATA_DIR", "/tmp/tsm-dict-test");
        let path = user_dict_path();
        assert_eq!(path, PathBuf::from("/tmp/tsm-dict-test/user_dict.csv"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_dict_candidate_freq_threshold() {
        assert_eq!(DICT_CANDIDATE_FREQ_THRESHOLD, 5);
    }

    #[test]
    fn test_embedder_idle_timeout_default_constant() {
        assert_eq!(DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS, 600);
    }

    #[test]
    fn test_embedder_idle_timeout_env() {
        std::env::set_var("TSM_EMBEDDER_IDLE_TIMEOUT", "0");
        let timeout = embedder_idle_timeout_secs();
        assert_eq!(timeout, 0);
        std::env::remove_var("TSM_EMBEDDER_IDLE_TIMEOUT");
    }

    #[test]
    fn test_embedder_idle_timeout_env_custom() {
        std::env::set_var("TSM_EMBEDDER_IDLE_TIMEOUT", "3600");
        let timeout = embedder_idle_timeout_secs();
        assert_eq!(timeout, 3600);
        std::env::remove_var("TSM_EMBEDDER_IDLE_TIMEOUT");
    }

    #[test]
    fn test_embedder_backfill_interval_default_constant() {
        assert_eq!(DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS, 300);
    }

    #[test]
    fn test_embedder_backfill_interval_env() {
        std::env::set_var("TSM_EMBEDDER_BACKFILL_INTERVAL", "0");
        let interval = embedder_backfill_interval_secs();
        assert_eq!(interval, 0);
        std::env::remove_var("TSM_EMBEDDER_BACKFILL_INTERVAL");
    }

    #[test]
    fn test_embedder_backfill_interval_env_custom() {
        std::env::set_var("TSM_EMBEDDER_BACKFILL_INTERVAL", "60");
        let interval = embedder_backfill_interval_secs();
        assert_eq!(interval, 60);
        std::env::remove_var("TSM_EMBEDDER_BACKFILL_INTERVAL");
    }

    #[test]
    fn test_constants() {
        assert_eq!(MAX_CHUNK_CHARS, 800);
        assert_eq!(RRF_K, 60.0);
        assert_eq!(SCORE_THRESHOLD, 0.005);
        assert_eq!(MAX_RESULTS, 5);
        assert_eq!(EMBEDDING_DIM, 256);
    }

    #[test]
    fn test_daemon_socket_path_env() {
        std::env::set_var("TSM_DAEMON_SOCKET", "/tmp/custom-daemon.sock");
        let path = daemon_socket_path();
        assert_eq!(path, PathBuf::from("/tmp/custom-daemon.sock"));
        std::env::remove_var("TSM_DAEMON_SOCKET");
    }

    #[test]
    fn test_daemon_pid_path() {
        std::env::set_var("TSM_DATA_DIR", "/tmp/tsm-pid-test");
        let path = daemon_pid_path();
        assert_eq!(path, PathBuf::from("/tmp/tsm-pid-test/tsmd.pid"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_embedder_socket_path_env() {
        std::env::set_var("TSM_EMBEDDER_SOCKET", "/tmp/custom-embedder.sock");
        let path = embedder_socket_path();
        assert_eq!(path, PathBuf::from("/tmp/custom-embedder.sock"));
        std::env::remove_var("TSM_EMBEDDER_SOCKET");
    }

    #[test]
    fn test_embedder_socket_path_uses_data_dir() {
        std::env::set_var("TSM_DATA_DIR", "/tmp/tsm-sock-test");
        std::env::remove_var("TSM_EMBEDDER_SOCKET");
        let path = embedder_socket_path();
        assert_eq!(path, PathBuf::from("/tmp/tsm-sock-test/embedder.sock"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_log_dir_env() {
        std::env::set_var("TSM_LOG_DIR", "/tmp/tsm-logs");
        let dir = log_dir();
        assert_eq!(dir, PathBuf::from("/tmp/tsm-logs"));
        std::env::remove_var("TSM_LOG_DIR");
    }

    #[test]
    fn test_log_dir_uses_data_dir() {
        std::env::set_var("TSM_DATA_DIR", "/tmp/tsm-log-test");
        std::env::remove_var("TSM_LOG_DIR");
        let dir = log_dir();
        assert_eq!(dir, PathBuf::from("/tmp/tsm-log-test/logs"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_model_cache_dir_env() {
        std::env::set_var("HF_HUB_CACHE", "/tmp/hf-cache");
        let dir = model_cache_dir();
        assert_eq!(dir, PathBuf::from("/tmp/hf-cache"));
        std::env::remove_var("HF_HUB_CACHE");
    }

    #[test]
    fn test_config_file_candidates_includes_xdg() {
        std::env::remove_var("TSM_CONFIG");
        let candidates = config_file_candidates();
        // Should have at least ./tsm.toml and XDG path
        assert!(candidates.len() >= 2);
        assert_eq!(candidates[0], PathBuf::from("tsm.toml"));
        assert!(candidates[1].to_string_lossy().contains("tsm"));
    }

    #[test]
    fn test_config_file_candidates_with_env() {
        std::env::set_var("TSM_CONFIG", "/tmp/custom-config.toml");
        let candidates = config_file_candidates();
        assert_eq!(candidates[0], PathBuf::from("/tmp/custom-config.toml"));
        std::env::remove_var("TSM_CONFIG");
    }

    #[test]
    fn test_load_config_from_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test-config.toml");
        std::fs::write(
            &config_path,
            r#"
data_dir = "/custom/data"
project_root = "/custom/root"
embedder_idle_timeout_secs = 1200
"#,
        )
        .unwrap();

        let cfg = load_config_from(&[config_path]);
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/custom/data")));
        assert_eq!(cfg.project_root, Some(PathBuf::from("/custom/root")));
        assert_eq!(cfg.embedder_idle_timeout_secs, Some(1200));
        assert!(cfg.daemon_socket_path.is_none());
    }

    #[test]
    fn test_load_config_merge_priority() {
        let dir = tempfile::tempdir().unwrap();

        let high = dir.path().join("high.toml");
        std::fs::write(&high, r#"data_dir = "/high""#).unwrap();

        let low = dir.path().join("low.toml");
        std::fs::write(
            &low,
            r#"
data_dir = "/low"
project_root = "/low-root"
"#,
        )
        .unwrap();

        // High-priority file is first in the list
        let cfg = load_config_from(&[high, low]);
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/high")));
        // project_root only in low-priority file, still picked up
        assert_eq!(cfg.project_root, Some(PathBuf::from("/low-root")));
    }

    #[test]
    fn test_load_config_empty_candidates() {
        let cfg = load_config_from(&[]);
        assert!(cfg.data_dir.is_none());
        assert!(cfg.project_root.is_none());
    }

    #[test]
    fn test_load_config_missing_file_skipped() {
        let cfg = load_config_from(&[PathBuf::from("/nonexistent/tsm.toml")]);
        assert!(cfg.data_dir.is_none());
    }

    #[test]
    fn test_env_var_overrides_config_file() {
        // Even if OnceLock has a cached config, env var always wins
        std::env::set_var("TSM_DATA_DIR", "/env/data");
        assert_eq!(data_dir(), PathBuf::from("/env/data"));
        std::env::remove_var("TSM_DATA_DIR");
    }

    #[test]
    fn test_load_config_malformed_file_skipped() {
        let dir = tempfile::tempdir().unwrap();

        let malformed = dir.path().join("bad.toml");
        std::fs::write(&malformed, "this is not valid toml [[[").unwrap();

        let valid = dir.path().join("good.toml");
        std::fs::write(&valid, r#"data_dir = "/good""#).unwrap();

        // Malformed file is higher priority but skipped; valid file still used
        let cfg = load_config_from(&[malformed, valid]);
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/good")));
    }

    #[test]
    fn test_ensure_model_cache_env_sets_when_absent() {
        std::env::remove_var("HF_HUB_CACHE");
        ensure_model_cache_env();
        assert!(std::env::var_os("HF_HUB_CACHE").is_some());
        std::env::remove_var("HF_HUB_CACHE");
    }

    #[test]
    fn test_ensure_model_cache_env_preserves_existing() {
        std::env::set_var("HF_HUB_CACHE", "/my/custom/cache");
        ensure_model_cache_env();
        assert_eq!(
            std::env::var("HF_HUB_CACHE").unwrap(),
            "/my/custom/cache"
        );
        std::env::remove_var("HF_HUB_CACHE");
    }
}
