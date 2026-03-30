use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::chunker::chunk_markdown_default;
use crate::config;
use crate::db;
use crate::doc_links;
use crate::embedder;
use crate::entity;
use crate::frontmatter;
use crate::session_chunker::parse_session_jsonl;
use crate::tokenizer::wakachi;
use crate::user_dict;

#[derive(Debug, Default)]
pub struct IndexStats {
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
}

#[derive(Debug, Default)]
pub struct BackfillStats {
    pub filled: usize,
    pub errors: usize,
    pub panics: usize,
}

fn file_hash(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let hash = Sha256::digest(&data);
    Ok(format!("{hash:x}"))
}

fn directory_from_rel_path(rel_path: &str) -> String {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() >= 3 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        parts[0].to_string()
    }
}

fn delete_old_entries(conn: &Connection, doc_id: i64) -> anyhow::Result<()> {
    let chunk_ids: Vec<i64> = conn
        .prepare("SELECT id FROM chunks WHERE document_id = ?")?
        .query_map([doc_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    if !chunk_ids.is_empty() {
        let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let params: Vec<Box<dyn rusqlite::types::ToSql>> = chunk_ids
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        conn.execute(
            &format!("DELETE FROM chunks_fts WHERE rowid IN ({placeholders})"),
            param_refs.as_slice(),
        )?;

        // chunks_vec may not exist
        let _ = conn.execute(
            &format!("DELETE FROM chunks_vec WHERE rowid IN ({placeholders})"),
            param_refs.as_slice(),
        );

        // chunk_entities — may not exist
        let _ = conn.execute(
            &format!("DELETE FROM chunk_entities WHERE chunk_id IN ({placeholders})"),
            param_refs.as_slice(),
        );
    }

    // document_links
    doc_links::delete_links(conn, doc_id);

    // entity_edges reference doc_id directly
    conn.execute("DELETE FROM entity_edges WHERE doc_id = ?", [doc_id])
        .or_else(|e| {
            if e.to_string().contains("no such table") {
                Ok(0)
            } else {
                Err(e)
            }
        })?;

    conn.execute("DELETE FROM documents WHERE id = ?", [doc_id])?;
    Ok(())
}

/// Index a single file. Returns true if the file was (re-)indexed, false if skipped.
pub fn index_file(
    conn: &Connection,
    file_path: &Path,
    project_root: &Path,
) -> anyhow::Result<bool> {
    let rel_path = file_path
        .strip_prefix(project_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let directory = directory_from_rel_path(&rel_path);
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
            [&rel_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    if let Some((_, ref old_hash)) = existing {
        if *old_hash == current_hash {
            return Ok(false); // unchanged
        }
    }

    // Delete old entries if they exist
    if let Some((doc_id, _)) = existing {
        delete_old_entries(conn, doc_id)?;
    }

    // Parse file
    let text = std::fs::read_to_string(file_path)?;
    let (fm, body) = frontmatter::parse(&text);

    let now = chrono::Utc::now().to_rfc3339();
    let source_type = config::source_type_from_dir(&directory);
    let tags_str = if fm.tags.is_empty() {
        None
    } else {
        Some(format!("{:?}", fm.tags))
    };

    conn.execute(
        "INSERT INTO documents (file_path, source_type, title, status, created, updated, tags, file_hash, indexed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            rel_path,
            source_type,
            filename,
            fm.status,
            fm.created,
            fm.updated,
            tags_str,
            current_hash,
            now,
        ],
    )?;
    let doc_id = conn.last_insert_rowid();

    // Chunk and insert
    let chunks = chunk_markdown_default(body, &directory, &filename);
    let mut chunk_entries: Vec<(i64, String)> = Vec::new();
    for chunk in &chunks {
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content)
             VALUES (?, ?, ?, ?)",
            rusqlite::params![
                doc_id,
                chunk.chunk_index as i64,
                chunk.section_path,
                chunk.content
            ],
        )?;
        let chunk_id = conn.last_insert_rowid();
        let wakachi_text = wakachi(&chunk.content);
        conn.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
            rusqlite::params![chunk_id, wakachi_text],
        )?;
        chunk_entries.push((chunk_id, chunk.content.clone()));
    }

    // Vector embedding (if embedder is running and vec table exists)
    insert_vectors(conn, &chunk_entries);

    // Entity extraction (if entity tables exist)
    if let Err(e) = entity::insert_entities(conn, doc_id, &chunk_entries, &fm.tags) {
        eprintln!("entity extraction warning: {e}");
    }

    // Document links (tags, explicit links, entity co-occurrence)
    doc_links::build_links(conn, doc_id, &text, &fm.tags);

    // Collect dictionary candidates from chunk text
    for (_, content) in &chunk_entries {
        user_dict::collect_from_text(conn, content, "document");
    }

    Ok(true)
}

/// Index all given files. Returns stats.
pub fn index_all(
    conn: &Connection,
    file_paths: &[PathBuf],
    project_root: &Path,
) -> anyhow::Result<IndexStats> {
    let mut stats = IndexStats::default();

    for fp in file_paths {
        if !fp.exists() {
            let rel_path = fp
                .strip_prefix(project_root)
                .unwrap_or(fp)
                .to_string_lossy()
                .to_string();
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM documents WHERE file_path = ?",
                    [&rel_path],
                    |row| row.get(0),
                )
                .ok();
            if let Some(doc_id) = existing {
                delete_old_entries(conn, doc_id)?;
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

    if let Some((doc_id, _)) = existing {
        delete_old_entries(conn, doc_id)?;
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
        .last()
        .unwrap_or(&now);
    conn.execute(
        "INSERT INTO documents (file_path, source_type, title, status, created, updated, tags, file_hash, indexed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            file_key,
            "session",
            jsonl_path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
            "current",
            created,
            updated,
            Option::<String>::None,
            current_hash,
            &now,
        ],
    )?;
    let doc_id = conn.last_insert_rowid();

    let mut chunk_entries: Vec<(i64, String)> = Vec::new();
    for chunk in &chunks {
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content)
             VALUES (?, ?, ?, ?)",
            rusqlite::params![doc_id, chunk.chunk_index as i64, "session", chunk.content],
        )?;
        let chunk_id = conn.last_insert_rowid();
        let wakachi_text = wakachi(&chunk.content);
        conn.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
            rusqlite::params![chunk_id, wakachi_text],
        )?;
        chunk_entries.push((chunk_id, chunk.content.clone()));
    }

    insert_vectors(conn, &chunk_entries);

    // Learn synonyms from human messages in the session
    learn_from_session_jsonl(conn, jsonl_path);

    Ok(true)
}

/// Extract human messages from session JSONL and learn synonym pairs.
fn learn_from_session_jsonl(conn: &Connection, jsonl_path: &Path) {
    use std::io::BufRead;

    let file = match std::fs::File::open(jsonl_path) {
        Ok(f) => f,
        Err(_) => return,
    };

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
            crate::synonyms::learn_from_message(conn, &content, "chat");
            user_dict::collect_from_text(conn, &content, "session");
        }
    }
}

/// Insert vectors for chunks if embedder is running and vec table exists.
/// Write a single embedding row to chunks_vec. Returns true on success.
fn write_vec_row(conn: &Connection, chunk_id: i64, emb: &[f32]) -> bool {
    let json = match serde_json::to_string(emb) {
        Ok(j) => j,
        Err(_) => return false,
    };
    conn.execute(
        "INSERT OR IGNORE INTO chunks_vec(rowid, embedding) VALUES (?, ?)",
        rusqlite::params![chunk_id, json],
    )
    .is_ok()
}

fn insert_vectors(conn: &Connection, chunk_entries: &[(i64, String)]) {
    if chunk_entries.is_empty() {
        return;
    }
    if !db::has_vec_table(conn) {
        return;
    }

    let texts: Vec<String> = chunk_entries.iter().map(|(_, text)| text.clone()).collect();
    let embeddings = match embedder::embed_via_socket(&texts) {
        Some(e) => e,
        None => return,
    };

    for ((chunk_id, _), emb) in chunk_entries.iter().zip(embeddings.iter()) {
        write_vec_row(conn, *chunk_id, emb);
    }
}

pub use crate::config::BACKFILL_BATCH_SIZE;

/// Encode function type: takes texts, returns embedding vectors.
pub type EncodeFn<'a> = &'a dyn Fn(&[String]) -> anyhow::Result<Vec<Vec<f32>>>;

/// Extract a human-readable message from a panic payload.
fn panic_message(info: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = info.downcast_ref::<String>() {
        s.clone()
    } else if let Some(&s) = info.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "unknown panic".to_string()
    }
}

/// Fill in missing vectors for chunks that have FTS5 entries but no vector entries.
/// Uses keyset pagination to avoid loading all missing chunks into memory at once.
/// Each INSERT auto-commits individually (rusqlite default autocommit mode).
/// Failed batches are logged and skipped — the next run will retry them.
pub fn backfill_vectors(
    conn: &Connection,
    encode_fn: EncodeFn,
    batch_size: usize,
    progress_cb: Option<&dyn Fn(i64, usize, usize)>,
) -> anyhow::Result<BackfillStats> {
    if !db::has_vec_table(conn) {
        return Ok(BackfillStats::default());
    }

    // Count total missing for progress reporting
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks c
             LEFT JOIN chunks_vec v ON c.id = v.rowid
             WHERE v.rowid IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if total == 0 {
        return Ok(BackfillStats::default());
    }

    eprintln!("Backfilling {total} chunks...");
    if let Some(cb) = &progress_cb {
        cb(total, 0, 0);
    }
    let mut stats = BackfillStats::default();
    let mut last_id: i64 = 0;

    loop {
        let batch: Vec<(i64, String, String)> = conn
            .prepare(
                "SELECT c.id, c.content, d.file_path
                 FROM chunks c
                 LEFT JOIN chunks_vec v ON c.id = v.rowid
                 JOIN documents d ON c.document_id = d.id
                 WHERE v.rowid IS NULL AND c.id > ?
                 ORDER BY c.id
                 LIMIT ?",
            )?
            .query_map(rusqlite::params![last_id, batch_size as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if batch.is_empty() {
            break;
        }
        last_id = batch.last().unwrap().0;

        let files: Vec<&str> = batch.iter().map(|(_, _, f)| f.as_str()).collect();
        let batch_start_id = batch.first().unwrap().0;
        let batch_end_id = last_id;
        eprintln!("  batch {batch_start_id}..{batch_end_id}: {:?}", files);

        let texts: Vec<String> = batch
            .iter()
            .map(|(_, content, _)| content.clone())
            .collect();

        match catch_unwind(AssertUnwindSafe(|| encode_fn(&texts))) {
            Ok(Ok(embeddings)) => {
                let tx = conn.unchecked_transaction()?;
                for ((chunk_id, _, _), emb) in batch.iter().zip(embeddings.iter()) {
                    if write_vec_row(conn, *chunk_id, emb) {
                        stats.filled += 1;
                    } else {
                        eprintln!("Insert error for chunk {chunk_id} (skipping)");
                        stats.errors += 1;
                    }
                }
                tx.commit()?;
            }
            Ok(Err(e)) => {
                eprintln!("Batch error (chunks {batch_start_id}..{batch_end_id}): {e} — skipping");
                stats.errors += batch.len();
            }
            Err(panic_info) => {
                let msg = panic_message(&panic_info);
                eprintln!(
                    "PANIC in encode (chunks {batch_start_id}..{batch_end_id}): {msg} — skipping"
                );
                stats.panics += 1;
                stats.errors += batch.len();
            }
        }

        let processed = stats.filled + stats.errors;
        eprintln!("  {processed}/{total}");

        if let Some(cb) = &progress_cb {
            cb(total, stats.filled, stats.errors);
        }
    }

    if stats.panics > 0 {
        eprintln!(
            "Backfill complete: {} filled, {} errors, {} panics.",
            stats.filled, stats.errors, stats.panics
        );
    } else {
        eprintln!(
            "Backfill complete: {} filled, {} errors.",
            stats.filled, stats.errors
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::io::Write;
    use tempfile::TempDir;

    fn setup() -> (Connection, TempDir) {
        let conn = db::get_memory_connection().unwrap();
        let dir = TempDir::new().unwrap();
        (conn, dir)
    }

    fn write_md(dir: &Path, rel_path: &str, content: &str) -> PathBuf {
        let full = dir.join(rel_path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        full
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

    #[test]
    fn test_deleted_file() {
        let (conn, dir) = setup();
        let path = write_md(dir.path(), "daily/notes/test.md", "# Hello\n\nWorld.\n");
        index_file(&conn, &path, dir.path()).unwrap();

        // Delete the file
        std::fs::remove_file(&path).unwrap();
        let stats = index_all(&conn, &[path], dir.path()).unwrap();
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
        // Index file (no embedder running, so no vectors)
        index_file(&conn, &path, dir.path()).unwrap();

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
}
