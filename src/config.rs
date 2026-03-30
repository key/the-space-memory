use std::path::PathBuf;

pub const MAX_CHUNK_CHARS: usize = 800;
pub const RRF_K: f64 = 60.0;
pub const SCORE_THRESHOLD: f64 = 0.005;
pub const MAX_RESULTS: usize = 5;
pub const EMBEDDING_DIM: usize = 256;
pub const SOCKET_PATH: &str = "/tmp/tsm-embedder.sock";
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 90.0;
pub const SNIPPET_MAX_CHARS: usize = 200;
pub const MIN_SESSION_MESSAGE_LEN: usize = 10;
pub const BACKFILL_BATCH_SIZE: usize = 8;
pub const MAX_QUERY_EXPANSIONS: usize = 5;
pub const RECENT_DAYS: i64 = 30;
pub const EMBEDDER_IDLE_TIMEOUT_SECS: u64 = 600;

/// Load embedder idle timeout from config.
/// TSM_EMBEDDER_IDLE_TIMEOUT env > config file > default (600s). 0 = disable.
pub fn embedder_idle_timeout_secs() -> u64 {
    if let Ok(val) = std::env::var("TSM_EMBEDDER_IDLE_TIMEOUT") {
        if let Ok(n) = val.parse::<u64>() {
            return n;
        }
    }
    if let Some(val) = load_config_value("embedder_idle_timeout_secs") {
        if let Ok(n) = val.parse::<u64>() {
            return n;
        }
    }
    EMBEDDER_IDLE_TIMEOUT_SECS
}

pub const EMBEDDER_BACKFILL_INTERVAL_SECS: u64 = 300;

/// Load embedder backfill interval from config.
/// TSM_EMBEDDER_BACKFILL_INTERVAL env > config file > default (300s). 0 = disable.
pub fn embedder_backfill_interval_secs() -> u64 {
    if let Ok(val) = std::env::var("TSM_EMBEDDER_BACKFILL_INTERVAL") {
        if let Ok(n) = val.parse::<u64>() {
            return n;
        }
    }
    if let Some(val) = load_config_value("embedder_backfill_interval_secs") {
        if let Ok(n) = val.parse::<u64>() {
            return n;
        }
    }
    EMBEDDER_BACKFILL_INTERVAL_SECS
}

pub const DICT_CANDIDATE_FREQ_THRESHOLD: i64 = 5;
pub const WORKER_ENCODE_TIMEOUT_PER_ITEM_SECS: u64 = 5;
pub const WORKER_ENCODE_TIMEOUT_BASE_SECS: u64 = 10;
pub const MAX_WORKER_RESTARTS: usize = 3;

const DEFAULT_PROJECT_ROOT: &str = "/workspaces";
const DEFAULT_DB_NAME: &str = "tsm.db";

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

/// Resolve data_dir: TSM_DATA_DIR env > config file > CARGO_MANIFEST_DIR/data
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TSM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = load_config_value("data_dir") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
}

/// Resolve project_root: TSM_PROJECT_ROOT env > config file > default
pub fn project_root() -> PathBuf {
    if let Ok(root) = std::env::var("TSM_PROJECT_ROOT") {
        return PathBuf::from(root);
    }
    if let Some(root) = load_config_value("project_root") {
        return PathBuf::from(root);
    }
    PathBuf::from(DEFAULT_PROJECT_ROOT)
}

pub fn db_path() -> PathBuf {
    data_dir().join(DEFAULT_DB_NAME)
}

pub fn user_dict_path() -> PathBuf {
    data_dir().join("user_dict.csv")
}

pub fn custom_terms_path() -> PathBuf {
    data_dir().join("custom_terms.toml")
}

/// Load a value from config file.
/// Search order: TSM_CONFIG env > ./tsm.toml > ~/.config/tsm/config.toml
fn load_config_value(key: &str) -> Option<String> {
    let candidates = config_file_candidates();
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(table) = content.parse::<toml::Table>() {
                if let Some(val) = table.get(key) {
                    // Handle both string and non-string TOML values
                    let s = match val.as_str() {
                        Some(s) => s.to_string(),
                        None => val.to_string(),
                    };
                    return Some(s);
                }
            }
        }
    }
    None
}

fn config_file_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("TSM_CONFIG") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(PathBuf::from("tsm.toml"));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".config/tsm/config.toml"));
    }
    candidates
}

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
    fn test_project_root_default() {
        std::env::remove_var("TSM_PROJECT_ROOT");
        std::env::remove_var("TSM_CONFIG");
        let root = project_root();
        assert_eq!(root, PathBuf::from(DEFAULT_PROJECT_ROOT));
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
    fn test_embedder_idle_timeout_default() {
        std::env::remove_var("TSM_EMBEDDER_IDLE_TIMEOUT");
        std::env::remove_var("TSM_CONFIG");
        let timeout = embedder_idle_timeout_secs();
        assert_eq!(timeout, EMBEDDER_IDLE_TIMEOUT_SECS);
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
    fn test_embedder_backfill_interval_default() {
        std::env::remove_var("TSM_EMBEDDER_BACKFILL_INTERVAL");
        std::env::remove_var("TSM_CONFIG");
        let interval = embedder_backfill_interval_secs();
        assert_eq!(interval, EMBEDDER_BACKFILL_INTERVAL_SECS);
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
}
