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

const DEFAULT_STATE_DIR: &str = ".tsm";
const DEFAULT_INDEX_ROOT: &str = "/workspaces";
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
pub(crate) struct ConfigFile {
    state_dir: Option<PathBuf>,
    index_root: Option<PathBuf>,
    embedder_socket_path: Option<PathBuf>,
    daemon_socket_path: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    embedder_idle_timeout_secs: Option<u64>,
    embedder_backfill_interval_secs: Option<u64>,
}

/// Fully resolved configuration — all values determined at startup.
///
/// Resolution priority: env var > config file (tsm.toml) > default.
/// Built once via `from_env()` and stored in a `OnceLock` singleton.
/// In tests, construct directly via `from_config_file()` without env var mutation.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Root directory for all tsm data (DB, dictionaries, PID files, logs).
    /// Default: `.tsm/` (relative to working directory).
    /// Env: `TSM_STATE_DIR`. Config: `state_dir`.
    pub state_dir: PathBuf,

    /// Root directory containing content workspaces to index.
    /// Default: `/workspaces`.
    /// Env: `TSM_INDEX_ROOT`. Config: `index_root`.
    pub index_root: PathBuf,

    /// UNIX socket path for tsm-embedder (encode requests).
    /// Default: `{state_dir}/embedder.sock`.
    /// Env: `TSM_EMBEDDER_SOCKET`. Config: `embedder_socket_path`.
    pub embedder_socket_path: PathBuf,

    /// UNIX socket path for tsmd (client requests).
    /// Default: `{state_dir}/daemon.sock`.
    /// Env: `TSM_DAEMON_SOCKET`. Config: `daemon_socket_path`.
    pub daemon_socket_path: PathBuf,

    /// Directory for daemon log files (tsmd, tsm-embedder, tsm-watcher).
    /// Default: `{state_dir}/logs`.
    /// Env: `TSM_LOG_DIR`. Config: `log_dir`.
    pub log_dir: PathBuf,

    /// Seconds of inactivity before tsm-embedder shuts down. 0 = never.
    /// Default: 600.
    /// Env: `TSM_EMBEDDER_IDLE_TIMEOUT`. Config: `embedder_idle_timeout_secs`.
    pub embedder_idle_timeout_secs: u64,

    /// Seconds between periodic backfill checks. 0 = disable.
    /// Default: 300.
    /// Env: `TSM_EMBEDDER_BACKFILL_INTERVAL`. Config: `embedder_backfill_interval_secs`.
    pub embedder_backfill_interval_secs: u64,
}

impl ResolvedConfig {
    /// Resolve all config values from environment variables, config files, and defaults.
    pub fn from_env() -> Self {
        let file_cfg = load_config_from(&config_file_candidates());
        Self::from_config_file(&file_cfg)
    }

    /// Resolve from a pre-loaded `ConfigFile` (still reads env vars for overrides).
    /// Visible within the crate for testing; production code should use `from_env()`.
    pub(crate) fn from_config_file(file_cfg: &ConfigFile) -> Self {
        let state_dir = env_or("TSM_STATE_DIR", file_cfg.state_dir.as_ref())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR));

        let index_root = env_or("TSM_INDEX_ROOT", file_cfg.index_root.as_ref())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_INDEX_ROOT));

        let embedder_socket_path = env_or(
            "TSM_EMBEDDER_SOCKET",
            file_cfg.embedder_socket_path.as_ref(),
        )
        .unwrap_or_else(|| state_dir.join("embedder.sock"));

        let daemon_socket_path = env_or("TSM_DAEMON_SOCKET", file_cfg.daemon_socket_path.as_ref())
            .unwrap_or_else(|| state_dir.join("daemon.sock"));

        let log_dir = env_or("TSM_LOG_DIR", file_cfg.log_dir.as_ref())
            .unwrap_or_else(|| state_dir.join("logs"));

        let embedder_idle_timeout_secs = env_parse_u64(
            "TSM_EMBEDDER_IDLE_TIMEOUT",
            file_cfg.embedder_idle_timeout_secs,
        )
        .unwrap_or(DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS);

        let embedder_backfill_interval_secs = env_parse_u64(
            "TSM_EMBEDDER_BACKFILL_INTERVAL",
            file_cfg.embedder_backfill_interval_secs,
        )
        .unwrap_or(DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS);

        Self {
            state_dir,
            index_root,
            embedder_socket_path,
            daemon_socket_path,
            log_dir,
            embedder_idle_timeout_secs,
            embedder_backfill_interval_secs,
        }
    }
}

/// Read an env var as PathBuf, falling back to a config file value.
fn env_or(var: &str, file_val: Option<&PathBuf>) -> Option<PathBuf> {
    if let Ok(val) = std::env::var(var) {
        return Some(PathBuf::from(val));
    }
    file_val.cloned()
}

/// Read an env var as u64, falling back to a config file value.
fn env_parse_u64(var: &str, file_val: Option<u64>) -> Option<u64> {
    if let Ok(val) = std::env::var(var) {
        match val.parse::<u64>() {
            Ok(n) => return Some(n),
            Err(e) => log::warn!("{var}='{val}' is not a valid integer ({e}); using default"),
        }
    }
    file_val
}

static RESOLVED: OnceLock<ResolvedConfig> = OnceLock::new();

/// Get the lazily-loaded resolved config singleton.
fn resolved() -> &'static ResolvedConfig {
    RESOLVED.get_or_init(ResolvedConfig::from_env)
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
                    log::error!("Cannot read TSM_CONFIG file '{}': {e}", path.display());
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
        merged.state_dir = merged.state_dir.or(file.state_dir);
        merged.index_root = merged.index_root.or(file.index_root);
        merged.embedder_socket_path = merged.embedder_socket_path.or(file.embedder_socket_path);
        merged.daemon_socket_path = merged.daemon_socket_path.or(file.daemon_socket_path);
        merged.log_dir = merged.log_dir.or(file.log_dir);
        merged.embedder_idle_timeout_secs = merged
            .embedder_idle_timeout_secs
            .or(file.embedder_idle_timeout_secs);
        merged.embedder_backfill_interval_secs = merged
            .embedder_backfill_interval_secs
            .or(file.embedder_backfill_interval_secs);
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

// ─── Accessor functions (delegate to ResolvedConfig singleton) ───

pub fn state_dir() -> PathBuf {
    resolved().state_dir.clone()
}

pub fn index_root() -> PathBuf {
    resolved().index_root.clone()
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

// ─── Derived paths ───────────────────────────────────────────────

pub fn db_path() -> PathBuf {
    state_dir().join("tsm.db")
}

pub fn user_dict_path() -> PathBuf {
    state_dir().join("user_dict.csv")
}

pub fn custom_terms_path() -> PathBuf {
    state_dir().join("custom_terms.toml")
}

pub fn stopwords_path() -> PathBuf {
    state_dir().join("stopwords.txt")
}

pub fn daemon_pid_path() -> PathBuf {
    state_dir().join("tsmd.pid")
}

// ─── Model cache (XDG) ──────────────────────────────────────────
// NOTE: model_cache_dir and ensure_model_cache_env are intentionally NOT
// part of ResolvedConfig. ensure_model_cache_env performs a side-effectful
// set_var that must run before any threads spawn (including the logger),
// and HF_HUB_CACHE is consumed by the hf_hub crate, not by tsm itself.

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
    use serial_test::serial;

    // ─── Helper: build ResolvedConfig from a TOML string ──────────────

    /// Build a ResolvedConfig from inline TOML. Does NOT clear env vars —
    /// callers that need clean defaults must clear TSM_* vars themselves
    /// or use #[serial].
    fn resolved_from_toml(toml_content: &str) -> ResolvedConfig {
        let file_cfg: ConfigFile = toml::from_str(toml_content).unwrap();
        ResolvedConfig::from_config_file(&file_cfg)
    }

    // ─── Constants ──────────────────────────────────────────────────

    #[test]
    fn test_content_dirs_count() {
        assert_eq!(CONTENT_DIRS.len(), 10);
    }

    #[test]
    fn test_constants() {
        assert_eq!(MAX_CHUNK_CHARS, 800);
        assert_eq!(RRF_K, 60.0);
        assert_eq!(SCORE_THRESHOLD, 0.005);
        assert_eq!(MAX_RESULTS, 5);
        assert_eq!(EMBEDDING_DIM, 256);
        assert_eq!(DEFAULT_INDEX_ROOT, "/workspaces");
        assert_eq!(DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS, 600);
        assert_eq!(DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS, 300);
        assert_eq!(DICT_CANDIDATE_FREQ_THRESHOLD, 5);
    }

    // ─── ResolvedConfig from config file ─────────────────────────────
    // These tests construct ResolvedConfig via from_config_file() directly,
    // bypassing the OnceLock singleton. Tests that touch env vars use #[serial].
    // Tests with no env var mutation can run in parallel safely IF no TSM_*
    // env vars are set in the execution environment. test_resolved_defaults
    // clears them explicitly to be safe in CI.

    #[test]
    #[serial]
    fn test_resolved_defaults() {
        // Clear all TSM env vars to ensure defaults are tested, not CI overrides.
        for var in [
            "TSM_STATE_DIR",
            "TSM_INDEX_ROOT",
            "TSM_EMBEDDER_SOCKET",
            "TSM_DAEMON_SOCKET",
            "TSM_LOG_DIR",
            "TSM_EMBEDDER_IDLE_TIMEOUT",
            "TSM_EMBEDDER_BACKFILL_INTERVAL",
        ] {
            std::env::remove_var(var);
        }
        let cfg = resolved_from_toml("");
        assert_eq!(cfg.state_dir, PathBuf::from(DEFAULT_STATE_DIR));
        assert_eq!(cfg.index_root, PathBuf::from(DEFAULT_INDEX_ROOT));
        assert_eq!(
            cfg.embedder_socket_path,
            PathBuf::from(".tsm/embedder.sock")
        );
        assert_eq!(cfg.daemon_socket_path, PathBuf::from(".tsm/daemon.sock"));
        assert_eq!(cfg.log_dir, PathBuf::from(".tsm/logs"));
        assert_eq!(
            cfg.embedder_idle_timeout_secs,
            DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS
        );
        assert_eq!(
            cfg.embedder_backfill_interval_secs,
            DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS
        );
    }

    #[test]
    fn test_resolved_from_config_file() {
        let cfg = resolved_from_toml(
            r#"
            state_dir = "/custom/data"
            index_root = "/custom/root"
            embedder_idle_timeout_secs = 0
            embedder_backfill_interval_secs = 60
        "#,
        );
        assert_eq!(cfg.state_dir, PathBuf::from("/custom/data"));
        assert_eq!(cfg.index_root, PathBuf::from("/custom/root"));
        assert_eq!(cfg.embedder_idle_timeout_secs, 0);
        assert_eq!(cfg.embedder_backfill_interval_secs, 60);
    }

    #[test]
    fn test_resolved_socket_paths_follow_state_dir() {
        let cfg = resolved_from_toml(r#"state_dir = "/my/data""#);
        assert_eq!(
            cfg.embedder_socket_path,
            PathBuf::from("/my/data/embedder.sock")
        );
        assert_eq!(
            cfg.daemon_socket_path,
            PathBuf::from("/my/data/daemon.sock")
        );
        assert_eq!(cfg.log_dir, PathBuf::from("/my/data/logs"));
    }

    #[test]
    fn test_resolved_explicit_socket_overrides_state_dir() {
        let cfg = resolved_from_toml(
            r#"
            state_dir = "/my/data"
            embedder_socket_path = "/custom/embedder.sock"
            daemon_socket_path = "/custom/daemon.sock"
            log_dir = "/custom/logs"
        "#,
        );
        assert_eq!(
            cfg.embedder_socket_path,
            PathBuf::from("/custom/embedder.sock")
        );
        assert_eq!(cfg.daemon_socket_path, PathBuf::from("/custom/daemon.sock"));
        assert_eq!(cfg.log_dir, PathBuf::from("/custom/logs"));
    }

    #[test]
    fn test_resolved_derived_paths() {
        let cfg = resolved_from_toml(r#"state_dir = "/test""#);
        assert_eq!(cfg.state_dir.join("tsm.db"), PathBuf::from("/test/tsm.db"));
        assert_eq!(
            cfg.state_dir.join("user_dict.csv"),
            PathBuf::from("/test/user_dict.csv")
        );
        assert_eq!(
            cfg.state_dir.join("tsmd.pid"),
            PathBuf::from("/test/tsmd.pid")
        );
    }

    // ─── ConfigFile loading (TOML parsing, merge, error handling) ───

    #[test]
    fn test_load_config_from_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test-config.toml");
        std::fs::write(
            &config_path,
            r#"
state_dir = "/custom/data"
index_root = "/custom/root"
embedder_idle_timeout_secs = 1200
"#,
        )
        .unwrap();

        let cfg = load_config_from(&[config_path]);
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/custom/data")));
        assert_eq!(cfg.index_root, Some(PathBuf::from("/custom/root")));
        assert_eq!(cfg.embedder_idle_timeout_secs, Some(1200));
        assert!(cfg.daemon_socket_path.is_none());
    }

    #[test]
    fn test_load_config_merge_priority() {
        let dir = tempfile::tempdir().unwrap();

        let high = dir.path().join("high.toml");
        std::fs::write(&high, r#"state_dir = "/high""#).unwrap();

        let low = dir.path().join("low.toml");
        std::fs::write(
            &low,
            r#"
state_dir = "/low"
index_root = "/low-root"
"#,
        )
        .unwrap();

        let cfg = load_config_from(&[high, low]);
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/high")));
        assert_eq!(cfg.index_root, Some(PathBuf::from("/low-root")));
    }

    #[test]
    fn test_load_config_empty_candidates() {
        let cfg = load_config_from(&[]);
        assert!(cfg.state_dir.is_none());
        assert!(cfg.index_root.is_none());
    }

    #[test]
    fn test_load_config_missing_file_skipped() {
        let cfg = load_config_from(&[PathBuf::from("/nonexistent/tsm.toml")]);
        assert!(cfg.state_dir.is_none());
    }

    #[test]
    fn test_load_config_malformed_file_skipped() {
        let dir = tempfile::tempdir().unwrap();

        let malformed = dir.path().join("bad.toml");
        std::fs::write(&malformed, "this is not valid toml [[[").unwrap();

        let valid = dir.path().join("good.toml");
        std::fs::write(&valid, r#"state_dir = "/good""#).unwrap();

        let cfg = load_config_from(&[malformed, valid]);
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/good")));
    }

    // ─── Pure functions ─────────────────────────────────────────────

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

    // ─── Env var integration tests (serialized, minimal) ────────────
    // These tests verify that env vars override config file values at the
    // ResolvedConfig::from_config_file level. They call from_config_file()
    // directly — NOT resolved() — to avoid the OnceLock singleton, which
    // is initialized once per process and cannot be reset between tests.

    #[test]
    #[serial]
    fn test_env_var_overrides_config_state_dir() {
        std::env::set_var("TSM_STATE_DIR", "/env/override");
        let cfg = ResolvedConfig::from_config_file(&ConfigFile {
            state_dir: Some(PathBuf::from("/from/config")),
            ..Default::default()
        });
        std::env::remove_var("TSM_STATE_DIR");
        // env wins over config file value
        assert_eq!(cfg.state_dir, PathBuf::from("/env/override"));
    }

    #[test]
    #[serial]
    fn test_env_var_overrides_config_timeout() {
        std::env::set_var("TSM_EMBEDDER_IDLE_TIMEOUT", "42");
        let cfg = ResolvedConfig::from_config_file(&ConfigFile::default());
        std::env::remove_var("TSM_EMBEDDER_IDLE_TIMEOUT");
        assert_eq!(cfg.embedder_idle_timeout_secs, 42);
    }

    #[test]
    #[serial]
    fn test_env_var_invalid_integer_falls_back_to_config() {
        std::env::set_var("TSM_EMBEDDER_IDLE_TIMEOUT", "not_a_number");
        let cfg = ResolvedConfig::from_config_file(&ConfigFile {
            embedder_idle_timeout_secs: Some(999),
            ..Default::default()
        });
        std::env::remove_var("TSM_EMBEDDER_IDLE_TIMEOUT");
        // Invalid env var → falls back to config file value
        assert_eq!(cfg.embedder_idle_timeout_secs, 999);
    }

    #[test]
    #[serial]
    fn test_env_var_overrides_config_socket() {
        std::env::set_var("TSM_EMBEDDER_SOCKET", "/tmp/custom.sock");
        let cfg = ResolvedConfig::from_config_file(&ConfigFile::default());
        std::env::remove_var("TSM_EMBEDDER_SOCKET");
        assert_eq!(cfg.embedder_socket_path, PathBuf::from("/tmp/custom.sock"));
    }

    #[test]
    #[serial]
    fn test_config_file_candidates_includes_xdg() {
        std::env::remove_var("TSM_CONFIG");
        let candidates = config_file_candidates();
        assert!(candidates.len() >= 2);
        assert_eq!(candidates[0], PathBuf::from("tsm.toml"));
    }

    #[test]
    #[serial]
    fn test_config_file_candidates_with_env() {
        std::env::set_var("TSM_CONFIG", "/tmp/custom-config.toml");
        let candidates = config_file_candidates();
        std::env::remove_var("TSM_CONFIG");
        assert_eq!(candidates[0], PathBuf::from("/tmp/custom-config.toml"));
    }

    #[test]
    #[serial]
    fn test_model_cache_dir_env() {
        std::env::set_var("HF_HUB_CACHE", "/tmp/hf-cache");
        let dir = model_cache_dir();
        std::env::remove_var("HF_HUB_CACHE");
        assert_eq!(dir, PathBuf::from("/tmp/hf-cache"));
    }

    #[test]
    #[serial]
    fn test_ensure_model_cache_env_sets_when_absent() {
        std::env::remove_var("HF_HUB_CACHE");
        ensure_model_cache_env();
        assert!(std::env::var_os("HF_HUB_CACHE").is_some());
        std::env::remove_var("HF_HUB_CACHE");
    }

    #[test]
    #[serial]
    fn test_ensure_model_cache_env_preserves_existing() {
        std::env::set_var("HF_HUB_CACHE", "/my/custom/cache");
        ensure_model_cache_env();
        assert_eq!(std::env::var("HF_HUB_CACHE").unwrap(), "/my/custom/cache");
        std::env::remove_var("HF_HUB_CACHE");
    }
}
