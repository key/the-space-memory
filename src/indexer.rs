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
    /// Keyset pagination cursor (last processed chunk ID).
    pub last_id: i64,
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

/// Delete FTS, vector, skip, and entity entries for specific chunk IDs.
fn delete_chunk_side_tables(conn: &Connection, chunk_ids: &[i64]) -> anyhow::Result<()> {
    if chunk_ids.is_empty() {
        return Ok(());
    }
    let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = chunk_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    conn.execute(
        &format!("DELETE FROM chunks_fts WHERE rowid IN ({placeholders})"),
        param_refs.as_slice(),
    )?;

    // chunks_vec may not exist in older DBs
    conn.execute(
        &format!("DELETE FROM chunks_vec WHERE rowid IN ({placeholders})"),
        param_refs.as_slice(),
    )
    .or_else(|e| {
        if e.to_string().contains("no such table") {
            Ok(0)
        } else {
            Err(e)
        }
    })?;

    // chunks_vec_skip — may not exist in older DBs
    conn.execute(
        &format!("DELETE FROM chunks_vec_skip WHERE chunk_id IN ({placeholders})"),
        param_refs.as_slice(),
    )
    .or_else(|e| {
        if e.to_string().contains("no such table") {
            Ok(0)
        } else {
            Err(e)
        }
    })?;

    // chunk_entities — may not exist in older DBs
    conn.execute(
        &format!("DELETE FROM chunk_entities WHERE chunk_id IN ({placeholders})"),
        param_refs.as_slice(),
    )
    .or_else(|e| {
        if e.to_string().contains("no such table") {
            Ok(0)
        } else {
            Err(e)
        }
    })?;

    Ok(())
}

fn delete_old_entries(conn: &Connection, doc_id: i64) -> anyhow::Result<()> {
    let chunk_ids: Vec<i64> = conn
        .prepare("SELECT id FROM chunks WHERE document_id = ?")?
        .query_map([doc_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    delete_chunk_side_tables(conn, &chunk_ids)?;

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

struct ChunkInput {
    chunk_index: usize,
    section_path: String,
    content: String,
    content_hash: String,
}

struct DiffResult {
    /// All chunk entries (new + existing) as (chunk_id, content) — for entity rebuild.
    all_chunk_entries: Vec<(i64, String)>,
    /// Chunks needing vector embedding (new + changed).
    chunks_needing_vectors: Vec<(i64, String)>,
    /// Whether any mutation occurred.
    had_mutations: bool,
}

/// Compare freshly parsed chunks against stored chunks for a document.
/// Inserts new chunks, updates changed chunks, deletes removed chunks, skips unchanged.
///
/// MUST be called within a transaction — the caller is responsible for wrapping
/// this in `unchecked_transaction()` to ensure atomicity of the multi-statement diff.
fn diff_chunks(
    conn: &Connection,
    doc_id: i64,
    new_chunks: &[ChunkInput],
) -> anyhow::Result<DiffResult> {
    use std::collections::HashMap;

    // Load existing chunks: chunk_index → (id, content_hash)
    let mut existing: HashMap<usize, (i64, Option<String>)> = HashMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT id, chunk_index, content_hash FROM chunks WHERE document_id = ?")?;
        let rows = stmt.query_map([doc_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, idx, hash) = row?;
            existing.insert(idx, (id, hash));
        }
    }

    let mut all_chunk_entries: Vec<(i64, String)> = Vec::new();
    let mut chunks_needing_vectors: Vec<(i64, String)> = Vec::new();
    let mut had_mutations = false;

    for chunk in new_chunks {
        if let Some((existing_id, ref stored_hash)) = existing.remove(&chunk.chunk_index) {
            // Chunk exists at this index
            if stored_hash.as_deref() == Some(&chunk.content_hash) {
                // Unchanged — skip
                all_chunk_entries.push((existing_id, chunk.content.clone()));
            } else {
                // Content changed — update
                had_mutations = true;
                conn.execute(
                    "UPDATE chunks SET content = ?, content_hash = ?, section_path = ? WHERE id = ?",
                    rusqlite::params![chunk.content, chunk.content_hash, chunk.section_path, existing_id],
                )?;
                // FTS5 does not support UPDATE — delete + insert
                delete_chunk_side_tables(conn, &[existing_id])?;
                let wakachi_text = wakachi(&chunk.content);
                conn.execute(
                    "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
                    rusqlite::params![existing_id, wakachi_text],
                )?;
                all_chunk_entries.push((existing_id, chunk.content.clone()));
                chunks_needing_vectors.push((existing_id, chunk.content.clone()));
            }
        } else {
            // New chunk — insert
            had_mutations = true;
            conn.execute(
                "INSERT INTO chunks (document_id, chunk_index, section_path, content, content_hash)
                 VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    doc_id,
                    chunk.chunk_index as i64,
                    chunk.section_path,
                    chunk.content,
                    chunk.content_hash,
                ],
            )?;
            let chunk_id = conn.last_insert_rowid();
            let wakachi_text = wakachi(&chunk.content);
            conn.execute(
                "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
                rusqlite::params![chunk_id, wakachi_text],
            )?;
            all_chunk_entries.push((chunk_id, chunk.content.clone()));
            chunks_needing_vectors.push((chunk_id, chunk.content.clone()));
        }
    }

    // Delete chunks that no longer exist
    if !existing.is_empty() {
        had_mutations = true;
        let removed_ids: Vec<i64> = existing.values().map(|(id, _)| *id).collect();
        delete_chunk_side_tables(conn, &removed_ids)?;
        for id in &removed_ids {
            conn.execute("DELETE FROM chunks WHERE id = ?", [id])?;
        }
    }

    Ok(DiffResult {
        all_chunk_entries,
        chunks_needing_vectors,
        had_mutations,
    })
}

/// Index a single file. Returns true if the file was (re-)indexed, false if skipped.
pub fn index_file(conn: &Connection, file_path: &Path, index_root: &Path) -> anyhow::Result<bool> {
    let rel_path = file_path
        .strip_prefix(index_root)
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

    // Build chunk inputs with content hashes
    let chunks = chunk_markdown_default(body, &directory, &filename);
    let chunk_inputs: Vec<ChunkInput> = chunks
        .iter()
        .map(|c| ChunkInput {
            chunk_index: c.chunk_index,
            section_path: c.section_path.clone(),
            content: c.content.clone(),
            content_hash: chunk_hash(&c.content),
        })
        .collect();

    let tx = conn.unchecked_transaction()?;

    let doc_id = if let Some((doc_id, _)) = existing {
        // Update existing document row (preserves doc_id for entity_edges/doc_links)
        tx.execute(
            "UPDATE documents SET source_type=?, title=?, status=?, created=?, updated=?, tags=?, file_hash=?, indexed_at=?
             WHERE id=?",
            rusqlite::params![
                source_type, filename, fm.status, fm.created, fm.updated,
                tags_str, current_hash, now, doc_id,
            ],
        )?;
        doc_id
    } else {
        tx.execute(
            "INSERT INTO documents (file_path, source_type, title, status, created, updated, tags, file_hash, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                rel_path, source_type, filename, fm.status, fm.created, fm.updated,
                tags_str, current_hash, now,
            ],
        )?;
        tx.last_insert_rowid()
    };

    let diff = diff_chunks(&tx, doc_id, &chunk_inputs)?;

    if diff.had_mutations {
        // Rebuild entity graph (document-level)
        tx.execute("DELETE FROM entity_edges WHERE doc_id = ?", [doc_id])
            .or_else(|e| {
                if e.to_string().contains("no such table") {
                    Ok(0)
                } else {
                    Err(e)
                }
            })?;
        if let Err(e) = entity::insert_entities(&tx, doc_id, &diff.all_chunk_entries, &fm.tags) {
            log::warn!("entity extraction warning: {e}");
        }

        // Rebuild document links
        doc_links::delete_links(&tx, doc_id);
        doc_links::build_links(&tx, doc_id, &text, &fm.tags);

        // Collect dictionary candidates
        for (_, content) in &diff.all_chunk_entries {
            user_dict::collect_from_text(&tx, content, "document");
        }
    }

    tx.commit()?;

    // Vector embedding outside transaction (socket I/O)
    if !diff.chunks_needing_vectors.is_empty() {
        insert_vectors(conn, &diff.chunks_needing_vectors);
    }

    Ok(true)
}

/// Index all given files. Returns stats.
pub fn index_all(
    conn: &Connection,
    file_paths: &[PathBuf],
    index_root: &Path,
) -> anyhow::Result<IndexStats> {
    index_all_with_progress(conn, file_paths, index_root, None)
}

/// Progress callback type for index_all_with_progress: (current, total, file_path).
pub type IndexProgressCb<'a> = &'a dyn Fn(usize, usize, &Path);

pub fn index_all_with_progress(
    conn: &Connection,
    file_paths: &[PathBuf],
    index_root: &Path,
    progress_cb: Option<IndexProgressCb<'_>>,
) -> anyhow::Result<IndexStats> {
    let mut stats = IndexStats::default();
    let total = file_paths.len();

    for (i, fp) in file_paths.iter().enumerate() {
        if let Some(cb) = progress_cb {
            cb(i + 1, total, fp);
        }
        if !fp.exists() {
            let rel_path = fp
                .strip_prefix(index_root)
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

        if index_file(conn, fp, index_root)? {
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

    let diff = diff_chunks(&tx, doc_id, &chunk_inputs)?;

    // Note: entity graph and doc_links are not rebuilt for sessions.
    // Sessions don't participate in entity co-occurrence or link graphs.

    tx.commit()?;

    // Vector embedding outside transaction (socket I/O)
    if !diff.chunks_needing_vectors.is_empty() {
        insert_vectors(conn, &diff.chunks_needing_vectors);
    }

    // Learn synonyms from human messages in the session (wrapped in transaction)
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
    // Skip socket I/O if embedder is not running
    if !config::embedder_socket_path().exists() {
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

/// Record a chunk in the skip table so it is not retried on subsequent backfill runs.
/// The skip record is automatically cleaned up when the parent document is re-indexed
/// (chunks are deleted and re-created with new IDs).
fn mark_chunk_skip(conn: &Connection, chunk_id: i64, reason: &str) -> bool {
    match conn.execute(
        "INSERT OR IGNORE INTO chunks_vec_skip(chunk_id, reason) VALUES (?, ?)",
        rusqlite::params![chunk_id, reason],
    ) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("failed to write skip record for chunk {chunk_id}: {e} — chunk will be retried next run");
            false
        }
    }
}

/// Retry each chunk in the batch individually, skipping persistent failures.
fn retry_individually(
    batch: &[(i64, String, String)],
    encode_fn: EncodeFn,
    conn: &Connection,
    stats: &mut BackfillStats,
) {
    for (chunk_id, content, file_path) in batch {
        let single = vec![content.clone()];
        let result = catch_unwind(AssertUnwindSafe(|| encode_fn(&single)));
        match result {
            Ok(Ok(ref embeddings)) if !embeddings.is_empty() => {
                if write_vec_row(conn, *chunk_id, &embeddings[0]) {
                    stats.filled += 1;
                } else {
                    log::warn!("chunk {chunk_id} ({file_path}): insert error — skipping");
                    mark_chunk_skip(conn, *chunk_id, "insert_error");
                    stats.errors += 1;
                }
            }
            Ok(Ok(_)) => {
                log::warn!("chunk {chunk_id} ({file_path}): empty embedding — skipping");
                mark_chunk_skip(conn, *chunk_id, "empty_embedding");
                stats.errors += 1;
            }
            Ok(Err(e)) => {
                log::warn!("chunk {chunk_id} ({file_path}): error ({e}) — skipping");
                mark_chunk_skip(conn, *chunk_id, "encode_error");
                stats.errors += 1;
            }
            Err(panic_info) => {
                let msg = panic_message(&panic_info);
                log::error!("chunk {chunk_id} ({file_path}): PANIC ({msg}) — skipping");
                mark_chunk_skip(conn, *chunk_id, "panic");
                stats.panics += 1;
                stats.errors += 1;
            }
        }
    }
}

/// Process one batch of missing vectors. Returns (stats, has_more).
/// `last_id` is the keyset pagination cursor — pass 0 for the first call,
/// then pass the returned `stats.last_id` for subsequent calls.
pub fn backfill_next_batch(
    conn: &Connection,
    encode_fn: EncodeFn,
    batch_size: usize,
    last_id: i64,
) -> anyhow::Result<(BackfillStats, bool)> {
    if !db::has_vec_table(conn) {
        return Ok((BackfillStats::default(), false));
    }

    let batch: Vec<(i64, String, String)> = conn
        .prepare(
            "SELECT c.id, c.content, d.file_path
             FROM chunks c
             LEFT JOIN chunks_vec v ON c.id = v.rowid
             LEFT JOIN chunks_vec_skip s ON c.id = s.chunk_id
             JOIN documents d ON c.document_id = d.id
             WHERE v.rowid IS NULL AND s.chunk_id IS NULL AND c.id > ?
             ORDER BY c.id
             LIMIT ?",
        )?
        .query_map(rusqlite::params![last_id, batch_size as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    if batch.is_empty() {
        return Ok((BackfillStats::default(), false));
    }

    let mut stats = BackfillStats {
        last_id: batch.last().unwrap().0,
        ..BackfillStats::default()
    };

    let texts: Vec<String> = batch
        .iter()
        .map(|(_, content, _)| content.clone())
        .collect();

    match catch_unwind(AssertUnwindSafe(|| encode_fn(&texts))) {
        Ok(Ok(embeddings)) if embeddings.len() == batch.len() => {
            let tx = conn.unchecked_transaction()?;
            for ((chunk_id, _, _), emb) in batch.iter().zip(embeddings.iter()) {
                if write_vec_row(&tx, *chunk_id, emb) {
                    stats.filled += 1;
                } else {
                    log::warn!("Insert error for chunk {chunk_id} — skipping");
                    mark_chunk_skip(conn, *chunk_id, "insert_error");
                    stats.errors += 1;
                }
            }
            tx.commit()?;
        }
        Ok(Ok(embeddings)) => {
            log::warn!(
                "Embedding count mismatch (got {}, expected {})",
                embeddings.len(),
                batch.len()
            );
            if batch.len() > 1 {
                retry_individually(&batch, encode_fn, conn, &mut stats);
            } else {
                let chunk_id = batch[0].0;
                mark_chunk_skip(conn, chunk_id, "embedding_count_mismatch");
                stats.errors += 1;
            }
        }
        Ok(Err(e)) => {
            log::warn!("Batch error: {e}");
            if batch.len() > 1 {
                retry_individually(&batch, encode_fn, conn, &mut stats);
            } else {
                mark_chunk_skip(conn, batch[0].0, "encode_error");
                stats.errors += 1;
            }
        }
        Err(panic_info) => {
            let msg = panic_message(&panic_info);
            log::error!("PANIC in encode: {msg}");
            stats.panics += 1;
            if batch.len() > 1 {
                retry_individually(&batch, encode_fn, conn, &mut stats);
            } else {
                mark_chunk_skip(conn, batch[0].0, "panic");
                stats.errors += 1;
            }
        }
    }

    Ok((stats, true))
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

    log::info!("Backfilling {total} chunks...");
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
                 LEFT JOIN chunks_vec_skip s ON c.id = s.chunk_id
                 JOIN documents d ON c.document_id = d.id
                 WHERE v.rowid IS NULL AND s.chunk_id IS NULL AND c.id > ?
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
        log::debug!("batch {batch_start_id}..{batch_end_id}: {:?}", files);

        let texts: Vec<String> = batch
            .iter()
            .map(|(_, content, _)| content.clone())
            .collect();

        match catch_unwind(AssertUnwindSafe(|| encode_fn(&texts))) {
            Ok(Ok(embeddings)) if embeddings.len() == batch.len() => {
                let tx = conn.unchecked_transaction()?;
                for ((chunk_id, _, _), emb) in batch.iter().zip(embeddings.iter()) {
                    if write_vec_row(&tx, *chunk_id, emb) {
                        stats.filled += 1;
                    } else {
                        log::warn!("Insert error for chunk {chunk_id} — skipping");
                        mark_chunk_skip(conn, *chunk_id, "insert_error");
                        stats.errors += 1;
                    }
                }
                tx.commit()?;
            }
            Ok(Ok(embeddings)) => {
                log::warn!(
                    "Embedding count mismatch (got {}, expected {}) for batch {batch_start_id}..{batch_end_id}",
                    embeddings.len(),
                    batch.len()
                );
                if batch.len() > 1 {
                    log::warn!("Retrying {} chunks individually...", batch.len());
                    retry_individually(&batch, encode_fn, conn, &mut stats);
                } else {
                    let chunk_id = batch[0].0;
                    mark_chunk_skip(conn, chunk_id, "embedding_count_mismatch");
                    stats.errors += 1;
                }
            }
            Ok(Err(e)) => {
                log::warn!("Batch error (chunks {batch_start_id}..{batch_end_id}): {e}");
                if batch.len() > 1 {
                    log::warn!("Retrying {} chunks individually...", batch.len());
                    retry_individually(&batch, encode_fn, conn, &mut stats);
                } else {
                    let chunk_id = batch[0].0;
                    log::warn!("chunk {chunk_id}: failed individually — skipping");
                    mark_chunk_skip(conn, chunk_id, "encode_error");
                    stats.errors += 1;
                }
            }
            Err(panic_info) => {
                let msg = panic_message(&panic_info);
                log::error!("PANIC in encode (chunks {batch_start_id}..{batch_end_id}): {msg}");
                stats.panics += 1;
                if batch.len() > 1 {
                    log::warn!("Retrying {} chunks individually...", batch.len());
                    retry_individually(&batch, encode_fn, conn, &mut stats);
                } else {
                    let chunk_id = batch[0].0;
                    log::warn!("chunk {chunk_id}: failed individually — skipping");
                    mark_chunk_skip(conn, chunk_id, "panic");
                    stats.errors += 1;
                }
            }
        }

        let processed = stats.filled + stats.errors;
        log::debug!("{processed}/{total}");

        if let Some(cb) = &progress_cb {
            cb(total, stats.filled, stats.errors);
        }
    }

    if stats.panics > 0 {
        log::info!(
            "Backfill complete: {} filled, {} errors, {} panics.",
            stats.filled,
            stats.errors,
            stats.panics
        );
    } else {
        log::info!(
            "Backfill complete: {} filled, {} errors.",
            stats.filled,
            stats.errors
        );
    }
    Ok(stats)
}

/// Rebuild only the FTS5 index by re-running wakachi on all chunks.
/// Vectors, documents, entities, and other data are preserved.
pub fn rebuild_fts(
    conn: &Connection,
    progress_cb: Option<&dyn Fn(usize, usize)>,
) -> anyhow::Result<usize> {
    let tx = conn.unchecked_transaction()?;

    let total: i64 = tx.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    let total = total as usize;

    let rows: Vec<(i64, String)> = {
        let mut stmt = tx.prepare("SELECT id, content FROM chunks ORDER BY id")?;
        let mapped = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        mapped
    };

    tx.execute("DELETE FROM chunks_fts", [])?;

    for (i, (id, content)) in rows.iter().enumerate() {
        let wakachi_text = wakachi(content);
        tx.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
            rusqlite::params![id, wakachi_text],
        )?;
        if let Some(cb) = &progress_cb {
            cb(i + 1, total);
        }
    }

    tx.commit()?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let (stats2, has_more2) =
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

        let stats = index_all_with_progress(&conn, &files, dir.path(), Some(&cb)).unwrap();
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
        let stats = index_all_with_progress(&conn, &files, dir.path(), None).unwrap();
        assert_eq!(stats.indexed, 1);
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
        index_all_with_progress(&conn, &paths, dir.path(), None).unwrap();

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
}
