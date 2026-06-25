use std::path::{Path, PathBuf};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::session_chunker::parse_session_jsonl;
use crate::user_dict;

pub mod walker;

pub use walker::ContentWalker;

mod embed;
pub use embed::{backfill_next_batch, backfill_vectors, BackfillStats, EncodeFn};

mod prepare;

mod persist;
pub use persist::{rebuild_fts, rebuild_fts_next_batch};

/// Decides whether a single path is allowed through the ingest pipeline.
///
/// This is the **correctness boundary** for "should this file reach the
/// index?" — it runs at the indexer's entry point (`index_all`) so no
/// caller can accidentally bypass it, and no future traversal source has
/// to remember to re-implement the check.
///
/// Caller-side pre-filters (the CLI stdin reader, the fs-watcher event
/// loop) exist purely as optimizations — to avoid IPC round-trips and
/// unnecessary work — not as correctness. If a caller forgets to
/// pre-filter, the indexer still enforces the policy.
///
/// Current impl: [`ContentWalker`] (forced excludes + `.tsmignore` +
/// extension allowlist). Future impls are expected for visibility layers
/// (see `products/the-space-memory/visibility.md`), plugin-provided
/// indexers (e.g. code/pdf/session), and content-type routing. When a
/// second implementation arrives, composition (chain-of-policies, all
/// must accept) becomes the natural next step.
pub trait IngestPolicy {
    /// Return true if `path` should proceed to indexing.
    /// Implementations must be side-effect-free and cheap to call.
    fn accepts(&self, path: &Path) -> bool;
}

#[derive(Debug, Default)]
pub struct IndexStats {
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            write!(s, "{b:02x}").unwrap();
            s
        })
}

fn file_hash(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let hash = Sha256::digest(&data);
    Ok(hex_encode(hash.as_slice()))
}

fn chunk_hash(content: &str) -> String {
    let hash = Sha256::digest(content.as_bytes());
    hex_encode(hash.as_slice())
}

fn directory_from_rel_path(rel_path: &str) -> String {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() >= 3 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        parts[0].to_string()
    }
}

use prepare::ChunkInput;

/// Index a single file. Returns true if the file was (re-)indexed, false if skipped.
pub fn index_file(
    conn: &Connection,
    file_path: &Path,
    project_root: &Path,
) -> anyhow::Result<bool> {
    // file_path is stored as a lexical absolute path (ADR-0017). The directory
    // label (used for source_type + chunking) is still derived from the
    // project_root-relative path so source-type classification is unaffected.
    let stored_path = crate::paths::absolutize(file_path, project_root)
        .to_string_lossy()
        .to_string();
    let rel_for_label = file_path
        .strip_prefix(project_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let directory = directory_from_rel_path(&rel_for_label);
    let filename = file_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let current_hash = file_hash(file_path)?;

    // Check existing record
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, file_hash FROM documents WHERE file_path = ?",
            [&stored_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    if let Some((_, ref old_hash)) = existing {
        if *old_hash == current_hash {
            return Ok(false); // unchanged
        }
    }

    // Hook API contract: extract hooks see the project-relative path as
    // `ctx.path` (unchanged by ADR-0017). Only DB persistence uses the absolute
    // `stored_path` (file identity). Keep these two separate.
    let prepared = prepare::prepare(file_path, &rel_for_label, &directory, &filename)?;

    let diff = persist::persist(
        conn,
        &stored_path,
        &current_hash,
        existing.map(|(id, _)| id),
        &prepared,
    )?;

    // Vector embedding outside transaction (socket I/O)
    if !diff.chunks_needing_vectors.is_empty() {
        embed::insert_vectors(conn, &diff.chunks_needing_vectors);
    }

    Ok(true)
}

/// Index all given files under `policy`. Returns stats.
///
/// `policy` is applied as the **correctness boundary** — any path for
/// which `policy.accepts(path) == false` is counted as skipped without
/// touching the file or the DB. Callers upstream may also pre-filter
/// (CLI stdin, fs-watcher) for latency, but that's optimization; this
/// is the non-bypassable gate.
pub fn index_all(
    conn: &Connection,
    file_paths: &[PathBuf],
    project_root: &Path,
    policy: &dyn IngestPolicy,
) -> anyhow::Result<IndexStats> {
    index_all_with_progress(conn, file_paths, project_root, policy, None)
}

/// Progress callback type for index_all_with_progress: (current, total, file_path).
pub type IndexProgressCb<'a> = &'a dyn Fn(usize, usize, &Path);

pub fn index_all_with_progress(
    conn: &Connection,
    file_paths: &[PathBuf],
    project_root: &Path,
    policy: &dyn IngestPolicy,
    progress_cb: Option<IndexProgressCb<'_>>,
) -> anyhow::Result<IndexStats> {
    let mut stats = IndexStats::default();
    let total = file_paths.len();

    for (i, fp) in file_paths.iter().enumerate() {
        if let Some(cb) = progress_cb {
            cb(i + 1, total, fp);
        }

        // Policy gate: applied BEFORE the existence check so paths the
        // user excluded can't trigger DB-side cleanup as a side effect
        // (existing orphans from newly-added ignore rules are handled
        // explicitly by `tsm rebuild`, not implicitly here).
        if !policy.accepts(fp) {
            log::debug!("policy rejected {}; skipping", fp.display());
            stats.skipped += 1;
            continue;
        }

        if !fp.exists() {
            // Match the absolute key written by index_file (ADR-0017).
            let stored_path = crate::paths::absolutize(fp, project_root)
                .to_string_lossy()
                .to_string();
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM documents WHERE file_path = ?",
                    [&stored_path],
                    |row| row.get(0),
                )
                .ok();
            if let Some(doc_id) = existing {
                persist::delete_old_entries(conn, doc_id)?;
                stats.removed += 1;
            }
            continue;
        }

        if index_file(conn, fp, project_root)? {
            stats.indexed += 1;
        } else {
            stats.skipped += 1;
        }
    }

    Ok(stats)
}

/// Index a session JSONL file.
pub fn index_session(conn: &Connection, jsonl_path: &Path) -> anyhow::Result<bool> {
    let file_key = format!(
        "session:{}",
        jsonl_path.file_stem().unwrap_or_default().to_string_lossy()
    );
    let current_hash = file_hash(jsonl_path)?;

    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, file_hash FROM documents WHERE file_path = ?",
            [&file_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    if let Some((_, ref old_hash)) = existing {
        if *old_hash == current_hash {
            return Ok(false);
        }
    }

    let chunks = parse_session_jsonl(jsonl_path)?;
    if chunks.is_empty() {
        return Ok(false);
    }

    let now = chrono::Utc::now().to_rfc3339();
    // Use conversation timestamps from JSONL (first/last chunk) instead of index time
    let created = chunks
        .iter()
        .filter_map(|c| c.timestamp.as_deref())
        .next()
        .unwrap_or(&now);
    let updated = chunks
        .iter()
        .filter_map(|c| c.timestamp.as_deref())
        .next_back()
        .unwrap_or(&now);

    // Build chunk inputs with content hashes
    let chunk_inputs: Vec<ChunkInput> = chunks
        .iter()
        .map(|c| ChunkInput {
            chunk_index: c.chunk_index,
            section_path: "session".to_string(),
            content: c.content.clone(),
            content_hash: chunk_hash(&c.content),
        })
        .collect();

    let title = jsonl_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let tx = conn.unchecked_transaction()?;

    let doc_id = if let Some((doc_id, _)) = existing {
        // metadata intentionally omitted: sessions have no frontmatter;
        // searcher synthesizes scoring from status/updated when metadata is NULL.
        tx.execute(
            "UPDATE documents SET source_type=?, title=?, status=?, created=?, updated=?, tags=?, file_hash=?, indexed_at=?
             WHERE id=?",
            rusqlite::params![
                "session", title, "current", created, updated,
                Option::<String>::None, current_hash, &now, doc_id,
            ],
        )?;
        doc_id
    } else {
        tx.execute(
            "INSERT INTO documents (file_path, source_type, title, status, created, updated, tags, file_hash, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                file_key, "session", title, "current", created, updated,
                Option::<String>::None, current_hash, &now,
            ],
        )?;
        tx.last_insert_rowid()
    };

    let diff = persist::diff_chunks(&tx, doc_id, &chunk_inputs)?;

    // Note: entity graph and doc_links are not rebuilt for sessions.
    // Sessions don't participate in entity co-occurrence or link graphs.

    tx.commit()?;

    // Vector embedding outside transaction (socket I/O)
    if !diff.chunks_needing_vectors.is_empty() {
        embed::insert_vectors(conn, &diff.chunks_needing_vectors);
    }

    // Learn synonyms from human messages in the session (wrapped in transaction)
    learn_from_session_jsonl(conn, jsonl_path);

    Ok(true)
}

/// A session document plus its chunks, captured from the DB so a destructive
/// rebuild can carry it forward. Sessions are keyed `session:<stem>` — a
/// non-filesystem document kind ingested only via `index_session`. The file
/// walker only ever yields real paths under `project_root`, so it can never
/// reproduce a `session:` document regardless of the extension allowlist;
/// without this carry-forward they would be lost on `tsm rebuild --apply`.
pub(crate) struct CapturedSession {
    file_path: String,
    title: Option<String>,
    status: Option<String>,
    created: Option<String>,
    updated: Option<String>,
    tags: Option<String>,
    file_hash: String,
    chunks: Vec<ChunkInput>,
}

/// Read every `session:*` document and its chunks from `conn`.
///
/// Called before a destructive rebuild deletes the DB. The chunk content is
/// authoritative in the DB, so carry-forward needs neither the original JSONL
/// (which may have been rotated out of `~/.claude/projects`) nor a schema change.
pub(crate) fn capture_sessions(conn: &Connection) -> anyhow::Result<Vec<CapturedSession>> {
    let mut stmt = conn.prepare(
        "SELECT id, file_path, title, status, created, updated, tags, file_hash
         FROM documents WHERE file_path LIKE 'session:%' ORDER BY file_path",
    )?;
    // (doc_id, partial CapturedSession with chunks filled in below).
    let partials = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, captured_session_from_row(row)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut sessions = Vec::with_capacity(partials.len());
    for (doc_id, mut session) in partials {
        session.chunks = read_session_chunks(conn, doc_id)?;
        sessions.push(session);
    }
    Ok(sessions)
}

/// Map a `documents` row (sans chunks) into a `CapturedSession`.
fn captured_session_from_row(row: &rusqlite::Row) -> rusqlite::Result<CapturedSession> {
    Ok(CapturedSession {
        file_path: row.get(1)?,
        title: row.get(2)?,
        status: row.get(3)?,
        created: row.get(4)?,
        updated: row.get(5)?,
        tags: row.get(6)?,
        file_hash: row.get(7)?,
        chunks: Vec::new(),
    })
}

/// Read the chunks of one document as `ChunkInput`s, ordered by chunk index.
fn read_session_chunks(conn: &Connection, doc_id: i64) -> anyhow::Result<Vec<ChunkInput>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_index, section_path, content, content_hash
         FROM chunks WHERE document_id = ? ORDER BY chunk_index",
    )?;
    let chunks = stmt
        .query_map([doc_id], |row| {
            Ok(ChunkInput {
                chunk_index: row.get::<_, i64>(0)? as usize,
                section_path: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                content: row.get::<_, String>(2)?,
                content_hash: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(chunks)
}

/// Re-persist captured sessions into a freshly-initialized DB. Mirrors the
/// persist path of `index_session` (document row + `diff_chunks` for chunks and
/// FTS) without touching the JSONL. Vectors are intentionally left to the
/// post-rebuild backfill, which embeds every chunk lacking a vector. Returns the
/// number of session documents restored.
///
/// All sessions are restored in a single transaction: a failure on any session
/// rolls back the whole batch rather than leaving the DB partially populated.
/// This matters because the rebuild has already deleted the old DB by this
/// point — an all-or-nothing restore keeps the failure recoverable from the
/// backup instead of silently dropping a subset of sessions.
pub(crate) fn restore_sessions(
    conn: &Connection,
    sessions: &[CapturedSession],
) -> anyhow::Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;
    for session in sessions {
        tx.execute(
            "INSERT INTO documents (file_path, source_type, title, status, created, updated, tags, file_hash, indexed_at)
             VALUES (?, 'session', ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                session.file_path,
                session.title,
                session.status,
                session.created,
                session.updated,
                session.tags,
                session.file_hash,
                &now,
            ],
        )?;
        let doc_id = tx.last_insert_rowid();
        persist::diff_chunks(&tx, doc_id, &session.chunks)?;
    }
    tx.commit()?;
    Ok(sessions.len())
}

/// Extract human messages from session JSONL and learn synonym pairs.
fn learn_from_session_jsonl(conn: &Connection, jsonl_path: &Path) {
    use std::io::BufRead;

    let file = match std::fs::File::open(jsonl_path) {
        Ok(f) => f,
        Err(_) => return,
    };

    // Collect all user messages first, then batch-process in a transaction
    let mut messages: Vec<String> = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = val
            .pointer("/message/role")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role != "user" {
            continue;
        }
        let content_val = val.pointer("/message/content");
        let content = match content_val {
            Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
            Some(v) if v.is_array() => {
                // Handle [{type: "text", text: "..."}, ...] format
                v.as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        if content.len() >= 4 {
            messages.push(content);
        }
    }

    if messages.is_empty() {
        return;
    }

    // Wrap all synonym/dictionary upserts in a single transaction
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(_) => return,
    };
    for content in &messages {
        crate::synonyms::learn_from_message(&tx, content, "chat");
        user_dict::collect_from_text(&tx, content, "session");
    }
    if let Err(e) = tx.commit() {
        log::error!("learn_from_session_jsonl: transaction commit failed: {e}");
    }
}

pub use crate::config::BACKFILL_BATCH_SIZE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db;

    /// Test-only policy that accepts every path.
    /// Used by tests whose focus is indexer internals (chunking, FTS,
    /// vector backfill, etc.), not the policy gate itself. Policy
    /// behavior is covered by `indexer::walker::tests` and the daemon
    /// integration tests.
    struct AcceptAll;
    impl IngestPolicy for AcceptAll {
        fn accepts(&self, _: &Path) -> bool {
            true
        }
    }

    /// Test-only policy that rejects every path. Used to verify the
    /// `index_all` gate actually calls `policy.accepts()` and honors
    /// its result — a property the existing walker-through-daemon tests
    /// can't prove, because the walker already pre-filters the inputs.
    struct RejectAll;
    impl IngestPolicy for RejectAll {
        fn accepts(&self, _: &Path) -> bool {
            false
        }
    }
    use crate::test_utils::setup_db_with_dir as setup;
    use std::io::Write;

    fn write_md(dir: &Path, rel_path: &str, content: &str) -> PathBuf {
        let full = dir.join(rel_path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        full
    }

    #[test]
    fn index_file_stores_absolute_path() {
        let conn = crate::db::get_memory_connection().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("note.md");
        std::fs::write(&f, "# Title\n\nbody").unwrap();
        index_file(&conn, &f, dir.path()).unwrap();
        let stored: String = conn
            .query_row("SELECT file_path FROM documents LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, f.to_string_lossy());
        assert!(std::path::Path::new(&stored).is_absolute());
    }

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        // SHA-256 of empty input
        let hash = Sha256::digest(b"");
        assert_eq!(
            hex_encode(hash.as_slice()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_index_new_file() {
        let (conn, dir) = setup();
        let md =
            "---\nstatus: current\ncreated: 2026-01-01\ntags: [test]\n---\n\n# Hello\n\nWorld.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", md);
        let result = index_file(&conn, &path, dir.path()).unwrap();
        assert!(result);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunk_count >= 1);

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, chunk_count);
    }

    #[test]
    fn test_skip_unchanged() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nWorld.\n");
        assert!(index_file(&conn, &path, dir.path()).unwrap());
        assert!(!index_file(&conn, &path, dir.path()).unwrap());
    }

    #[test]
    fn test_reindex_on_change() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nWorld.\n");
        assert!(index_file(&conn, &path, dir.path()).unwrap());

        // Modify the file
        std::fs::write(&path, "# Hello\n\nUpdated content.\n").unwrap();
        assert!(index_file(&conn, &path, dir.path()).unwrap());

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_index_session() {
        let (conn, dir) = setup();
        let jsonl = r#"{"message":{"role":"user","content":"テスト質問のテキストです。"}}
{"message":{"role":"assistant","content":"テスト回答のテキストです。"}}"#;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, jsonl).unwrap();

        assert!(index_session(&conn, &path).unwrap());

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let source_type: String = conn
            .query_row("SELECT source_type FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(source_type, "session");
    }

    /// Regression test for the rebuild data-loss bug: session documents are a
    /// non-filesystem kind (`session:<stem>`) ingested only via `index_session`.
    /// `cmd_rebuild` re-indexes via the file walker, which only yields real
    /// filesystem paths and so never reproduces a `session:` document, so a
    /// destructive rebuild used to drop them permanently. Carry-forward captures
    /// them from the old DB before deletion and re-persists them into the fresh
    /// DB without needing the original JSONL (which may be rotated).
    /// Read a session document's scalar fields, used to assert that carry-forward
    /// preserves every column by value (catching a positional `row.get` swap).
    fn session_doc_fields(
        conn: &Connection,
        file_path: &str,
    ) -> (String, String, String, String, String) {
        conn.query_row(
            "SELECT title, status, created, updated, file_hash FROM documents WHERE file_path = ?",
            [file_path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
    }

    /// Read a session's chunk contents ordered by chunk_index.
    fn session_chunk_contents(conn: &Connection, file_path: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT c.content FROM chunks c JOIN documents d ON c.document_id = d.id \
                 WHERE d.file_path = ? ORDER BY c.chunk_index",
            )
            .unwrap();
        stmt.query_map([file_path], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn test_sessions_survive_rebuild_via_carry_forward() {
        // Arrange: old DB with one session whose two Q&A pairs carry distinct
        // timestamps, so created != updated and there are two ordered chunks —
        // this lets the assertions catch a field transposition or chunk
        // mis-alignment that a single-chunk, no-timestamp fixture would hide.
        let (old_conn, dir) = setup();
        let jsonl = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"最初の質問のテキスト。"}}
{"timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":"最初の回答のテキスト。"}}
{"timestamp":"2026-02-02T00:00:00Z","message":{"role":"user","content":"次の質問のテキスト。"}}
{"timestamp":"2026-02-02T00:00:01Z","message":{"role":"assistant","content":"次の回答のテキスト。"}}"#;
        let path = dir.path().join("abc123.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        assert!(index_session(&old_conn, &path).unwrap());

        let old_fields = session_doc_fields(&old_conn, "session:abc123");
        let old_contents = session_chunk_contents(&old_conn, "session:abc123");
        assert_eq!(old_contents.len(), 2, "two Q&A pairs yield two chunks");
        assert_ne!(
            old_fields.2, old_fields.3,
            "fixture must have created != updated to detect a field swap"
        );

        let captured = capture_sessions(&old_conn).unwrap();
        assert_eq!(captured.len(), 1, "one session should be captured");

        // Act: a fresh DB stands in for rebuild's delete + reinit. Without
        // carry-forward the session is gone; restore_sessions brings it back.
        let new_conn = db::get_memory_connection().unwrap();
        let before: i64 = new_conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE file_path LIKE 'session:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            before, 0,
            "fresh DB starts with no sessions (the data loss)"
        );

        let restored = restore_sessions(&new_conn, &captured).unwrap();
        assert_eq!(restored, 1);

        // Assert: the document survives with every field preserved by value...
        let source_type: String = new_conn
            .query_row(
                "SELECT source_type FROM documents WHERE file_path = 'session:abc123'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source_type, "session");
        assert_eq!(
            session_doc_fields(&new_conn, "session:abc123"),
            old_fields,
            "title/status/created/updated/file_hash all preserved (no column swap)"
        );

        // ...its chunks survive in order with content aligned to their index...
        assert_eq!(
            session_chunk_contents(&new_conn, "session:abc123"),
            old_contents,
            "chunk contents carried forward in chunk_index order"
        );

        // ...and the FTS index is repopulated for those chunks.
        let fts_count: i64 = new_conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 2, "FTS rows re-created for both session chunks");
    }

    #[test]
    fn test_restore_sessions_multiple_keeps_chunks_per_document() {
        // Two distinct sessions must both restore, each owning only its chunks.
        let (old_conn, dir) = setup();
        for (stem, q) in [
            ("alpha", "アルファセッションの質問テキストです。"),
            ("beta", "ベータセッションの質問テキストです。"),
        ] {
            let jsonl = format!(
                r#"{{"message":{{"role":"user","content":"{q}"}}}}
{{"message":{{"role":"assistant","content":"対応する回答のテキストです。"}}}}"#
            );
            let path = dir.path().join(format!("{stem}.jsonl"));
            std::fs::write(&path, &jsonl).unwrap();
            index_session(&old_conn, &path).unwrap();
        }

        let captured = capture_sessions(&old_conn).unwrap();
        assert_eq!(captured.len(), 2, "both sessions captured");

        let new_conn = db::get_memory_connection().unwrap();
        assert_eq!(restore_sessions(&new_conn, &captured).unwrap(), 2);

        // Each restored session owns exactly its own chunk (no cross-attribution).
        for stem in ["alpha", "beta"] {
            let n: i64 = new_conn
                .query_row(
                    "SELECT COUNT(*) FROM chunks c JOIN documents d ON c.document_id = d.id \
                     WHERE d.file_path = ?",
                    [format!("session:{stem}")],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "session:{stem} keeps exactly its own chunk");
        }
    }

    #[test]
    fn test_capture_sessions_excludes_md_and_handles_empty() {
        let (conn, dir) = setup();

        // A markdown document must not be captured as a session.
        let md = write_md(dir.path(), "daily/notes/test.md", "# Title\n\nBody text.\n");
        index_file(&conn, &md, dir.path()).unwrap();

        let captured = capture_sessions(&conn).unwrap();
        assert!(captured.is_empty(), "md documents are not sessions");

        // Restoring an empty capture is a no-op that restores nothing.
        let fresh = db::get_memory_connection().unwrap();
        assert_eq!(restore_sessions(&fresh, &captured).unwrap(), 0);
        let docs: i64 = fresh
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(docs, 0);
    }

    #[test]
    fn test_capture_sessions_captures_only_sessions_when_mixed() {
        let (conn, dir) = setup();
        let md = write_md(dir.path(), "daily/notes/test.md", "# Title\n\nBody text.\n");
        index_file(&conn, &md, dir.path()).unwrap();

        let jsonl = r#"{"message":{"role":"user","content":"混在テストの質問文です。"}}
{"message":{"role":"assistant","content":"混在テストの回答文です。"}}"#;
        let path = dir.path().join("mixed.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        index_session(&conn, &path).unwrap();

        let captured = capture_sessions(&conn).unwrap();
        assert_eq!(captured.len(), 1, "only the session is captured");
        assert_eq!(captured[0].file_path, "session:mixed");
        assert!(!captured[0].chunks.is_empty());
    }

    #[test]
    fn test_deleted_file() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nWorld.\n");
        index_file(&conn, &path, dir.path()).unwrap();

        // Delete the file
        std::fs::remove_file(&path).unwrap();
        let stats = index_all(&conn, &[path], dir.path(), &AcceptAll).unwrap();
        assert_eq!(stats.removed, 1);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_fts_rowid_matches_chunk_id() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "# Title\n\nContent text.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();

        let chunk_id: i64 = conn
            .query_row("SELECT id FROM chunks LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let fts_rowid: i64 = conn
            .query_row("SELECT rowid FROM chunks_fts LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunk_id, fts_rowid);
    }

    #[test]
    fn test_frontmatter_saved_to_documents() {
        let (conn, dir) = setup();
        let md = "---\nstatus: outdated\ncreated: 2026-01-15\nupdated: 2026-03-20\n---\n\n# Doc\n\nText.\n";
        let path = write_md(dir.path(), "company/research/study.md", md);
        index_file(&conn, &path, dir.path()).unwrap();

        let (status, created, updated, source_type): (
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT status, created, updated, source_type FROM documents",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(status.as_deref(), Some("outdated"));
        assert!(created.unwrap().contains("2026"));
        assert!(updated.unwrap().contains("2026"));
        assert_eq!(source_type, "research");
    }

    // ─── backfill_vectors tests ───────────────────────────────────

    /// Clear all vectors so backfill tests start from a known state.
    /// Needed because index_file may insert vectors if the embedder daemon is running.
    fn clear_vectors(conn: &Connection) {
        let _ = conn.execute("DELETE FROM chunks_vec", []);
    }

    fn mock_encode(texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|_| {
                (0..config::EMBEDDING_DIM)
                    .map(|i| i as f32 / 256.0)
                    .collect()
            })
            .collect())
    }

    fn mock_encode_fail(_texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("encode failed")
    }

    #[test]
    fn test_backfill_no_missing() {
        let conn = db::get_memory_connection().unwrap();
        let stats = backfill_vectors(&conn, &mock_encode, BACKFILL_BATCH_SIZE, None).unwrap();
        assert_eq!(stats.filled, 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn test_backfill_fills_missing_vectors() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "# Hello\n\nSome content here.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn); // Ensure no vectors exist before backfill

        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunks > 0);

        let vecs_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs_before, 0);

        // Backfill
        let stats = backfill_vectors(&conn, &mock_encode, BACKFILL_BATCH_SIZE, None).unwrap();
        assert_eq!(stats.filled as i64, chunks);
        assert_eq!(stats.errors, 0);

        let vecs_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs_after, chunks);
    }

    #[test]
    fn test_backfill_idempotent() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nContent.\n");
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        let stats1 = backfill_vectors(&conn, &mock_encode, BACKFILL_BATCH_SIZE, None).unwrap();
        assert!(stats1.filled > 0);

        // Second run should find nothing to fill
        let stats2 = backfill_vectors(&conn, &mock_encode, BACKFILL_BATCH_SIZE, None).unwrap();
        assert_eq!(stats2.filled, 0);
        assert_eq!(stats2.errors, 0);
    }

    #[test]
    fn test_backfill_with_batch_size() {
        let (conn, dir) = setup();
        // Create multiple files to get several chunks
        for i in 0..5 {
            let md = format!("# Doc {i}\n\nContent for document number {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunks >= 5);

        // Use batch_size=2 to force multiple batches
        let stats = backfill_vectors(&conn, &mock_encode, 2, None).unwrap();
        assert_eq!(stats.filled as i64, chunks);

        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, chunks);
    }

    #[test]
    fn test_backfill_encode_error() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nContent.\n");
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        let stats = backfill_vectors(&conn, &mock_encode_fail, BACKFILL_BATCH_SIZE, None).unwrap();
        assert_eq!(stats.filled, 0);
        assert!(stats.errors > 0);
    }

    fn mock_encode_panic(_texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        panic!("simulated candle panic");
    }

    #[test]
    fn test_backfill_catches_panic() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nContent.\n");
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        let stats = backfill_vectors(&conn, &mock_encode_panic, BACKFILL_BATCH_SIZE, None).unwrap();
        assert_eq!(stats.filled, 0);
        assert!(stats.panics > 0, "should have caught a panic");
        assert!(stats.errors > 0, "panics should count as errors too");
    }

    #[test]
    fn test_backfill_continues_after_panic() {
        let (conn, dir) = setup();
        // Create multiple files to get multiple batches
        for i in 0..3 {
            let md = format!("# Doc {i}\n\nContent for doc {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        let call_count = std::sync::atomic::AtomicUsize::new(0);
        let panic_on_first = |texts: &[String]| -> anyhow::Result<Vec<Vec<f32>>> {
            let count = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                panic!("first batch panic");
            }
            mock_encode(texts)
        };

        // batch_size=1 so each chunk is its own batch
        let stats = backfill_vectors(&conn, &panic_on_first, 1, None).unwrap();
        assert!(stats.panics > 0, "should have caught at least 1 panic");
        assert!(
            stats.filled > 0,
            "should have filled some chunks after the panic"
        );
    }

    #[test]
    fn test_backfill_retry_individually_on_batch_error() {
        let (conn, dir) = setup();
        // Create 3 files → at least 3 chunks
        for i in 0..3 {
            let md = format!("# Doc {i}\n\nContent for document number {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunk_count >= 3, "need at least 3 chunks");

        // Fail on batch (len > 1), succeed on individual (len == 1)
        let encode_fail_batch = |texts: &[String]| -> anyhow::Result<Vec<Vec<f32>>> {
            if texts.len() > 1 {
                anyhow::bail!("batch error");
            }
            mock_encode(texts)
        };

        // Use batch_size > 1 to trigger batch failure + individual retry
        let stats = backfill_vectors(&conn, &encode_fail_batch, 8, None).unwrap();
        assert_eq!(
            stats.filled as i64, chunk_count,
            "all chunks should be filled via individual retry"
        );
        assert_eq!(stats.errors, 0, "no chunks should remain as errors");
    }

    #[test]
    fn test_backfill_skip_written_for_persistent_failure() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "# Hello\n\nContent here.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        // Always fail
        let stats = backfill_vectors(&conn, &mock_encode_fail, BACKFILL_BATCH_SIZE, None).unwrap();
        assert!(stats.errors > 0);

        // Skip records should have been written
        let skip_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec_skip", [], |r| r.get(0))
            .unwrap();
        assert!(
            skip_count > 0,
            "skip records should be written for failed chunks"
        );

        // chunks_vec should remain empty (no sentinel vectors polluting search)
        let vec_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            vec_count, 0,
            "no vectors should be written for failed chunks"
        );

        // A second run should find no missing vectors (skip table excludes them)
        let stats2 = backfill_vectors(&conn, &mock_encode_fail, BACKFILL_BATCH_SIZE, None).unwrap();
        assert_eq!(stats2.filled, 0, "no chunks should be retried after skip");
        assert_eq!(stats2.errors, 0, "no errors on second run");
    }

    #[test]
    fn test_backfill_count_excludes_skipped_chunks() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "# Hello\n\nContent here.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        // Mark all chunks as skipped via a persistently failing encode.
        let stats = backfill_vectors(&conn, &mock_encode_fail, BACKFILL_BATCH_SIZE, None).unwrap();
        assert!(stats.errors > 0);

        // A subsequent healthy pass must NOT report skip-marked chunks as
        // outstanding work: the progress total drives doctor/status display,
        // so counting skipped chunks there produces a permanent phantom "missing".
        let reported_totals = std::cell::RefCell::new(Vec::new());
        let cb = |total: i64, _filled: usize, _errors: usize| {
            reported_totals.borrow_mut().push(total);
        };
        backfill_vectors(&conn, &mock_encode, BACKFILL_BATCH_SIZE, Some(&cb)).unwrap();

        assert!(
            reported_totals.borrow().iter().all(|&t| t == 0),
            "skip-marked chunks must not be counted as missing, got totals: {:?}",
            reported_totals.borrow()
        );
    }

    #[test]
    fn test_backfill_embedding_count_mismatch_triggers_retry() {
        let (conn, dir) = setup();
        for i in 0..3 {
            let md = format!("# Doc {i}\n\nContent for doc number {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        // Return wrong number of embeddings for batch (> 1), correct for individual (== 1)
        let encode_mismatch = |texts: &[String]| -> anyhow::Result<Vec<Vec<f32>>> {
            if texts.len() > 1 {
                // Return only 1 embedding for a batch of N
                mock_encode(&texts[..1])
            } else {
                mock_encode(texts)
            }
        };

        let stats = backfill_vectors(&conn, &encode_mismatch, 8, None).unwrap();
        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            stats.filled as i64, chunk_count,
            "all chunks should be filled via individual retry after mismatch"
        );
    }

    #[test]
    fn test_backfill_skip_cleared_on_reindex() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "# Hello\n\nContent here.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        // Fail to create skip records
        backfill_vectors(&conn, &mock_encode_fail, BACKFILL_BATCH_SIZE, None).unwrap();
        let skip_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec_skip", [], |r| r.get(0))
            .unwrap();
        assert!(skip_before > 0);

        // Re-index same file with new content → old chunks deleted → skip records cleaned
        std::fs::write(&path, "# Updated\n\nNew content.\n").unwrap();
        index_file(&conn, &path, dir.path()).unwrap();
        // Clear vectors that may have been inserted by a running embedder daemon
        clear_vectors(&conn);

        let skip_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec_skip", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            skip_after, 0,
            "skip records should be cleaned up on re-index"
        );

        // Backfill should now succeed for the new chunks
        let stats = backfill_vectors(&conn, &mock_encode, BACKFILL_BATCH_SIZE, None).unwrap();
        assert!(stats.filled > 0, "new chunks should be backfilled");
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn test_backfill_batch_panic_retries_individually() {
        let (conn, dir) = setup();
        for i in 0..3 {
            let md = format!("# Doc {i}\n\nContent for document number {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        // Panic on batch (len > 1), succeed on individual (len == 1)
        let encode_panic_batch = |texts: &[String]| -> anyhow::Result<Vec<Vec<f32>>> {
            if texts.len() > 1 {
                panic!("batch panic");
            }
            mock_encode(texts)
        };

        let stats = backfill_vectors(&conn, &encode_panic_batch, 8, None).unwrap();
        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(stats.panics > 0, "should have caught batch panic");
        assert_eq!(
            stats.filled as i64, chunk_count,
            "all chunks should be filled via individual retry after batch panic"
        );
    }

    // ─── backfill_next_batch tests ────────────────────────────

    #[test]
    fn test_next_batch_empty_db() {
        let conn = db::get_memory_connection().unwrap();
        let (stats, has_more) = backfill_next_batch(&conn, &mock_encode, 8, 0).unwrap();
        assert_eq!(stats.filled, 0);
        assert!(!has_more);
    }

    #[test]
    fn test_next_batch_keyset_pagination() {
        let (conn, dir) = setup();
        for i in 0..3 {
            let md = format!("# Doc {i}\n\nContent for document number {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunks >= 3);

        // First batch: batch_size=2
        let (stats1, has_more1) = backfill_next_batch(&conn, &mock_encode, 2, 0).unwrap();
        assert_eq!(stats1.filled, 2);
        assert!(has_more1);
        assert!(stats1.last_id > 0);

        // Second batch: from last_id
        let (stats2, _has_more2) =
            backfill_next_batch(&conn, &mock_encode, 2, stats1.last_id).unwrap();
        assert!(stats2.filled > 0);
        assert!(stats2.last_id > stats1.last_id);

        // Continue until exhausted
        let mut last_id = stats2.last_id;
        loop {
            let (s, more) = backfill_next_batch(&conn, &mock_encode, 2, last_id).unwrap();
            if !more {
                break;
            }
            last_id = s.last_id;
        }

        // All vectors should be filled
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, chunks);
    }

    #[test]
    fn test_next_batch_encode_error_marks_skip() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nContent.\n");
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        let (stats, _) = backfill_next_batch(&conn, &mock_encode_fail, 8, 0).unwrap();
        assert_eq!(stats.filled, 0);
        assert!(stats.errors > 0);

        // Skip records should be written
        let skips: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec_skip", [], |r| r.get(0))
            .unwrap();
        assert!(skips > 0);

        // Next call should find nothing (skipped chunks excluded)
        let (stats2, has_more) = backfill_next_batch(&conn, &mock_encode, 8, 0).unwrap();
        assert_eq!(stats2.filled, 0);
        assert!(!has_more);
    }

    #[test]
    fn test_next_batch_partial_batch() {
        let (conn, dir) = setup();
        for i in 0..3 {
            let md = format!("# Doc {i}\n\nContent for document number {i}.\n");
            let path = write_md(dir.path(), &format!("daily/notes/test{i}.md"), &md);
            index_file(&conn, &path, dir.path()).unwrap();
        }
        clear_vectors(&conn);

        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();

        // Use a large batch_size that exceeds total chunks
        let (stats, has_more) =
            backfill_next_batch(&conn, &mock_encode, chunks as usize + 10, 0).unwrap();
        assert_eq!(stats.filled as i64, chunks);
        assert!(has_more); // non-empty batch always returns has_more=true

        // Next call finds nothing
        let (stats2, has_more2) =
            backfill_next_batch(&conn, &mock_encode, chunks as usize + 10, stats.last_id).unwrap();
        assert_eq!(stats2.filled, 0);
        assert!(!has_more2);
    }

    #[test]
    fn test_next_batch_catches_panic() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nContent.\n");
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);

        let (stats, _) = backfill_next_batch(&conn, &mock_encode_panic, 8, 0).unwrap();
        assert!(stats.panics > 0);
        assert!(stats.errors > 0);
    }

    // ─── entity integration tests ────────────────────────────

    #[test]
    fn test_entities_populated_after_index() {
        let (conn, dir) = setup();
        let md = "---\ntags: [Rust, 検索]\n---\n\n# 東京のメモ\n\n東京タワーは有名な観光地です。\n";
        let path = write_md(dir.path(), "daily/notes/test.md", md);
        index_file(&conn, &path, dir.path()).unwrap();

        let entity_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
            .unwrap();
        // At least tags (rust, 検索) should be present
        assert!(
            entity_count >= 2,
            "expected at least 2 entities, got {entity_count}"
        );

        let ce_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_entities", [], |r| r.get(0))
            .unwrap();
        assert!(ce_count > 0);
    }

    #[test]
    fn test_entity_data_cleaned_on_reindex() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "---\ntags: [OldTag]\n---\n\n# Doc\n\nContent.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();

        let old_ce: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_entities", [], |r| r.get(0))
            .unwrap();
        assert!(old_ce > 0);

        // Reindex with different tags
        std::fs::write(&path, "---\ntags: [NewTag]\n---\n\n# Doc\n\nNew content.\n").unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        // Only 1 document
        let doc_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(doc_count, 1);

        // Old chunk_entities should be gone, new ones present
        let new_ce: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_entities", [], |r| r.get(0))
            .unwrap();
        assert!(new_ce > 0);

        // "newtag" should exist
        let has_new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE name = 'newtag'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_new, 1);
    }

    #[test]
    fn test_entity_edges_cleaned_on_reindex() {
        let (conn, dir) = setup();
        let md = "---\ntags: [Rust, SQLite]\n---\n\n# ドキュメント\n\nタグのテスト。\n";
        let path = write_md(dir.path(), "daily/notes/test.md", md);
        index_file(&conn, &path, dir.path()).unwrap();

        // Verify rust-sqlite edge exists
        let has_rust_sqlite: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM entity_edges ee
                 JOIN entities ea ON ee.entity_a = ea.id
                 JOIN entities eb ON ee.entity_b = eb.id
                 WHERE (ea.name = 'rust' AND eb.name = 'sqlite')
                    OR (ea.name = 'sqlite' AND eb.name = 'rust')",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        assert!(
            has_rust_sqlite,
            "should have rust-sqlite co-occurrence edge"
        );

        // Reindex with completely different content
        std::fs::write(&path, "# シンプル\n\n本文だけ。\n").unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        // Old rust-sqlite edge should be gone
        let has_rust_sqlite_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entity_edges ee
                 JOIN entities ea ON ee.entity_a = ea.id
                 JOIN entities eb ON ee.entity_b = eb.id
                 WHERE (ea.name = 'rust' AND eb.name = 'sqlite')
                    OR (ea.name = 'sqlite' AND eb.name = 'rust')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_rust_sqlite_after, 0, "old edges should be cleaned");
    }

    // ─── dictionary candidates integration tests ─────────────

    #[test]
    fn test_candidates_collected_on_index() {
        let (conn, dir) = setup();
        let md = "---\nstatus: current\n---\n\n# candle Framework\n\ncandle is used for ML inference with lindera tokenization.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", md);
        index_file(&conn, &path, dir.path()).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            count > 0,
            "should collect dictionary candidates during indexing"
        );
    }

    #[test]
    fn test_index_all_with_progress_callback() {
        let (conn, dir) = setup();
        write_md(dir.path(), "daily/notes/a.md", "# A\n\nContent A.\n");
        write_md(dir.path(), "daily/notes/b.md", "# B\n\nContent B.\n");

        let files = vec![
            dir.path().join("daily/notes/a.md"),
            dir.path().join("daily/notes/b.md"),
        ];

        let calls = std::cell::RefCell::new(Vec::new());
        let cb = |current: usize, total: usize, _path: &std::path::Path| {
            calls.borrow_mut().push((current, total));
        };

        let stats =
            index_all_with_progress(&conn, &files, dir.path(), &AcceptAll, Some(&cb)).unwrap();
        assert_eq!(stats.indexed, 2);

        let calls = calls.into_inner();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], (1, 2));
        assert_eq!(calls[1], (2, 2));
    }

    #[test]
    fn test_index_all_with_progress_none() {
        let (conn, dir) = setup();
        write_md(dir.path(), "daily/notes/c.md", "# C\n\nContent C.\n");

        let files = vec![dir.path().join("daily/notes/c.md")];
        let stats = index_all_with_progress(&conn, &files, dir.path(), &AcceptAll, None).unwrap();
        assert_eq!(stats.indexed, 1);
    }

    /// Prove the policy gate actually gates: without it, a RejectAll
    /// policy would still index the file. Pins the contract that
    /// `index_all` calls `policy.accepts()` and honors rejection.
    #[test]
    fn test_index_all_policy_gate_skips_rejected_paths() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nWorld.\n");

        let stats = index_all(&conn, &[path], dir.path(), &RejectAll).unwrap();

        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.indexed, 0);
        assert_eq!(stats.removed, 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "rejected path must not reach the DB");
    }

    /// Pins the design invariant documented at `index_all_with_progress`:
    /// the policy gate fires BEFORE the existence check, so a path that
    /// the policy rejects never triggers the delete-stale-doc codepath
    /// as a side effect. Without this test, reordering the checks would
    /// silently change deletion semantics.
    #[test]
    fn test_index_all_policy_gate_fires_before_existence_check() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nWorld.\n");
        index_file(&conn, &path, dir.path()).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 1);

        // Delete on disk, then call index_all with a rejecting policy.
        std::fs::remove_file(&path).unwrap();
        let stats = index_all(&conn, &[path], dir.path(), &RejectAll).unwrap();

        assert_eq!(
            stats.removed, 0,
            "policy-rejected path must not trigger DB cleanup"
        );
        assert_eq!(stats.skipped, 1);
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 1, "existing row must survive — gate ran first");
    }

    /// Progress callback fires for every input path, including those
    /// rejected by the policy. This is an opinionated design choice
    /// (the caller sees a stable total count) — pin it so a well-meaning
    /// "only count processed items" refactor is caught.
    #[test]
    fn test_index_all_with_progress_callback_fires_for_rejected_paths() {
        struct RejectSecond {
            seen: std::cell::Cell<usize>,
        }
        impl IngestPolicy for RejectSecond {
            fn accepts(&self, _: &Path) -> bool {
                let n = self.seen.get();
                self.seen.set(n + 1);
                n != 1 // index #0 accepted, #1 rejected
            }
        }

        let (conn, dir) = setup();
        write_md(dir.path(), "daily/notes/a.md", "# A\n\nContent A.\n");
        write_md(dir.path(), "daily/notes/b.md", "# B\n\nContent B.\n");
        let files = vec![
            dir.path().join("daily/notes/a.md"),
            dir.path().join("daily/notes/b.md"),
        ];

        let calls = std::cell::RefCell::new(Vec::new());
        let cb = |current: usize, total: usize, _path: &std::path::Path| {
            calls.borrow_mut().push((current, total));
        };

        let policy = RejectSecond {
            seen: std::cell::Cell::new(0),
        };
        let stats = index_all_with_progress(&conn, &files, dir.path(), &policy, Some(&cb)).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.skipped, 1);

        let calls = calls.into_inner();
        assert_eq!(
            calls.len(),
            2,
            "callback must fire for every input path, policy decisions notwithstanding"
        );
    }

    #[test]
    fn test_candidates_collected_on_session_ingest() {
        let (conn, dir) = setup();
        let jsonl = r#"{"message":{"role":"user","content":"candle framework is great for lindera tokenization testing."}}"#;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, jsonl).unwrap();

        index_session(&conn, &path).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            count > 0,
            "should collect dictionary candidates during session ingest"
        );
    }

    // --- Incremental chunk-level diff tests ---

    fn get_chunk_ids(conn: &Connection, doc_id: i64) -> Vec<(i64, i64)> {
        conn.prepare(
            "SELECT id, chunk_index FROM chunks WHERE document_id = ? ORDER BY chunk_index",
        )
        .unwrap()
        .query_map([doc_id], |row| {
            Ok((row.get::<_, i64>(0).unwrap(), row.get::<_, i64>(1).unwrap()))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    fn get_doc_id(conn: &Connection) -> i64 {
        conn.query_row("SELECT id FROM documents LIMIT 1", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_content_hash_stored() {
        let (conn, dir) = setup();
        let path = write_md(
            dir.path(),
            "daily/notes/test.md",
            "# Title\n\nSome content here.\n",
        );
        index_file(&conn, &path, dir.path()).unwrap();

        let hash: String = conn
            .query_row("SELECT content_hash FROM chunks LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hash.len(), 64, "content_hash should be 64-char hex SHA-256");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_incremental_skip_unchanged_chunks() {
        let (conn, dir) = setup();
        let content = "# Doc\n\n## Section A\n\nContent A is here with enough text.\n\n## Section B\n\nContent B is here with enough text.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id = get_doc_id(&conn);
        let ids_before = get_chunk_ids(&conn, doc_id);
        assert!(ids_before.len() >= 2, "should have at least 2 chunks");

        // Re-write with trailing whitespace change (file_hash changes but chunk content same)
        let content2 = "# Doc\n\n## Section A\n\nContent A is here with enough text.\n\n## Section B\n\nContent B is here with enough text.\n\n";
        std::fs::write(&path, content2).unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        let ids_after = get_chunk_ids(&conn, doc_id);
        // Chunk IDs should be preserved (not deleted and re-created)
        assert_eq!(
            ids_before, ids_after,
            "unchanged chunk IDs should be preserved"
        );
    }

    #[test]
    fn test_incremental_update_changed_chunk() {
        let (conn, dir) = setup();
        let content = "# Doc\n\n## Section A\n\nContent A original text here.\n\n## Section B\n\nContent B original text here.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id = get_doc_id(&conn);
        let ids_before = get_chunk_ids(&conn, doc_id);

        // Modify section B only
        let content2 = "# Doc\n\n## Section A\n\nContent A original text here.\n\n## Section B\n\nContent B UPDATED text here.\n";
        std::fs::write(&path, content2).unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        let ids_after = get_chunk_ids(&conn, doc_id);
        assert_eq!(
            ids_before.len(),
            ids_after.len(),
            "chunk count should be same"
        );
        // Section A chunk (index 0) should keep same ID
        assert_eq!(
            ids_before[0].0, ids_after[0].0,
            "unchanged chunk A should keep its ID"
        );

        // Verify updated content is searchable in FTS
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks_fts WHERE content MATCH ?",
                ["UPDATED"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(fts_count > 0, "updated content should be searchable in FTS");

        // Verify old Section B content is no longer in FTS
        // (search for the combined unique phrase from old Section B)
        let old_b_id = ids_after.last().unwrap().0;
        let old_b_content: String = conn
            .query_row(
                "SELECT content FROM chunks_fts WHERE rowid = ?",
                [old_b_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !old_b_content.contains("original"),
            "updated chunk's FTS should not contain old text"
        );
    }

    #[test]
    fn test_incremental_insert_new_chunk() {
        let (conn, dir) = setup();
        let content = "# Doc\n\n## Section A\n\nContent A here with text.\n\n## Section B\n\nContent B here with text.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id = get_doc_id(&conn);
        let ids_before = get_chunk_ids(&conn, doc_id);
        let count_before = ids_before.len();

        // Add a third section
        let content2 = "# Doc\n\n## Section A\n\nContent A here with text.\n\n## Section B\n\nContent B here with text.\n\n## Section C\n\nNew content C here.\n";
        std::fs::write(&path, content2).unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        let ids_after = get_chunk_ids(&conn, doc_id);
        assert!(
            ids_after.len() > count_before,
            "should have more chunks after adding section"
        );
        // Original chunks should keep their IDs
        for (id, idx) in &ids_before {
            assert!(
                ids_after.iter().any(|(aid, aidx)| aid == id && aidx == idx),
                "original chunk at index {} should be preserved",
                idx
            );
        }
    }

    #[test]
    fn test_incremental_delete_removed_chunk() {
        let (conn, dir) = setup();
        let content = "# Doc\n\n## Section A\n\nContent A here with text.\n\n## Section B\n\nContent B here with text.\n\n## Section C\n\nContent C here with text.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id = get_doc_id(&conn);
        let count_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE document_id = ?",
                [doc_id],
                |r| r.get(0),
            )
            .unwrap();

        // Remove section C
        let content2 = "# Doc\n\n## Section A\n\nContent A here with text.\n\n## Section B\n\nContent B here with text.\n";
        std::fs::write(&path, content2).unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        let count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE document_id = ?",
                [doc_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            count_after < count_before,
            "should have fewer chunks after removal"
        );

        // FTS count should match chunk count
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, count_after, "FTS count should match chunk count");
    }

    #[test]
    fn test_session_incremental_append() {
        let (conn, dir) = setup();
        let jsonl1 = r#"{"message":{"role":"user","content":"First question text here for testing."},"timestamp":"2026-01-01T00:00:00Z"}
{"message":{"role":"assistant","content":"First answer text here for testing."},"timestamp":"2026-01-01T00:01:00Z"}"#;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, jsonl1).unwrap();
        index_session(&conn, &path).unwrap();

        let doc_id = get_doc_id(&conn);
        let ids_before = get_chunk_ids(&conn, doc_id);
        let count_before = ids_before.len();
        assert!(count_before >= 1);

        // Append a second Q&A pair
        let jsonl2 = format!(
            "{}\n{}\n{}",
            r#"{"message":{"role":"user","content":"First question text here for testing."},"timestamp":"2026-01-01T00:00:00Z"}"#,
            r#"{"message":{"role":"assistant","content":"First answer text here for testing."},"timestamp":"2026-01-01T00:01:00Z"}"#,
            r#"{"message":{"role":"user","content":"Second question text here for testing."},"timestamp":"2026-01-01T00:02:00Z"}"#,
        );
        std::fs::write(&path, jsonl2).unwrap();
        index_session(&conn, &path).unwrap();

        let ids_after = get_chunk_ids(&conn, doc_id);
        assert!(
            ids_after.len() > count_before,
            "should have more chunks after append"
        );
        // Original chunk(s) should keep their IDs
        for (id, idx) in &ids_before {
            let found = ids_after.iter().any(|(aid, aidx)| aid == id && aidx == idx);
            assert!(found, "original chunk at index {} should be preserved", idx);
        }
    }

    #[test]
    fn test_null_content_hash_treated_as_changed() {
        let (conn, dir) = setup();
        let content = "# Doc\n\n## Section A\n\nContent A here with text.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id = get_doc_id(&conn);

        // Simulate a pre-migration chunk by clearing content_hash to NULL
        conn.execute(
            "UPDATE chunks SET content_hash = NULL WHERE document_id = ?",
            [doc_id],
        )
        .unwrap();
        // Force file_hash change to trigger re-index
        conn.execute(
            "UPDATE documents SET file_hash = 'stale' WHERE id = ?",
            [doc_id],
        )
        .unwrap();

        // Re-index same content — NULL hash should be treated as changed
        index_file(&conn, &path, dir.path()).unwrap();

        // Verify content_hash is now populated
        let hash: Option<String> = conn
            .query_row(
                "SELECT content_hash FROM chunks WHERE document_id = ? LIMIT 1",
                [doc_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            hash.is_some(),
            "content_hash should be populated after re-index"
        );
        assert_eq!(
            hash.unwrap().len(),
            64,
            "content_hash should be 64-char hex"
        );
    }

    #[test]
    fn test_no_mutation_preserves_entities_and_links() {
        let (conn, dir) = setup();
        let content = "---\ntags: [rust, test]\n---\n\n# Doc\n\nSome content about Rust testing.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id = get_doc_id(&conn);

        // Check entity data exists after first index
        let entity_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunk_entities ce JOIN chunks c ON ce.chunk_id = c.id WHERE c.document_id = ?",
                [doc_id],
                |r| r.get(0),
            )
            .unwrap_or(0);

        // Force file_hash to differ (but keep content identical) to trigger re-index
        conn.execute(
            "UPDATE documents SET file_hash = 'stale' WHERE id = ?",
            [doc_id],
        )
        .unwrap();

        // Re-index — had_mutations should be false, entities should be preserved
        index_file(&conn, &path, dir.path()).unwrap();

        let entity_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunk_entities ce JOIN chunks c ON ce.chunk_id = c.id WHERE c.document_id = ?",
                [doc_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            entity_count, entity_count_after,
            "entity data should be preserved when chunks unchanged"
        );
    }

    #[test]
    fn test_doc_id_preserved_on_reindex() {
        let (conn, dir) = setup();
        let content = "# Doc\n\n## Section A\n\nContent A here with text.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", content);
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id_before = get_doc_id(&conn);

        // Modify and re-index
        std::fs::write(&path, "# Doc\n\n## Section A\n\nUpdated content here.\n").unwrap();
        index_file(&conn, &path, dir.path()).unwrap();

        let doc_id_after = get_doc_id(&conn);
        assert_eq!(
            doc_id_before, doc_id_after,
            "doc_id should be preserved across re-indexes"
        );
    }

    #[test]
    fn test_rebuild_fts_repopulates() {
        let (conn, dir) = setup();
        let md = "# Test\n\nSome content here.\n";
        let path = write_md(dir.path(), "daily/notes/test.md", md);
        index_file(&conn, &path, dir.path()).unwrap();

        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunk_count > 0);

        // Clear FTS manually
        conn.execute("DELETE FROM chunks_fts", []).unwrap();
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 0);

        // Rebuild
        let inserted = rebuild_fts(&conn, None).unwrap();
        assert_eq!(inserted as i64, chunk_count);

        let fts_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_after, chunk_count);

        // Verify rowids match chunk ids
        let mismatches: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks c LEFT JOIN chunks_fts f ON c.id = f.rowid WHERE f.rowid IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mismatches, 0);
    }

    #[test]
    fn test_rebuild_fts_preserves_vectors() {
        let (conn, dir) = setup();
        let md = "# Test\n\nContent for vector test.\n";
        let path = write_md(dir.path(), "daily/notes/vec.md", md);
        index_file(&conn, &path, dir.path()).unwrap();

        let chunk_id: i64 = conn
            .query_row("SELECT id FROM chunks LIMIT 1", [], |r| r.get(0))
            .unwrap();

        // Insert a fake vector
        let dim: usize = crate::config::EMBEDDING_DIM;
        let fake_vec = vec![0.1f32; dim];
        let vec_bytes: Vec<u8> = fake_vec.iter().flat_map(|f| f.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO chunks_vec(rowid, embedding) VALUES (?, ?)",
            rusqlite::params![chunk_id, vec_bytes],
        )
        .unwrap();

        let vec_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        let doc_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();

        rebuild_fts(&conn, None).unwrap();

        let vec_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        let doc_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();

        assert_eq!(vec_before, vec_after, "vectors should be preserved");
        assert_eq!(doc_before, doc_after, "documents should be preserved");
    }

    #[test]
    fn test_rebuild_fts_empty_db() {
        let (conn, _dir) = setup();
        let inserted = rebuild_fts(&conn, None).unwrap();
        assert_eq!(inserted, 0);

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 0);
    }

    #[test]
    fn test_rebuild_fts_progress_callback() {
        let (conn, dir) = setup();
        let md1 = "# One\n\nFirst doc content.\n";
        let md2 = "# Two\n\nSecond doc content.\n";
        write_md(dir.path(), "daily/notes/a.md", md1);
        write_md(dir.path(), "daily/notes/b.md", md2);
        let paths: Vec<_> = vec![
            dir.path().join("daily/notes/a.md"),
            dir.path().join("daily/notes/b.md"),
        ];
        index_all_with_progress(&conn, &paths, dir.path(), &AcceptAll, None).unwrap();

        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();

        let calls = std::cell::RefCell::new(Vec::new());
        let cb = |current: usize, total: usize| {
            calls.borrow_mut().push((current, total));
        };

        rebuild_fts(&conn, Some(&cb)).unwrap();

        let calls = calls.into_inner();
        assert_eq!(calls.len(), chunk_count as usize);
        // All calls should have the same total
        for (_, t) in &calls {
            assert_eq!(*t, chunk_count as usize);
        }
        // Last call should have current == total
        assert_eq!(calls.last().unwrap().0, chunk_count as usize);
    }

    // ─── rebuild_fts_next_batch tests ───────────────────────────────

    #[test]
    fn test_rebuild_fts_next_batch_empty_db() {
        let (conn, _dir) = setup();
        let (inserted, last_id, has_more) = rebuild_fts_next_batch(&conn, 0, 100, true).unwrap();
        assert_eq!(inserted, 0);
        assert_eq!(last_id, 0);
        assert!(!has_more);
    }

    #[test]
    fn test_rebuild_fts_next_batch_single_batch() {
        let (conn, dir) = setup();

        // Index 2 files
        let md1 = "---\nstatus: current\n---\n\n# Title\n\nContent one.\n";
        let md2 = "---\nstatus: current\n---\n\n# Title\n\nContent two.\n";
        let p1 = write_md(dir.path(), "notes/a.md", md1);
        let p2 = write_md(dir.path(), "notes/b.md", md2);
        index_file(&conn, &p1, dir.path()).unwrap();
        index_file(&conn, &p2, dir.path()).unwrap();

        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(chunks > 0);

        // Clear FTS to simulate needing a rebuild
        conn.execute("DELETE FROM chunks_fts", []).unwrap();

        // Single batch covers all
        let (inserted, _last_id, has_more) = rebuild_fts_next_batch(&conn, 0, 1000, true).unwrap();
        assert_eq!(inserted as i64, chunks);
        assert!(!has_more);

        // Verify FTS is populated
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, chunks);
    }

    #[test]
    fn test_rebuild_fts_next_batch_pagination() {
        let (conn, dir) = setup();

        // Index 3 files to ensure we have multiple chunks
        for i in 0..3 {
            let md = format!("---\nstatus: current\n---\n\n# Title {i}\n\nContent number {i}.\n");
            let p = write_md(dir.path(), &format!("notes/{i}.md"), &md);
            index_file(&conn, &p, dir.path()).unwrap();
        }

        let total_chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert!(total_chunks >= 3);

        // First batch: batch_size = 1 (delete + insert 1)
        let (inserted, last_id, has_more) = rebuild_fts_next_batch(&conn, 0, 1, true).unwrap();
        assert_eq!(inserted, 1);
        assert!(has_more);

        // Subsequent batches: no delete, just insert
        let mut total_inserted = inserted;
        let mut cursor = last_id;
        loop {
            let (inserted, new_last_id, more) =
                rebuild_fts_next_batch(&conn, cursor, 1, false).unwrap();
            total_inserted += inserted;
            cursor = new_last_id;
            if !more {
                break;
            }
        }
        assert_eq!(total_inserted as i64, total_chunks);

        // Verify FTS row count
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, total_chunks);
    }

    #[test]
    fn test_rebuild_fts_next_batch_first_batch_clears_fts() {
        let (conn, dir) = setup();

        let md = "---\nstatus: current\n---\n\n# Title\n\nContent.\n";
        let p = write_md(dir.path(), "notes/a.md", md);
        index_file(&conn, &p, dir.path()).unwrap();

        // FTS should have rows from indexing
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert!(before > 0);

        // First batch clears and re-inserts
        let (inserted, _, _) = rebuild_fts_next_batch(&conn, 0, 1000, true).unwrap();
        assert_eq!(inserted as i64, before);

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, before);
    }

    // ─── metadata JSON integration test ──────────────────────

    #[test]
    #[serial_test::serial]
    fn test_index_file_stores_metadata_json() {
        crate::lua_hooks::reset_hooks_cache();
        let conn = db::get_memory_connection().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("doc.md");
        std::fs::write(
            &f,
            "---\nstatus: outdated\nupdated: 2026-02-02\n---\n# T\n\nbody\n",
        )
        .unwrap();
        index_file(&conn, &f, tmp.path()).unwrap();
        let meta: Option<String> = conn
            .query_row(
                "SELECT metadata FROM documents WHERE source_type IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&meta.unwrap()).unwrap();
        assert_eq!(v["status"], serde_json::json!("outdated"));
        assert_eq!(v["effective_date"], serde_json::json!("2026-02-02"));
    }
}
