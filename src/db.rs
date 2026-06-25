use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, OpenFlags};

use crate::config;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    file_path TEXT UNIQUE NOT NULL,
    source_type TEXT NOT NULL,
    title TEXT,
    status TEXT,
    created TEXT,
    updated TEXT,
    tags TEXT,
    file_hash TEXT NOT NULL,
    indexed_at TEXT NOT NULL,
    metadata TEXT
);

CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    section_path TEXT,
    content TEXT NOT NULL,
    content_hash TEXT,
    UNIQUE(document_id, chunk_index)
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    content,
    tokenize='unicode61'
);

CREATE TABLE IF NOT EXISTS entities (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    UNIQUE(name, entity_type)
);

CREATE TABLE IF NOT EXISTS chunk_entities (
    chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    PRIMARY KEY (chunk_id, entity_id)
);

CREATE TABLE IF NOT EXISTS entity_edges (
    entity_a INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    entity_b INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    occurrences INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (entity_a, entity_b, doc_id),
    CHECK (entity_a < entity_b)
);

CREATE TABLE IF NOT EXISTS document_links (
    doc_a_id  INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    doc_b_id  INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    link_type TEXT NOT NULL,
    strength  REAL NOT NULL,
    PRIMARY KEY (doc_a_id, doc_b_id, link_type),
    CHECK (doc_a_id < doc_b_id)
);

CREATE INDEX IF NOT EXISTS idx_doc_links_a ON document_links(doc_a_id);
CREATE INDEX IF NOT EXISTS idx_doc_links_b ON document_links(doc_b_id);

CREATE TABLE IF NOT EXISTS synonyms (
    word_a  TEXT NOT NULL,
    word_b  TEXT NOT NULL,
    score   REAL NOT NULL DEFAULT 0.1,
    source  TEXT NOT NULL,
    hits    INTEGER DEFAULT 0,
    last_hit TEXT,
    created TEXT NOT NULL,
    PRIMARY KEY (word_a, word_b),
    CHECK (word_a < word_b)
);

CREATE INDEX IF NOT EXISTS idx_synonyms_a ON synonyms(word_a);
CREATE INDEX IF NOT EXISTS idx_synonyms_b ON synonyms(word_b);

CREATE TABLE IF NOT EXISTS dictionary_candidates (
    surface     TEXT PRIMARY KEY,
    frequency   INTEGER NOT NULL DEFAULT 1,
    pos         TEXT NOT NULL,
    source      TEXT NOT NULL,
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
                    CHECK(status IN ('pending', 'rejected', 'accepted')),
    reading     TEXT
);

CREATE INDEX IF NOT EXISTS idx_dict_candidates_status_freq
    ON dictionary_candidates(status, frequency DESC);

CREATE INDEX IF NOT EXISTS idx_chunks_document_id ON chunks(document_id);

CREATE TABLE IF NOT EXISTS chunks_vec_skip (
    chunk_id INTEGER PRIMARY KEY,
    reason   TEXT NOT NULL DEFAULT 'encode_error',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

static VEC_INIT: Once = Once::new();

/// Register sqlite-vec as an auto-extension (idempotent, process-wide).
pub fn ensure_vec_extension() {
    VEC_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::ffi::c_int,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

/// Create the chunks_vec virtual table if sqlite-vec is available.
fn create_vec_table(conn: &Connection) {
    let sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(embedding float[{}])",
        config::EMBEDDING_DIM
    );
    let _ = conn.execute_batch(&sql);
}

/// Busy timeout (ms) applied to every connection. Turns the rare SQLITE_BUSY
/// (WAL checkpoint, writer-vs-writer) into a bounded retry instead of an error.
pub const BUSY_TIMEOUT_MS: u64 = 5000;

fn apply_pragmas(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout={BUSY_TIMEOUT_MS};",
    ))?;
    Ok(())
}

/// Initialize the database schema at the given path.
pub fn init_db(db_path: &Path) -> anyhow::Result<()> {
    ensure_vec_extension();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    apply_pragmas(&conn)?;
    conn.execute_batch(SCHEMA_SQL)?;
    create_vec_table(&conn);
    Ok(())
}

/// Check whether a table with the given name exists.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Check whether the database has been initialized with the core schema.
///
/// A freshly created (empty) DB file has no tables; `init` must be run before
/// the daemon can serve requests against it. Requires the core operational
/// tables `init_db` always creates: `documents`, `chunks`, and the FTS5
/// `chunks_fts` (so a DB that merely happens to have the first two is rejected).
/// `chunks_vec` is intentionally NOT required — it is created best-effort and may
/// be legitimately absent when the sqlite-vec extension is unavailable. Column-
/// and index-level identity is not checked here; schema drift is `rebuild`'s job.
pub fn is_initialized(conn: &Connection) -> bool {
    table_exists(conn, "documents")
        && table_exists(conn, "chunks")
        && table_exists(conn, "chunks_fts")
}

/// Whether the DB at `path` exists and carries the core schema, WITHOUT
/// creating it.
///
/// A missing file reports `Ok(false)` without opening anything, so starting an
/// uninitialized project never materializes the state directory (ADR-0008 —
/// `init` is an explicit, separate step). An existing file is opened read-write
/// but **without** the `CREATE` flag: this never creates a new DB, yet still
/// lets SQLite recover a hot WAL left by an unclean shutdown (a read-only open
/// would fail with `SQLITE_READONLY_RECOVERY` and falsely block startup).
/// Genuine open failures (permission denied, not-a-database) propagate as `Err`
/// so callers surface the real problem instead of misreporting "not
/// initialized". Used as a side-effect-free pre-flight gate before the daemon
/// creates its logger, lock, and socket.
pub fn probe_initialized(path: &Path) -> anyhow::Result<bool> {
    if !path.try_exists().unwrap_or(false) {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| anyhow::anyhow!("Failed to open DB at {}: {e}", path.display()))?;
    Ok(is_initialized(&conn))
}

/// The canonical "the DB has no schema yet" error, shared by the CLI and daemon
/// fail-fast guards so the user-facing wording stays identical in one place.
pub fn uninitialized_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "Database not initialized at {}. Run `tsm init` first.",
        path.display()
    )
}

/// Ensure the `content_hash` column exists on the `chunks` table (migration for older DBs).
///
/// No-op when the `chunks` table is absent: a freshly initialized DB already has
/// the column via `SCHEMA_SQL`, and an uninitialized DB has no table to migrate.
pub fn ensure_chunk_hash_column(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "chunks") {
        return Ok(());
    }
    let has: bool = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('chunks') WHERE name='content_hash'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !has {
        conn.execute("ALTER TABLE chunks ADD COLUMN content_hash TEXT", [])?;
    }
    Ok(())
}

/// Add the `documents.metadata` column to pre-existing DBs (no-op if present).
///
/// No-op when the `documents` table is absent: a freshly initialized DB already has
/// the column via `SCHEMA_SQL`, and an uninitialized DB has no table to migrate.
fn ensure_metadata_column(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "documents") {
        return Ok(());
    }
    let exists: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('documents') WHERE name = 'metadata'")?
        .exists([])?;
    if !exists {
        conn.execute("ALTER TABLE documents ADD COLUMN metadata TEXT", [])?;
    }
    Ok(())
}

/// Add the `dictionary_candidates.reading` column to pre-existing DBs (no-op if present).
///
/// No-op when the `dictionary_candidates` table is absent: a freshly initialized DB
/// already has the column via `SCHEMA_SQL`, and an uninitialized DB has no table to
/// migrate. Existing rows keep a NULL reading until set by an explicit `dict add`.
fn ensure_reading_column(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "dictionary_candidates") {
        return Ok(());
    }
    let exists: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('dictionary_candidates') WHERE name = 'reading'")?
        .exists([])?;
    if !exists {
        conn.execute(
            "ALTER TABLE dictionary_candidates ADD COLUMN reading TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Get a connection to the database at the given path.
pub fn get_connection(db_path: &Path) -> anyhow::Result<Connection> {
    ensure_vec_extension();
    let conn = Connection::open(db_path)?;
    apply_pragmas(&conn)?;
    ensure_chunk_hash_column(&conn)?;
    ensure_metadata_column(&conn)?;
    ensure_reading_column(&conn)?;
    Ok(conn)
}

/// Open a read-only connection for the daemon's reader pool.
///
/// Opened READ_WRITE (not READ_ONLY: a read-only handle on a WAL DB can fail to
/// recover a hot WAL with SQLITE_READONLY_RECOVERY), then constrained with
/// `PRAGMA query_only=ON` so writes are rejected. Runs the same idempotent
/// column migrations as `get_connection` so a reader opened before the writer
/// still sees `content_hash` / `metadata`.
pub fn get_read_connection(db_path: &Path) -> anyhow::Result<Connection> {
    ensure_vec_extension();
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    apply_pragmas(&conn)?;
    ensure_chunk_hash_column(&conn)?;
    ensure_metadata_column(&conn)?;
    ensure_reading_column(&conn)?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(conn)
}

/// Get an in-memory connection with schema applied (for testing).
pub fn get_memory_connection() -> anyhow::Result<Connection> {
    ensure_vec_extension();
    let conn = Connection::open_in_memory()?;
    apply_pragmas(&conn)?;
    conn.execute_batch(SCHEMA_SQL)?;
    create_vec_table(&conn);
    Ok(conn)
}

/// Check if synonyms table exists in the database.
pub fn has_synonyms_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='synonyms'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Check if document_links table exists in the database.
pub fn has_doc_links_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='document_links'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Check if entity tables exist in the database.
pub fn has_entity_tables(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entities'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Check if dictionary_candidates table exists in the database.
pub fn has_candidates_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dictionary_candidates'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Check if the chunks_vec table exists in the database.
pub fn has_vec_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='chunks_vec'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_creates_tables() {
        let conn = get_memory_connection().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"documents".to_string()));
        assert!(tables.contains(&"chunks".to_string()));
        assert!(tables.contains(&"chunks_fts".to_string()));
        assert!(tables.contains(&"entities".to_string()));
        assert!(tables.contains(&"chunk_entities".to_string()));
        assert!(tables.contains(&"entity_edges".to_string()));
        assert!(tables.contains(&"document_links".to_string()));
        assert!(tables.contains(&"synonyms".to_string()));
        assert!(tables.contains(&"dictionary_candidates".to_string()));
    }

    #[test]
    fn test_has_entity_tables() {
        let conn = get_memory_connection().unwrap();
        assert!(has_entity_tables(&conn));
    }

    #[test]
    fn test_has_doc_links_table() {
        let conn = get_memory_connection().unwrap();
        assert!(has_doc_links_table(&conn));
    }

    #[test]
    fn test_has_synonyms_table() {
        let conn = get_memory_connection().unwrap();
        assert!(has_synonyms_table(&conn));
    }

    #[test]
    fn test_has_candidates_table() {
        let conn = get_memory_connection().unwrap();
        assert!(has_candidates_table(&conn));
    }

    #[test]
    fn test_chunks_vec_table_created() {
        let conn = get_memory_connection().unwrap();
        assert!(has_vec_table(&conn));
    }

    #[test]
    fn test_vec_insert_and_query() {
        let conn = get_memory_connection().unwrap();
        // Insert a vector
        let embedding: Vec<f32> = (0..config::EMBEDDING_DIM)
            .map(|i| i as f32 / 256.0)
            .collect();
        let json = serde_json::to_string(&embedding).unwrap();
        conn.execute(
            "INSERT INTO chunks_vec(rowid, embedding) VALUES (?, ?)",
            rusqlite::params![1i64, json],
        )
        .unwrap();

        // Query it back
        let query_vec = serde_json::to_string(&embedding).unwrap();
        let (rowid, distance): (i64, f64) = conn
            .query_row(
                "SELECT rowid, distance FROM chunks_vec WHERE embedding MATCH ? ORDER BY distance LIMIT 1",
                rusqlite::params![query_vec],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rowid, 1);
        assert!(distance < 0.001); // Same vector, distance ≈ 0
    }

    #[test]
    fn test_idempotent() {
        let conn = get_memory_connection().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
    }

    #[test]
    fn test_wal_mode() {
        let conn = get_memory_connection().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(mode == "wal" || mode == "memory");
    }

    #[test]
    fn test_wal_mode_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        init_db(&db_path).unwrap();
        let conn = get_connection(&db_path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn test_foreign_keys_on() {
        let conn = get_memory_connection().unwrap();
        let fk: i32 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn test_row_access() {
        let conn = get_memory_connection().unwrap();
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at)
             VALUES ('test.md', 'note', 'Test', 'abc123', '2026-01-01')",
            [],
        )
        .unwrap();
        let (path, stype): (String, String) = conn
            .query_row(
                "SELECT file_path, source_type FROM documents WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(path, "test.md");
        assert_eq!(stype, "note");
    }

    #[test]
    fn test_has_vec_table() {
        let conn = get_memory_connection().unwrap();
        assert!(has_vec_table(&conn));
    }

    #[test]
    fn test_vec_table_in_file_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        init_db(&db_path).unwrap();
        let conn = get_connection(&db_path).unwrap();
        assert!(has_vec_table(&conn));
    }

    #[test]
    fn test_is_initialized_true_for_schema() {
        let conn = get_memory_connection().unwrap();
        assert!(is_initialized(&conn));
    }

    #[test]
    fn test_is_initialized_false_for_bare_db() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(!is_initialized(&conn));
    }

    #[test]
    fn test_is_initialized_false_without_fts_table() {
        // documents + chunks alone is not our schema: a real init also creates
        // chunks_fts. Reject a DB that merely happens to have those two tables.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (id INTEGER PRIMARY KEY);
             CREATE TABLE chunks (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        assert!(!is_initialized(&conn));
    }

    #[test]
    fn test_ensure_chunk_hash_column_noop_without_chunks_table() {
        // A bare DB has no `chunks` table; the migration must be a no-op, not an error.
        let conn = Connection::open_in_memory().unwrap();
        ensure_chunk_hash_column(&conn).unwrap();
    }

    #[test]
    fn test_get_connection_succeeds_on_uninitialized_db() {
        // An empty (uninitialized) DB file must open cleanly; init is a separate step.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("uninit.db");
        let conn = get_connection(&db_path).unwrap();
        assert!(!is_initialized(&conn));
    }

    #[test]
    fn test_probe_initialized_false_for_missing_file_without_creating_it() {
        // The whole point of the probe: report "not initialized" for an absent DB
        // WITHOUT materializing the file (which would leave a stray `.tsm`).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("missing.db");
        assert!(!probe_initialized(&db_path).unwrap());
        assert!(!db_path.exists(), "probe must not create the DB file");
    }

    #[test]
    fn test_probe_initialized_false_for_bare_db() {
        // An existing-but-schemaless DB file is not initialized.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bare.db");
        Connection::open(&db_path).unwrap(); // creates an empty file, no schema
        assert!(!probe_initialized(&db_path).unwrap());
    }

    #[test]
    fn test_probe_initialized_true_for_initialized_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ready.db");
        init_db(&db_path).unwrap();
        assert!(probe_initialized(&db_path).unwrap());
    }

    #[test]
    fn test_probe_initialized_errors_when_path_is_unopenable() {
        // An existing path that cannot be opened as a DB (here: a directory) must
        // propagate the real error, NOT be misreported as "not initialized".
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("not-a-db");
        std::fs::create_dir(&db_path).unwrap();
        assert!(probe_initialized(&db_path).is_err());
    }

    #[test]
    fn test_uninitialized_error_mentions_init() {
        let err = uninitialized_error(Path::new("/x/.tsm/tsm.db"));
        let msg = err.to_string();
        assert!(msg.contains("/x/.tsm/tsm.db"));
        assert!(msg.contains("Run `tsm init` first"));
    }

    #[test]
    fn test_memory_db_has_metadata_column() {
        let conn = get_memory_connection().unwrap();
        // Column exists and accepts JSON text.
        conn.execute(
            "INSERT INTO documents (file_path, source_type, file_hash, indexed_at, metadata)
             VALUES ('a.md', 'note', 'h', '2026-01-01', '{\"status\":\"current\"}')",
            [],
        )
        .unwrap();
        let got: Option<String> = conn
            .query_row(
                "SELECT metadata FROM documents WHERE file_path='a.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(got.as_deref(), Some("{\"status\":\"current\"}"));
    }

    #[test]
    fn test_memory_db_has_reading_column() {
        let conn = get_memory_connection().unwrap();
        let has: bool = conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('dictionary_candidates') WHERE name='reading'",
            )
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has, "fresh schema must include the reading column");
    }

    #[test]
    fn test_ensure_reading_column_adds_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        // Pre-migration schema: dictionary_candidates without `reading`.
        conn.execute_batch(
            "CREATE TABLE dictionary_candidates (
                surface TEXT PRIMARY KEY,
                frequency INTEGER NOT NULL DEFAULT 1,
                pos TEXT NOT NULL,
                source TEXT NOT NULL,
                first_seen TEXT NOT NULL,
                last_seen TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending');
             INSERT INTO dictionary_candidates
                VALUES ('w', 1, 'ascii', 'doc', 't', 't', 'pending');",
        )
        .unwrap();

        ensure_reading_column(&conn).unwrap();
        // Second call is a no-op (idempotent).
        ensure_reading_column(&conn).unwrap();

        let has: bool = conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('dictionary_candidates') WHERE name='reading'",
            )
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has);

        // Pre-existing rows carry a NULL reading.
        let reading: Option<String> = conn
            .query_row(
                "SELECT reading FROM dictionary_candidates WHERE surface='w'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(reading.is_none());
    }

    #[test]
    fn test_ensure_reading_column_noop_without_table() {
        let conn = Connection::open_in_memory().unwrap();
        // No dictionary_candidates table at all → no-op, no error.
        ensure_reading_column(&conn).unwrap();
    }

    #[test]
    fn test_busy_timeout_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        init_db(&path).unwrap();
        let conn = get_connection(&path).unwrap();
        let ms: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn test_read_connection_busy_timeout_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        init_db(&path).unwrap();
        let reader = get_read_connection(&path).unwrap();
        let ms: i64 = reader
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn test_read_connection_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        init_db(&path).unwrap();
        let reader = get_read_connection(&path).unwrap();
        let err = reader
            .execute(
                "INSERT INTO documents (file_path, source_type, file_hash, indexed_at) VALUES ('x', 'note', 'h', '2026-01-01')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("readonly") || err.to_string().to_lowercase().contains("read"),
            "expected a read-only rejection, got: {err}"
        );
    }

    #[test]
    fn test_read_connection_sees_snapshot_during_writer_delete() {
        // Reader must read chunks_vec without error while the writer holds a
        // transaction that has DELETEd from it (WAL snapshot isolation across the
        // vec0 virtual table's shadow tables).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        init_db(&path).unwrap();

        let writer = get_connection(&path).unwrap();
        writer
            .execute_batch("BEGIN; DELETE FROM chunks_vec; ")
            .unwrap(); // open write txn, not yet committed

        let reader = get_read_connection(&path).unwrap();
        let n: i64 = reader
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .expect("reader should read a snapshot, not error");
        assert_eq!(n, 0); // empty DB; the point is it returns, not the value

        writer.execute_batch("ROLLBACK;").unwrap();
    }
}
