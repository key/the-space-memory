use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::daemon_protocol::ReindexKind;

const STATUS_FILENAME: &str = "tsm-status.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StatusFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backfill: Option<BackfillStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedder: Option<EmbedderStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<WatcherStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reindex: Option<ReindexStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackfillStatus {
    pub total: i64,
    pub filled: usize,
    pub errors: usize,
    pub started_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedderStatus {
    pub started_at: String,
    pub pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub started_at: String,
    pub pid: u32,
    pub socket: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WatcherStatus {
    pub started_at: String,
    #[serde(default)]
    pub pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReindexStatus {
    pub kind: ReindexKind,
    pub total: i64,
    pub processed: i64,
    pub errors: i64,
    pub started_at: String,
}

pub fn status_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(STATUS_FILENAME)
}

pub fn read(state_dir: &Path) -> StatusFile {
    let path = status_path(state_dir);
    let Ok(s) = std::fs::read_to_string(&path) else {
        return StatusFile::default(); // File absent is normal
    };
    match serde_json::from_str(&s) {
        Ok(sf) => sf,
        Err(e) => {
            log::warn!("failed to parse status file ({}): {e}", path.display());
            StatusFile::default()
        }
    }
}

/// Atomic write: write to tmp file then rename.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Update the status file by applying a mutation function.
/// Reads current state, applies the mutation, writes back atomically.
pub fn update(state_dir: &Path, f: impl FnOnce(&mut StatusFile)) {
    let path = status_path(state_dir);
    let mut status = read(state_dir);
    f(&mut status);
    match serde_json::to_string_pretty(&status) {
        Ok(json) => {
            if let Err(e) = write_atomic(&path, json.as_bytes()) {
                log::warn!("failed to write status file: {e}");
            }
        }
        Err(e) => log::warn!("failed to serialize status: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_status_path_joins_filename() {
        let dir = TempDir::new().unwrap();
        let p = status_path(dir.path());
        assert_eq!(p, dir.path().join(STATUS_FILENAME));
        assert!(p.ends_with(STATUS_FILENAME));
    }

    #[test]
    fn test_read_absent_file_returns_default() {
        // No status file written yet → a default (all-None) StatusFile.
        let dir = TempDir::new().unwrap();
        let sf = read(dir.path());
        assert!(sf.daemon.is_none());
        assert!(sf.embedder.is_none());
        assert!(sf.watcher.is_none());
        assert!(sf.backfill.is_none());
        assert!(sf.reindex.is_none());
    }

    #[test]
    fn test_read_invalid_json_returns_default() {
        // A corrupt file must degrade to default rather than propagate an error.
        let dir = TempDir::new().unwrap();
        std::fs::write(status_path(dir.path()), b"{ not valid json ]").unwrap();
        let sf = read(dir.path());
        assert!(sf.daemon.is_none());
    }

    #[test]
    fn test_update_then_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        update(dir.path(), |s| {
            s.daemon = Some(DaemonStatus {
                started_at: "2026-01-01T00:00:00Z".to_string(),
                pid: 4242,
                socket: "/tmp/d.sock".to_string(),
            });
        });
        // The status file now exists and round-trips back through read().
        assert!(status_path(dir.path()).exists());
        let sf = read(dir.path());
        let d = sf.daemon.expect("daemon status persisted");
        assert_eq!(d.pid, 4242);
        assert_eq!(d.socket, "/tmp/d.sock");
    }

    #[test]
    fn test_update_preserves_other_fields_and_overwrites_atomically() {
        // First write sets the embedder; a second update mutates the watcher
        // while the embedder is read back, mutated in place, and re-persisted.
        let dir = TempDir::new().unwrap();
        update(dir.path(), |s| {
            s.embedder = Some(EmbedderStatus {
                started_at: "t0".to_string(),
                pid: 7,
            });
        });
        update(dir.path(), |s| {
            s.watcher = Some(WatcherStatus {
                started_at: "t1".to_string(),
                pid: 8,
            });
        });
        let sf = read(dir.path());
        assert_eq!(sf.embedder.unwrap().pid, 7);
        assert_eq!(sf.watcher.unwrap().pid, 8);
        // The atomic-write temp file is renamed away, not left behind.
        assert!(!status_path(dir.path()).with_extension("json.tmp").exists());
    }

    #[test]
    fn test_write_atomic_writes_and_renames() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.json");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn test_write_atomic_propagates_error_on_bad_dir() {
        // Writing under a nonexistent directory surfaces the IO error rather
        // than silently succeeding.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing-subdir").join("out.json");
        assert!(write_atomic(&path, b"x").is_err());
    }
}
