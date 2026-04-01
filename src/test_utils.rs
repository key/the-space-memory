//! Shared test utilities.
//!
//! - `setup_db()` — in-memory DB with schema applied.
//! - `setup_db_with_dir()` — in-memory DB + temp directory for file operations.

/// Create an in-memory database with full schema applied.
pub fn setup_db() -> rusqlite::Connection {
    crate::db::get_memory_connection().unwrap()
}

/// Create an in-memory database and a temp directory for file-based tests.
pub fn setup_db_with_dir() -> (rusqlite::Connection, tempfile::TempDir) {
    (setup_db(), tempfile::tempdir().unwrap())
}
