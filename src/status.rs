use std::path::Path;

use serde::{Deserialize, Serialize};

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
    pub pid: u32,
}

pub fn status_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(STATUS_FILENAME)
}

pub fn read(data_dir: &Path) -> StatusFile {
    let path = status_path(data_dir);
    let Ok(s) = std::fs::read_to_string(&path) else {
        return StatusFile::default(); // File absent is normal
    };
    match serde_json::from_str(&s) {
        Ok(sf) => sf,
        Err(e) => {
            eprintln!(
                "warning: failed to parse status file ({}): {e}",
                path.display()
            );
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
pub fn update(data_dir: &Path, f: impl FnOnce(&mut StatusFile)) {
    let path = status_path(data_dir);
    let mut status = read(data_dir);
    f(&mut status);
    match serde_json::to_string_pretty(&status) {
        Ok(json) => {
            if let Err(e) = write_atomic(&path, json.as_bytes()) {
                eprintln!("warning: failed to write status file: {e}");
            }
        }
        Err(e) => eprintln!("warning: failed to serialize status: {e}"),
    }
}
