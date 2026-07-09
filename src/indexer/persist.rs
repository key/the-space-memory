//! Persist stage: all DB writes for one file in a single transaction
//! (documents, chunks, FTS5, entity graph, doc_links, dict candidates).

use rusqlite::Connection;

use super::prepare::{ChunkInput, PreparedFile};
use crate::tokenizer::wakachi;
use crate::{doc_links, entity, user_dict};

pub(crate) struct DiffResult {
    /// All chunk entries (new + existing) as (chunk_id, content) — for entity rebuild.
    pub(crate) all_chunk_entries: Vec<(i64, String)>,
    /// Chunks needing vector embedding (new + changed).
    pub(crate) chunks_needing_vectors: Vec<(i64, String)>,
    /// Whether any mutation occurred.
    had_mutations: bool,
}

/// Delete FTS, vector, skip, and entity entries for specific chunk IDs.
pub(crate) fn delete_chunk_side_tables(conn: &Connection, chunk_ids: &[i64]) -> anyhow::Result<()> {
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

pub(crate) fn delete_old_entries(conn: &Connection, doc_id: i64) -> anyhow::Result<()> {
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

/// Compare freshly parsed chunks against stored chunks for a document.
/// Inserts new chunks, updates changed chunks, deletes removed chunks, skips unchanged.
///
/// MUST be called within a transaction — the caller is responsible for wrapping
/// this in `unchecked_transaction()` to ensure atomicity of the multi-statement diff.
pub(crate) fn diff_chunks(
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

/// Persist one file into the DB inside a single transaction.
///
/// Writes to `documents`, `chunks`, `chunks_fts`, `entity_edges`,
/// `document_links`, and `dictionary_candidates`. Vector embedding
/// is **not** done here — the caller invokes `embed::insert_vectors`
/// after this returns.
pub(crate) fn persist(
    conn: &Connection,
    rel_path: &str,
    current_hash: &str,
    existing_doc_id: Option<i64>,
    prepared: &PreparedFile,
) -> anyhow::Result<DiffResult> {
    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;

    let doc_id = match existing_doc_id {
        Some(doc_id) => {
            // Update existing document row (preserves doc_id for entity_edges/doc_links)
            tx.execute(
                "UPDATE documents SET source_type=?, title=?, status=?, created=?, updated=?, tags=?, file_hash=?, indexed_at=?, metadata=?
                 WHERE id=?",
                rusqlite::params![
                    prepared.source_type, prepared.title, prepared.frontmatter.status,
                    prepared.frontmatter.created, prepared.frontmatter.updated,
                    prepared.tags_str, current_hash, now, prepared.metadata_json, doc_id,
                ],
            )?;
            doc_id
        }
        None => {
            tx.execute(
                "INSERT INTO documents (file_path, source_type, title, status, created, updated, tags, file_hash, indexed_at, metadata)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    rel_path, prepared.source_type, prepared.title, prepared.frontmatter.status,
                    prepared.frontmatter.created, prepared.frontmatter.updated,
                    prepared.tags_str, current_hash, now, prepared.metadata_json,
                ],
            )?;
            tx.last_insert_rowid()
        }
    };

    let diff = diff_chunks(&tx, doc_id, &prepared.chunk_inputs)?;

    // Side indexes are per-source policy (see SourcePolicy): e.g. sessions
    // are searchable text only and skip all three.
    if diff.had_mutations {
        if prepared.policy.entity_graph {
            // Rebuild entity graph (document-level)
            tx.execute("DELETE FROM entity_edges WHERE doc_id = ?", [doc_id])
                .or_else(|e| {
                    if e.to_string().contains("no such table") {
                        Ok(0)
                    } else {
                        Err(e)
                    }
                })?;
            if let Err(e) = entity::insert_entities(
                &tx,
                doc_id,
                &diff.all_chunk_entries,
                &prepared.frontmatter.tags,
            ) {
                log::warn!("entity extraction warning: {e}");
            }
        }

        if prepared.policy.doc_links {
            // Rebuild document links
            doc_links::delete_links(&tx, doc_id);
            doc_links::build_links(&tx, doc_id, &prepared.text, &prepared.frontmatter.tags);
        }

        if prepared.policy.dict_candidates {
            // Collect dictionary candidates
            for (_, content) in &diff.all_chunk_entries {
                user_dict::collect_from_text(&tx, content, "document");
            }
        }
    }

    tx.commit()?;
    Ok(diff)
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

/// Re-tokenize one batch of chunks into the FTS5 index.
///
/// Uses keyset pagination (`WHERE id > last_id ORDER BY id LIMIT batch_size`).
/// When `is_first_batch` is true, clears the entire FTS table before inserting.
///
/// Returns `(inserted_count, new_last_id, has_more)`.
/// `has_more` is `true` when `batch.len() == batch_size`; callers should
/// handle the case where the subsequent call returns `inserted_count == 0`.
pub fn rebuild_fts_next_batch(
    conn: &Connection,
    last_id: i64,
    batch_size: usize,
    is_first_batch: bool,
) -> anyhow::Result<(usize, i64, bool)> {
    let batch: Vec<(i64, String)> = conn
        .prepare("SELECT id, content FROM chunks WHERE id > ? ORDER BY id LIMIT ?")?
        .query_map(rusqlite::params![last_id, batch_size as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if batch.is_empty() {
        return Ok((0, last_id, false));
    }

    let new_last_id = batch.last().unwrap().0;

    let tx = conn.unchecked_transaction()?;
    if is_first_batch {
        tx.execute("DELETE FROM chunks_fts", [])?;
    }
    for (id, content) in &batch {
        let wakachi_text = wakachi(content);
        tx.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
            rusqlite::params![id, wakachi_text],
        )?;
    }
    tx.commit()?;

    let has_more = batch.len() == batch_size;
    Ok((batch.len(), new_last_id, has_more))
}
