//! Embed stage: asynchronous vector inference and vector-row writes.
//! Invoked only after Persist commits; never blocks indexing (vectors-always-async).
use std::panic::{catch_unwind, AssertUnwindSafe};

use rusqlite::Connection;

use crate::{config, db, embedder};

#[derive(Debug, Default)]
pub struct BackfillStats {
    pub filled: usize,
    pub errors: usize,
    pub panics: usize,
    /// Keyset pagination cursor (last processed chunk ID).
    pub last_id: i64,
}

/// Encode function type: takes texts, returns embedding vectors.
pub type EncodeFn<'a> = &'a dyn Fn(&[String]) -> anyhow::Result<Vec<Vec<f32>>>;

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

/// Insert vectors for chunks if embedder is running and vec table exists.
pub(crate) fn insert_vectors(conn: &Connection, chunk_entries: &[(i64, String)]) {
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

/// Fetch the next page of chunks that still need vectors, ordered by id using
/// keyset pagination (`id > last_id`). Skip-marked chunks are excluded so they
/// are never retried.
fn fetch_missing_batch(
    conn: &Connection,
    last_id: i64,
    batch_size: usize,
) -> anyhow::Result<Vec<(i64, String, String)>> {
    let batch = conn
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
    Ok(batch)
}

/// Handle a batch-level encode failure: retry each chunk individually when the
/// batch holds more than one chunk, otherwise record the lone chunk in the skip
/// table under `skip_reason` so it is not retried next run.
fn retry_or_skip(
    conn: &Connection,
    encode_fn: EncodeFn,
    batch: &[(i64, String, String)],
    stats: &mut BackfillStats,
    skip_reason: &str,
) {
    if batch.len() > 1 {
        log::warn!("Retrying {} chunks individually...", batch.len());
        retry_individually(batch, encode_fn, conn, stats);
    } else {
        let chunk_id = batch[0].0;
        log::warn!("chunk {chunk_id}: failed — marking skip ({skip_reason})");
        mark_chunk_skip(conn, chunk_id, skip_reason);
        stats.errors += 1;
    }
}

/// Encode one already-fetched batch and persist the resulting vectors, updating
/// `stats`. Successful embeddings are written in a single transaction; a
/// batch-level failure (count mismatch, encode error, or panic) falls back to
/// [`retry_or_skip`]. Shared by the full-corpus and incremental backfill paths.
fn process_batch(
    conn: &Connection,
    encode_fn: EncodeFn,
    batch: &[(i64, String, String)],
    stats: &mut BackfillStats,
) -> anyhow::Result<()> {
    debug_assert!(
        !batch.is_empty(),
        "process_batch requires a non-empty batch"
    );
    let start_id = batch.first().map_or(0, |c| c.0);
    let end_id = batch.last().map_or(0, |c| c.0);

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
                "Embedding count mismatch (got {}, expected {}) for batch {start_id}..{end_id}",
                embeddings.len(),
                batch.len()
            );
            retry_or_skip(conn, encode_fn, batch, stats, "embedding_count_mismatch");
        }
        Ok(Err(e)) => {
            log::warn!("Batch error (chunks {start_id}..{end_id}): {e}");
            retry_or_skip(conn, encode_fn, batch, stats, "encode_error");
        }
        Err(panic_info) => {
            let msg = panic_message(&panic_info);
            log::error!("PANIC in encode (chunks {start_id}..{end_id}): {msg}");
            stats.panics += 1;
            retry_or_skip(conn, encode_fn, batch, stats, "panic");
        }
    }
    Ok(())
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

    let batch = fetch_missing_batch(conn, last_id, batch_size)?;
    if batch.is_empty() {
        return Ok((BackfillStats::default(), false));
    }

    let mut stats = BackfillStats {
        last_id: batch.last().unwrap().0,
        ..BackfillStats::default()
    };
    process_batch(conn, encode_fn, &batch, &mut stats)?;

    Ok((stats, true))
}

/// Count chunks still missing a vector. Skip-marked chunks are excluded so the
/// reported total matches what the fetch query will actually attempt; otherwise
/// skipped chunks show up as permanent phantom "missing" work in doctor/status
/// (matches the daemon's periodic-backfill count).
fn count_missing(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM chunks c
         LEFT JOIN chunks_vec v ON c.id = v.rowid
         LEFT JOIN chunks_vec_skip s ON c.id = s.chunk_id
         WHERE v.rowid IS NULL AND s.chunk_id IS NULL",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// Log a one-line completion summary, including the panic count only when any
/// chunk panicked during encoding.
fn log_summary(stats: &BackfillStats) {
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
}

/// Fill in missing vectors for chunks that have FTS5 entries but no vector entries.
/// Uses keyset pagination to avoid loading all missing chunks into memory at once.
/// Each batch's successful inserts are committed in one transaction; failed
/// batches fall back to per-chunk retry or skip, and the next run retries skips.
pub fn backfill_vectors(
    conn: &Connection,
    encode_fn: EncodeFn,
    batch_size: usize,
    progress_cb: Option<&dyn Fn(i64, usize, usize)>,
) -> anyhow::Result<BackfillStats> {
    if !db::has_vec_table(conn) {
        return Ok(BackfillStats::default());
    }

    let total = count_missing(conn);
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
        let batch = fetch_missing_batch(conn, last_id, batch_size)?;
        if batch.is_empty() {
            break;
        }
        last_id = batch.last().unwrap().0;

        let files: Vec<&str> = batch.iter().map(|(_, _, f)| f.as_str()).collect();
        log::debug!("batch {}..{last_id}: {files:?}", batch.first().unwrap().0);

        process_batch(conn, encode_fn, &batch, &mut stats)?;

        log::debug!("{}/{total}", stats.filled + stats.errors);
        if let Some(cb) = &progress_cb {
            cb(total, stats.filled, stats.errors);
        }
    }

    log_summary(&stats);
    Ok(stats)
}
