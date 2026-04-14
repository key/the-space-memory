use std::fmt;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

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
pub const REINDEX_FTS_BATCH_SIZE: usize = 1000;
pub const MAX_QUERY_EXPANSIONS: usize = 5;
pub const RECENT_DAYS: i64 = 30;
pub const DICT_CANDIDATE_FREQ_THRESHOLD: i64 = 5;
pub const MIN_QUERY_KEYWORDS: usize = 1;

const DEFAULT_STATE_DIR: &str = ".tsm";
const DEFAULT_INDEX_ROOT: &str = "/workspaces";
const DEFAULT_EMBEDDER_IDLE_TIMEOUT_SECS: u64 = 600;
const DEFAULT_EMBEDDER_BACKFILL_INTERVAL_SECS: u64 = 300;

/// Behavior when the embedder is stopped during search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub enum SearchFallback {
    /// Error exit (default) — refuse to search without vector search.
    #[default]
    Error,
    /// Fall back to FTS5-only search with a warning.
    FtsOnly,
}

impl<'de> serde::Deserialize<'de> for SearchFallback {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "error" => Ok(SearchFallback::Error),
            "fts_only" => Ok(SearchFallback::FtsOnly),
            other => Err(serde::de::Error::custom(format!(
                "unknown search_fallback value: '{other}' (expected 'error' or 'fts_only')"
            ))),
        }
    }
}

impl fmt::Display for SearchFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchFallback::Error => write!(f, "error"),
            SearchFallback::FtsOnly => write!(f, "fts_only"),
        }
    }
}

impl std::str::FromStr for SearchFallback {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "error" => Ok(SearchFallback::Error),
            "fts_only" => Ok(SearchFallback::FtsOnly),
            other => Err(format!(
                "unknown search_fallback value: '{other}' (expected 'error' or 'fts_only')"
            )),
        }
    }
}

const DEFAULT_SESSION_WEIGHT: f64 = 0.3;
const DEFAULT_SESSION_HALF_LIFE_DAYS: f64 = 30.0;
const DEFAULT_IGNORE_FILE: &str = ".tsmignore";
const DEFAULT_INDEX_EXTENSION: &str = "md";

// ─── Config struct ───────────────────────────────────────────────

/// A content directory entry as written in tsm.toml.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ContentDirConfig {
    pub path: String,
    pub weight: Option<f64>,
    pub half_life_days: Option<f64>,
}

/// A content directory entry with all values resolved.
#[derive(Debug, Clone)]
pub struct ContentDir {
    pub path: String,
    pub weight: f64,
    pub half_life_days: f64,
}

/// Claude session scoring config as written in tsm.toml.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ClaudeSessionConfig {
    pub weight: Option<f64>,
    pub half_life_days: Option<f64>,
}

/// The `[index]` section of tsm.toml.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default)]
pub(crate) struct IndexConfig {
    pub content_dirs: Vec<ContentDirConfig>,
    pub claude_session: ClaudeSessionConfig,
    pub respect_gitignore: Option<bool>,
    pub ignore_file: Option<String>,
    pub extensions: Option<Vec<String>>,
}

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
    search_fallback: Option<SearchFallback>,
    user_dict_path: Option<PathBuf>,
    #[serde(default)]
    index: IndexConfig,
}

/// Fully resolved configuration.
///
/// Resolution priority: env var > config file (tsm.toml) > default.
/// Stored in a `OnceLock<RwLock<ResolvedConfig>>` singleton. Initialized lazily
/// via `from_env()`; may be updated at runtime by calling `reload()`.
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

    /// UNIX socket path for the embedder child process (encode requests).
    /// Default: `{state_dir}/embedder.sock`.
    /// Env: `TSM_EMBEDDER_SOCKET`. Config: `embedder_socket_path`.
    pub embedder_socket_path: PathBuf,

    /// UNIX socket path for tsmd (client requests).
    /// Default: `{state_dir}/daemon.sock`.
    /// Env: `TSM_DAEMON_SOCKET`. Config: `daemon_socket_path`.
    pub daemon_socket_path: PathBuf,

    /// Directory for daemon log files (tsmd, tsmd --embedder, tsmd --fs-watcher).
    /// Default: `{state_dir}/logs`.
    /// Env: `TSM_LOG_DIR`. Config: `log_dir`.
    pub log_dir: PathBuf,

    /// Seconds of inactivity before the embedder child process shuts down. 0 = never.
    /// Default: 600.
    /// Env: `TSM_EMBEDDER_IDLE_TIMEOUT`. Config: `embedder_idle_timeout_secs`.
    pub embedder_idle_timeout_secs: u64,

    /// Seconds between periodic backfill checks. 0 = disable.
    /// Default: 300.
    /// Env: `TSM_EMBEDDER_BACKFILL_INTERVAL`. Config: `embedder_backfill_interval_secs`.
    pub embedder_backfill_interval_secs: u64,

    /// Behavior when the embedder is stopped during search.
    /// Default: `Error` (refuse to search without vector search).
    /// Env: `TSM_SEARCH_FALLBACK`. Config: `search_fallback`.
    pub search_fallback: SearchFallback,

    /// Path to user dictionary file (lindera IPAdic format).
    /// Default: `{state_dir}/user_dict.simpledic`.
    /// Env: `TSM_USER_DICT`. Config: `user_dict_path`.
    pub user_dict_path: PathBuf,

    /// Content directories with scoring weights and half-life.
    /// Empty = auto-discover mode (recursively index all .md under index_root).
    pub content_dirs: Vec<ContentDir>,

    /// Score weight for Claude Code session data.
    pub session_weight: f64,

    /// Half-life in days for Claude Code session data time decay.
    pub session_half_life_days: f64,

    /// Whether to also apply the root `.gitignore` under `index_root` during indexing.
    /// Default: true. Config: `[index].respect_gitignore`.
    pub respect_gitignore: bool,

    /// Filename of the project-level ignore file, resolved relative to the tsm
    /// project root (the directory containing `tsm.toml`). Its patterns are
    /// applied relative to `index_root`.
    /// Default: `.tsmignore`. Config: `[index].ignore_file`.
    pub ignore_file: String,

    /// File extensions to include during indexing (without leading dot).
    /// Default: `["md"]`. Config: `[index].extensions`.
    pub extensions: Vec<String>,
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

        let search_fallback = env_parse_fallback(file_cfg.search_fallback);

        let user_dict_path = env_or("TSM_USER_DICT", file_cfg.user_dict_path.as_ref())
            .unwrap_or_else(|| state_dir.join("user_dict.simpledic"));

        let mut content_dirs: Vec<ContentDir> = file_cfg
            .index
            .content_dirs
            .iter()
            .filter_map(|c| {
                if c.path.is_empty() {
                    log::warn!("content_dirs entry has empty path; skipping");
                    return None;
                }
                if std::path::Path::new(&c.path).is_absolute() {
                    log::warn!(
                        "content_dirs entry '{}' is absolute; paths must be relative to index_root",
                        c.path
                    );
                    return None;
                }
                let weight = c.weight.unwrap_or(1.0);
                if !weight.is_finite() || weight <= 0.0 {
                    log::warn!(
                        "content_dirs '{}': weight {weight} is invalid; using 1.0",
                        c.path
                    );
                }
                let half_life = c.half_life_days.unwrap_or(DEFAULT_HALF_LIFE_DAYS);
                if !half_life.is_finite() || half_life <= 0.0 {
                    log::warn!(
                        "content_dirs '{}': half_life_days {half_life} is invalid; using {DEFAULT_HALF_LIFE_DAYS}",
                        c.path
                    );
                }
                Some(ContentDir {
                    path: c.path.clone(),
                    weight: if weight.is_finite() && weight > 0.0 {
                        weight
                    } else {
                        1.0
                    },
                    half_life_days: if half_life.is_finite() && half_life > 0.0 {
                        half_life
                    } else {
                        DEFAULT_HALF_LIFE_DAYS
                    },
                })
            })
            .collect();
        // Sort longest-first so more-specific paths match before shorter prefixes
        content_dirs.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

        let session_weight = file_cfg
            .index
            .claude_session
            .weight
            .unwrap_or(DEFAULT_SESSION_WEIGHT);

        let session_half_life_days = file_cfg
            .index
            .claude_session
            .half_life_days
            .unwrap_or(DEFAULT_SESSION_HALF_LIFE_DAYS);

        let respect_gitignore = file_cfg.index.respect_gitignore.unwrap_or(true);

        let ignore_file = file_cfg
            .index
            .ignore_file
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_IGNORE_FILE.to_string());

        let extensions = file_cfg
            .index
            .extensions
            .clone()
            .map(|v| v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![DEFAULT_INDEX_EXTENSION.to_string()]);

        Self {
            state_dir,
            index_root,
            embedder_socket_path,
            daemon_socket_path,
            log_dir,
            embedder_idle_timeout_secs,
            embedder_backfill_interval_secs,
            search_fallback,
            user_dict_path,
            content_dirs,
            session_weight,
            session_half_life_days,
            respect_gitignore,
            ignore_file,
            extensions,
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

/// Resolve search_fallback: env var > config file > default (Error).
fn env_parse_fallback(file_val: Option<SearchFallback>) -> SearchFallback {
    if let Ok(val) = std::env::var("TSM_SEARCH_FALLBACK") {
        match val.parse::<SearchFallback>() {
            Ok(f) => return f,
            Err(e) => log::warn!("TSM_SEARCH_FALLBACK='{val}' is invalid ({e}); using default"),
        }
    }
    file_val.unwrap_or_default()
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

static RESOLVED: OnceLock<RwLock<ResolvedConfig>> = OnceLock::new();

/// Get the lazily-loaded resolved config singleton (cloned).
fn resolved() -> ResolvedConfig {
    RESOLVED
        .get_or_init(|| RwLock::new(ResolvedConfig::from_env()))
        .read()
        .expect("config RwLock poisoned")
        .clone()
}

/// Reload config from tsm.toml. Returns a list of warnings for fields
/// that changed but require a daemon restart to take effect.
pub fn reload() -> Vec<String> {
    // Ensure singleton is initialized before building new config
    let _ = resolved();
    let new_cfg = ResolvedConfig::from_env();

    // Hold write lock for the entire read-compare-write to avoid TOCTOU races
    let lock = RESOLVED.get().expect("config not initialized");
    let mut w = lock.write().expect("config RwLock poisoned");
    let old = w.clone();

    let mut warnings = Vec::new();

    if old.state_dir != new_cfg.state_dir {
        warnings.push(format!(
            "state_dir changed ({} → {}); requires `tsm restart`",
            old.state_dir.display(),
            new_cfg.state_dir.display()
        ));
    }
    if old.index_root != new_cfg.index_root {
        warnings.push(format!(
            "index_root changed ({} → {}); requires `tsm restart`",
            old.index_root.display(),
            new_cfg.index_root.display()
        ));
    }
    if old.daemon_socket_path != new_cfg.daemon_socket_path {
        warnings.push("daemon_socket_path changed; requires `tsm restart`".to_string());
    }
    if old.embedder_socket_path != new_cfg.embedder_socket_path {
        warnings.push("embedder_socket_path changed; requires `tsm restart`".to_string());
    }
    if old.log_dir != new_cfg.log_dir {
        warnings.push("log_dir changed; requires `tsm restart`".to_string());
    }
    if old.user_dict_path != new_cfg.user_dict_path {
        warnings.push("user_dict_path changed; requires `tsm restart`".to_string());
    }

    *w = new_cfg;

    log::info!("config reloaded from tsm.toml");
    if !warnings.is_empty() {
        for w in &warnings {
            log::warn!("{w}");
        }
    }
    warnings
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
        merged.search_fallback = merged.search_fallback.or(file.search_fallback);
        merged.user_dict_path = merged.user_dict_path.or(file.user_dict_path);
        if merged.index.content_dirs.is_empty() {
            merged.index.content_dirs = file.index.content_dirs;
        }
        merged.index.claude_session.weight = merged
            .index
            .claude_session
            .weight
            .or(file.index.claude_session.weight);
        merged.index.claude_session.half_life_days = merged
            .index
            .claude_session
            .half_life_days
            .or(file.index.claude_session.half_life_days);
        merged.index.respect_gitignore = merged
            .index
            .respect_gitignore
            .or(file.index.respect_gitignore);
        merged.index.ignore_file = merged.index.ignore_file.or(file.index.ignore_file);
        if merged.index.extensions.is_none() {
            merged.index.extensions = file.index.extensions;
        }
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

// ─── Derived paths ───────────────────────────────────────────────

pub fn db_path() -> PathBuf {
    state_dir().join("tsm.db")
}

pub fn user_dict_path() -> PathBuf {
    resolved().user_dict_path.clone()
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

// ─── Local model directory ──────────────────────────────────────

/// Canonical list of required model files. Used by `models_dir_complete()`,
/// `embedder_mode::load_model()`, and `doctor_check_with_conn()`.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Directory for locally cached model files: `{state_dir}/models/ruri-v3-30m/`.
pub fn models_dir() -> PathBuf {
    state_dir().join("models/ruri-v3-30m")
}

/// Check if all required model files exist in `models_dir()`.
/// Returns `Some(path)` if all files in `MODEL_FILES` are present, `None` otherwise.
pub fn models_dir_complete() -> Option<PathBuf> {
    let dir = models_dir();
    if MODEL_FILES.iter().all(|f| dir.join(f).is_file()) {
        Some(dir)
    } else {
        None
    }
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

/// Half-life in days, resolved from content_dirs config by file path prefix.
/// Falls back to source_type-based defaults when content_dirs is empty or unmatched.
pub fn half_life_days(file_path: &str, source_type: &str) -> f64 {
    let cfg = resolved();
    if file_path.starts_with("session:") {
        return cfg.session_half_life_days;
    }
    for dir in &cfg.content_dirs {
        if file_path.starts_with(dir.path.as_str())
            && file_path.as_bytes().get(dir.path.len()) == Some(&b'/')
        {
            return dir.half_life_days;
        }
    }
    half_life_days_by_source_type(source_type)
}

/// Default half-life by source_type (used when content_dirs is empty or unmatched).
fn half_life_days_by_source_type(source_type: &str) -> f64 {
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
    let cfg = resolved();
    if file_path.starts_with("session:") {
        return cfg.session_weight;
    }
    for dir in &cfg.content_dirs {
        if file_path.starts_with(dir.path.as_str())
            && file_path.as_bytes().get(dir.path.len()) == Some(&b'/')
        {
            return dir.weight;
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
    // bypassing the OnceLock singleton. Tests that set TOML fields overridden
    // by env vars (state_dir, index_root, etc.) must use #[serial] to avoid
    // races with other tests that mutate those env vars.

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
        assert!(cfg.respect_gitignore);
        assert_eq!(cfg.ignore_file, DEFAULT_IGNORE_FILE);
        assert_eq!(cfg.extensions, vec![DEFAULT_INDEX_EXTENSION.to_string()]);
    }

    #[test]
    fn test_index_ignore_settings_from_toml() {
        let cfg = resolved_from_toml(
            r#"
            [index]
            respect_gitignore = false
            ignore_file = ".myignore"
            extensions = ["md", "txt"]
        "#,
        );
        assert!(!cfg.respect_gitignore);
        assert_eq!(cfg.ignore_file, ".myignore");
        assert_eq!(cfg.extensions, vec!["md".to_string(), "txt".to_string()]);
    }

    #[test]
    fn test_index_ignore_empty_extensions_falls_back() {
        // Empty list in TOML falls back to defaults, not an empty list.
        let cfg = resolved_from_toml(
            r#"
            [index]
            extensions = []
        "#,
        );
        assert_eq!(cfg.extensions, vec![DEFAULT_INDEX_EXTENSION.to_string()]);
    }

    #[test]
    #[serial]
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
    #[serial]
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
    #[serial]
    fn test_resolved_derived_paths() {
        let cfg = resolved_from_toml(r#"state_dir = "/test""#);
        assert_eq!(cfg.state_dir.join("tsm.db"), PathBuf::from("/test/tsm.db"));
        assert_eq!(
            cfg.user_dict_path,
            PathBuf::from("/test/user_dict.simpledic")
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
    fn test_directory_weight_unknown() {
        // With no content_dirs configured, everything falls through to 1.0
        assert_eq!(directory_weight("unknown/path/file.md"), 1.0);
    }

    #[test]
    fn test_directory_weight_session() {
        assert_eq!(directory_weight("session:abc123"), DEFAULT_SESSION_WEIGHT);
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
    fn test_half_life_days_fallback() {
        // No content_dirs configured → falls back to source_type-based defaults
        assert_eq!(half_life_days("daily/notes/test.md", "note"), 120.0);
        assert_eq!(half_life_days("daily/research/r.md", "research"), 60.0);
        assert_eq!(
            half_life_days("session:abc", "session"),
            DEFAULT_SESSION_HALF_LIFE_DAYS
        );
        assert_eq!(
            half_life_days("unknown/path.md", "unknown"),
            DEFAULT_HALF_LIFE_DAYS
        );
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

    // ─── SearchFallback ────────────────────────────────────────────────

    #[test]
    fn test_search_fallback_default() {
        assert_eq!(SearchFallback::default(), SearchFallback::Error);
    }

    #[test]
    fn test_search_fallback_from_str() {
        assert_eq!(
            "error".parse::<SearchFallback>().unwrap(),
            SearchFallback::Error
        );
        assert_eq!(
            "fts_only".parse::<SearchFallback>().unwrap(),
            SearchFallback::FtsOnly
        );
        assert!("invalid".parse::<SearchFallback>().is_err());
    }

    #[test]
    fn test_search_fallback_display() {
        assert_eq!(SearchFallback::Error.to_string(), "error");
        assert_eq!(SearchFallback::FtsOnly.to_string(), "fts_only");
    }

    #[test]
    fn test_search_fallback_serde_roundtrip() {
        let val: SearchFallback = serde_json::from_str("\"error\"").unwrap();
        assert_eq!(val, SearchFallback::Error);
        let val: SearchFallback = serde_json::from_str("\"fts_only\"").unwrap();
        assert_eq!(val, SearchFallback::FtsOnly);
        assert!(serde_json::from_str::<SearchFallback>("\"bogus\"").is_err());
    }

    #[test]
    fn test_search_fallback_toml() {
        let cfg: ConfigFile = toml::from_str(r#"search_fallback = "fts_only""#).unwrap();
        assert_eq!(cfg.search_fallback, Some(SearchFallback::FtsOnly));

        let cfg: ConfigFile = toml::from_str(r#"search_fallback = "error""#).unwrap();
        assert_eq!(cfg.search_fallback, Some(SearchFallback::Error));

        assert!(toml::from_str::<ConfigFile>(r#"search_fallback = "nope""#).is_err());
    }

    #[test]
    #[serial]
    fn test_resolved_search_fallback_default() {
        std::env::remove_var("TSM_SEARCH_FALLBACK");
        let cfg = resolved_from_toml("");
        assert_eq!(cfg.search_fallback, SearchFallback::Error);
    }

    #[test]
    #[serial]
    fn test_resolved_search_fallback_from_config() {
        std::env::remove_var("TSM_SEARCH_FALLBACK");
        let cfg = resolved_from_toml(r#"search_fallback = "fts_only""#);
        assert_eq!(cfg.search_fallback, SearchFallback::FtsOnly);
    }

    #[test]
    #[serial]
    fn test_resolved_search_fallback_env_overrides_config() {
        std::env::set_var("TSM_SEARCH_FALLBACK", "fts_only");
        let cfg = resolved_from_toml(r#"search_fallback = "error""#);
        std::env::remove_var("TSM_SEARCH_FALLBACK");
        assert_eq!(cfg.search_fallback, SearchFallback::FtsOnly);
    }

    #[test]
    #[serial]
    fn test_resolved_search_fallback_invalid_env_falls_back() {
        std::env::set_var("TSM_SEARCH_FALLBACK", "bogus");
        let cfg = resolved_from_toml(r#"search_fallback = "fts_only""#);
        std::env::remove_var("TSM_SEARCH_FALLBACK");
        // Invalid env → falls back to config file value
        assert_eq!(cfg.search_fallback, SearchFallback::FtsOnly);
    }

    // ─── content_dirs config ────────────────────────────────────────

    #[test]
    fn test_content_dirs_from_toml() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "daily/notes"
weight = 1.2
half_life_days = 120

[[index.content_dirs]]
path = "company/knowledge"
weight = 1.3
"#,
        );
        assert_eq!(cfg.content_dirs.len(), 2);
        // Sorted longest-first: "company/knowledge" (19) > "daily/notes" (11)
        assert_eq!(cfg.content_dirs[0].path, "company/knowledge");
        assert_eq!(cfg.content_dirs[0].weight, 1.3);
        assert_eq!(cfg.content_dirs[0].half_life_days, DEFAULT_HALF_LIFE_DAYS);
        assert_eq!(cfg.content_dirs[1].path, "daily/notes");
        assert_eq!(cfg.content_dirs[1].weight, 1.2);
        assert_eq!(cfg.content_dirs[1].half_life_days, 120.0);
    }

    #[test]
    fn test_content_dirs_defaults_empty() {
        let cfg = resolved_from_toml("");
        assert!(cfg.content_dirs.is_empty());
        assert_eq!(cfg.session_weight, DEFAULT_SESSION_WEIGHT);
        assert_eq!(cfg.session_half_life_days, DEFAULT_SESSION_HALF_LIFE_DAYS);
    }

    #[test]
    fn test_claude_session_from_toml() {
        let cfg = resolved_from_toml(
            r#"
[index.claude_session]
weight = 0.5
half_life_days = 14
"#,
        );
        assert_eq!(cfg.session_weight, 0.5);
        assert_eq!(cfg.session_half_life_days, 14.0);
    }

    #[test]
    fn test_content_dirs_weight_defaults() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "daily/notes"
"#,
        );
        assert_eq!(cfg.content_dirs.len(), 1);
        assert_eq!(cfg.content_dirs[0].weight, 1.0);
        assert_eq!(cfg.content_dirs[0].half_life_days, DEFAULT_HALF_LIFE_DAYS);
    }

    #[test]
    fn test_content_dirs_prefix_specificity() {
        // More-specific path should match before shorter prefix
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "company"
weight = 1.0

[[index.content_dirs]]
path = "company/knowledge"
weight = 1.5
"#,
        );
        // Sorted longest-first
        assert_eq!(cfg.content_dirs[0].path, "company/knowledge");
        assert_eq!(cfg.content_dirs[1].path, "company");
    }

    #[test]
    fn test_content_dirs_validation_empty_path_skipped() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = ""

[[index.content_dirs]]
path = "daily/notes"
"#,
        );
        assert_eq!(cfg.content_dirs.len(), 1);
        assert_eq!(cfg.content_dirs[0].path, "daily/notes");
    }

    #[test]
    fn test_content_dirs_validation_absolute_path_skipped() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "/etc/passwd"

[[index.content_dirs]]
path = "daily/notes"
"#,
        );
        assert_eq!(cfg.content_dirs.len(), 1);
        assert_eq!(cfg.content_dirs[0].path, "daily/notes");
    }

    #[test]
    fn test_content_dirs_validation_negative_weight_clamped() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "daily/notes"
weight = -1.0
"#,
        );
        assert_eq!(cfg.content_dirs[0].weight, 1.0);
    }

    #[test]
    fn test_content_dirs_validation_zero_half_life_clamped() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "daily/notes"
half_life_days = 0.0
"#,
        );
        assert_eq!(cfg.content_dirs[0].half_life_days, DEFAULT_HALF_LIFE_DAYS);
    }

    #[test]
    fn test_directory_weight_with_config() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "company/knowledge"
weight = 1.5
"#,
        );
        // Simulate directory_weight logic against config
        let file_path = "company/knowledge/foo.md";
        let weight = cfg
            .content_dirs
            .iter()
            .find(|d| {
                file_path.starts_with(d.path.as_str())
                    && file_path.as_bytes().get(d.path.len()) == Some(&b'/')
            })
            .map(|d| d.weight)
            .unwrap_or(1.0);
        assert_eq!(weight, 1.5);

        // Boundary: similar prefix should NOT match
        let file_path2 = "company/knowledge_extra/foo.md";
        let weight2 = cfg
            .content_dirs
            .iter()
            .find(|d| {
                file_path2.starts_with(d.path.as_str())
                    && file_path2.as_bytes().get(d.path.len()) == Some(&b'/')
            })
            .map(|d| d.weight)
            .unwrap_or(1.0);
        assert_eq!(weight2, 1.0);
    }

    #[test]
    fn test_half_life_days_with_config() {
        let cfg = resolved_from_toml(
            r#"
[[index.content_dirs]]
path = "daily/notes"
half_life_days = 180
"#,
        );
        let file_path = "daily/notes/test.md";
        let hl = cfg
            .content_dirs
            .iter()
            .find(|d| {
                file_path.starts_with(d.path.as_str())
                    && file_path.as_bytes().get(d.path.len()) == Some(&b'/')
            })
            .map(|d| d.half_life_days)
            .unwrap_or(DEFAULT_HALF_LIFE_DAYS);
        assert_eq!(hl, 180.0);
    }

    // ─── reload ──────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_reload_updates_config() {
        // Force singleton initialization
        let _ = resolved();

        // reload should not panic even without tsm.toml changes
        let warnings = reload();
        // No structural fields changed, so no warnings
        assert!(warnings.is_empty());
    }

    #[test]
    #[serial]
    fn test_reload_warns_on_structural_changes() {
        // Force singleton initialization with current env
        let _ = resolved();

        // Set env vars to force structural field changes
        std::env::set_var("TSM_STATE_DIR", "/tmp/reload-test-state");
        std::env::set_var("TSM_INDEX_ROOT", "/tmp/reload-test-root");

        let warnings = reload();

        // Clean up env vars
        std::env::remove_var("TSM_STATE_DIR");
        std::env::remove_var("TSM_INDEX_ROOT");

        // Restore original config
        reload();

        // Should have warnings for state_dir and index_root (and derived paths)
        assert!(
            warnings.iter().any(|w| w.contains("state_dir")),
            "expected state_dir warning, got: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("index_root")),
            "expected index_root warning, got: {warnings:?}"
        );
        // All warnings should mention tsm restart
        for w in &warnings {
            assert!(
                w.contains("tsm restart"),
                "warning should mention tsm restart: {w}"
            );
        }
    }

    // ─── models_dir / models_dir_complete ──────────────────────────

    #[test]
    #[serial]
    fn test_models_dir_default() {
        std::env::remove_var("TSM_STATE_DIR");
        reload();
        let dir = models_dir();
        assert_eq!(dir, PathBuf::from(".tsm/models/ruri-v3-30m"));
    }

    #[test]
    #[serial]
    fn test_models_dir_with_custom_state_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", tmp.path());
        reload();
        let dir = models_dir();
        assert_eq!(dir, tmp.path().join("models/ruri-v3-30m"));
        std::env::remove_var("TSM_STATE_DIR");
        reload();
    }

    #[test]
    #[serial]
    fn test_models_dir_complete_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", tmp.path());
        reload();
        let dir = models_dir();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(models_dir_complete().is_none());
        std::env::remove_var("TSM_STATE_DIR");
        reload();
    }

    #[test]
    #[serial]
    fn test_models_dir_complete_partial() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", tmp.path());
        reload();
        let dir = models_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        assert!(models_dir_complete().is_none());
        std::env::remove_var("TSM_STATE_DIR");
        reload();
    }

    #[test]
    #[serial]
    fn test_models_dir_complete_all_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", tmp.path());
        reload();
        let dir = models_dir();
        std::fs::create_dir_all(&dir).unwrap();
        for f in &MODEL_FILES {
            std::fs::write(dir.join(f), "dummy").unwrap();
        }
        let result = models_dir_complete();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), dir);
        std::env::remove_var("TSM_STATE_DIR");
        reload();
    }
}
