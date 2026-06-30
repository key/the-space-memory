//! Shared test utilities.
//!
//! - `setup_db()` — in-memory DB with schema applied.
//! - `setup_db_with_dir()` — in-memory DB + temp directory for file operations.
//! - `CwdGuard` — RAII guard that restores the process CWD on drop.

/// Create an in-memory database with full schema applied.
pub fn setup_db() -> rusqlite::Connection {
    crate::db::get_memory_connection().unwrap()
}

/// Create an in-memory database and a temp directory for file-based tests.
pub fn setup_db_with_dir() -> (rusqlite::Connection, tempfile::TempDir) {
    (setup_db(), tempfile::tempdir().unwrap())
}

/// RAII guard for the process-global current working directory.
///
/// `change_to` saves the current CWD and switches to `new_cwd`; `Drop` restores
/// the saved CWD. Because the CWD is process-global, a test that points it at a
/// tempdir must restore it — otherwise the dropped tempdir leaves the CWD on a
/// removed inode and a later test's `current_dir()` fails (the #320 flake).
/// Restoring on drop makes this panic-safe (the CWD is restored even if an
/// assertion unwinds), and the save tolerates an already-dangling CWD (falls
/// back to the temp dir) so the guard never panics on construction.
pub struct CwdGuard {
    prev: std::path::PathBuf,
}

impl CwdGuard {
    /// Save the current CWD and switch to `new_cwd`.
    pub fn change_to(new_cwd: &std::path::Path) -> Self {
        let prev = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        std::env::set_current_dir(new_cwd).expect("set_current_dir to new_cwd");
        Self { prev }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev).or_else(|_| std::env::set_current_dir("/"));
    }
}
