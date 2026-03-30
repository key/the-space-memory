use std::path::Path;
use std::sync::Once;

use rusqlite::Connection;

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
    indexed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    section_path TEXT,
    content TEXT NOT NULL,
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
                    CHECK(status IN ('pending', 'rejected', 'accepted'))
);

CREATE INDEX IF NOT EXISTS idx_dict_candidates_status_freq
    ON dictionary_candidates(status, frequency DESC);

CREATE INDEX IF NOT EXISTS idx_chunks_document_id ON chunks(document_id);
"#;

static VEC_INIT: Once = Once::new();

/// Register sqlite-vec as an auto-extension (idempotent, process-wide).
pub fn ensure_vec_extension() {
    VEC_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut i8,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
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

fn apply_pragmas(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;",
    )?;
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

/// Get a connection to the database at the given path.
pub fn get_connection(db_path: &Path) -> anyhow::Result<Connection> {
    ensure_vec_extension();
    let conn = Connection::open(db_path)?;
    apply_pragmas(&conn)?;
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
}
