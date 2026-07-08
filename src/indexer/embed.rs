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

    let encode_fn = |texts: &[String]| {
        embedder::embed_via_socket(texts).ok_or_else(|| anyhow::anyhow!("embedder not available"))
    };
    insert_vectors_batched(conn, chunk_entries, &encode_fn, config::BACKFILL_BATCH_SIZE);
}

/// Embed and store vectors in sub-batches so a document with many chunks
/// (sessions have been observed with 350+) never lands in the embedder as
/// one request — attention memory there grows with batch × seq².
/// On encode failure, stop: the embedder is likely down and the periodic
/// backfill will fill the remaining chunks.
fn insert_vectors_batched(
    conn: &Connection,
    chunk_entries: &[(i64, String)],
    encode_fn: EncodeFn,
    batch_size: usize,
) {
    for batch in chunk_entries.chunks(batch_size.max(1)) {
        let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
        let embeddings = match encode_fn(&texts) {
            Ok(e) => e,
            Err(e) => {
                log::warn!(
                    "insert_vectors_batched: encode failed ({e}); leaving remaining chunks to backfill"
                );
                return;
            }
        };
        // Guard against a short result (e.g. embed_via_socket's lossy JSON
        // parsing silently dropping a malformed row): zip() pairs positionally,
        // so a shorter `embeddings` would otherwise misalign the tail of this
        // batch, writing wrong embeddings against wrong chunk_ids. Stop instead
        // of writing a partially-misaligned batch; the periodic backfill's
        // `process_batch` has its own per-chunk retry/skip for this batch and
        // all remaining ones.
        if embeddings.len() != batch.len() {
            log::warn!(
                "insert_vectors_batched: embedding count mismatch (got {}, expected {}); leaving this batch and remaining chunks to backfill",
                embeddings.len(),
                batch.len()
            );
            return;
        }
        for ((chunk_id, _), emb) in batch.iter().zip(embeddings.iter()) {
            write_vec_row(conn, *chunk_id, emb);
        }
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
            // Dimension correctness is already guaranteed by
            // `embed_via_socket_at`'s response validation; here we only need
            // to guard the *count* — a single-chunk request must get back
            // exactly one embedding. Accepting any non-empty result would
            // silently take embeddings[0] even when the encoder returned 2+
            // rows, pairing this chunk with a vector that isn't necessarily
            // its own.
            Ok(Ok(ref embeddings)) if embeddings.len() == 1 => {
                if write_vec_row(conn, *chunk_id, &embeddings[0]) {
                    stats.filled += 1;
                } else {
                    log::warn!("chunk {chunk_id} ({file_path}): insert error — skipping");
                    mark_chunk_skip(conn, *chunk_id, "insert_error");
                    stats.errors += 1;
                }
            }
            Ok(Ok(ref embeddings)) => {
                log::warn!(
                    "chunk {chunk_id} ({file_path}): expected 1 embedding, got {} — skipping",
                    embeddings.len()
                );
                mark_chunk_skip(conn, *chunk_id, "embedding_count_mismatch");
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
    // Clamp the caller-supplied batch size: embedder memory is
    // O(batch × seq²), so a page larger than BACKFILL_BATCH_SIZE sent whole
    // to `encode_fn` (as this function does) can still exhaust memory even
    // with the per-input token cap. Treat the caller's value as an upper
    // hint only.
    let batch_size = batch_size.clamp(1, config::BACKFILL_BATCH_SIZE);

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
    // Clamp the caller-supplied batch size (e.g. `tsm vector-fill
    // --batch-size`): embedder memory is O(batch × seq²), and this function
    // sends each SQL page whole to `encode_fn`, so an unclamped caller value
    // can still exhaust memory even with the per-input token cap. Treat the
    // caller's value as an upper hint only.
    let batch_size = batch_size.clamp(1, config::BACKFILL_BATCH_SIZE);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::index_file;
    use std::io::Write;

    fn setup() -> (Connection, tempfile::TempDir) {
        crate::test_utils::setup_db_with_dir()
    }

    fn write_md(dir: &std::path::Path, rel_path: &str, content: &str) -> std::path::PathBuf {
        let full = dir.join(rel_path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        full
    }

    /// Clear all vectors so tests start from a known state. Needed because
    /// `index_file` may insert vectors if the embedder daemon is running.
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

    // ─── insert_vectors_batched tests ─────────────────────────────

    /// Index a markdown file with `n` H2 sections; return (chunk_id, content).
    fn arrange_chunks(n: usize) -> (Connection, tempfile::TempDir, Vec<(i64, String)>) {
        let (conn, dir) = setup();
        // No leading H1 line: any non-blank text before the first H2 header
        // becomes its own chunk (see chunker::split_by_header), which would
        // throw off the exact chunk count asserted below.
        let mut body = String::new();
        for i in 0..n {
            body.push_str(&format!("\n## Section {i}\n\nContent number {i} here.\n"));
        }
        let path = write_md(dir.path(), "daily/notes/many.md", &body);
        index_file(&conn, &path, dir.path()).unwrap();
        clear_vectors(&conn);
        let entries: Vec<(i64, String)> = conn
            .prepare("SELECT id, content FROM chunks ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(entries.len(), n);
        (conn, dir, entries)
    }

    #[test]
    fn test_insert_vectors_batched_caps_request_size() {
        let (conn, _dir, entries) = arrange_chunks(20);
        let batch_sizes = std::cell::RefCell::new(Vec::new());
        let recording_encode = |texts: &[String]| {
            batch_sizes.borrow_mut().push(texts.len());
            mock_encode(texts)
        };
        insert_vectors_batched(&conn, &entries, &recording_encode, 8);

        // Every request capped at 8; all 20 chunks embedded
        assert_eq!(*batch_sizes.borrow(), vec![8, 8, 4]);
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 20);
    }

    #[test]
    fn test_insert_vectors_batched_stops_on_failure() {
        let (conn, _dir, entries) = arrange_chunks(10);
        let calls = std::cell::Cell::new(0usize);
        let flaky_encode = |texts: &[String]| {
            calls.set(calls.get() + 1);
            if calls.get() >= 2 {
                mock_encode_fail(texts)
            } else {
                mock_encode(texts)
            }
        };
        insert_vectors_batched(&conn, &entries, &flaky_encode, 8);

        // First batch (8) written, second failed → stop, rest left for backfill
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 8);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn test_insert_vectors_batched_stops_on_embedding_count_mismatch() {
        // embed_via_socket's lossy JSON parsing can silently drop a malformed
        // row, returning fewer embeddings than texts. Without a length guard,
        // zip() would still pair the short result with the batch's chunk_ids
        // positionally — writing wrong embeddings to the tail chunk_ids of
        // this batch. The guard must reject the whole batch and stop, per
        // insert_vectors_batched's existing "leave the rest to backfill"
        // contract (mirrors process_batch's mismatch handling).
        let (conn, _dir, entries) = arrange_chunks(10);
        let calls = std::cell::Cell::new(0usize);
        let short_encode = |texts: &[String]| {
            calls.set(calls.get() + 1);
            // Always return one fewer embedding than requested.
            Ok(mock_encode(texts)?.into_iter().skip(1).collect())
        };
        insert_vectors_batched(&conn, &entries, &short_encode, 8);

        // No rows written for the short-returning batch, and processing
        // stopped rather than continuing to the second (2-chunk) batch.
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 0);
        assert_eq!(calls.get(), 1);
    }

    // ─── retry_individually embedding-count tests ─────────────────

    #[test]
    fn test_retry_individually_skips_chunk_with_wrong_embedding_count() {
        // A single-chunk retry request must get back exactly one embedding.
        // Accepting any non-empty result (the old `!embeddings.is_empty()`
        // guard) would take embeddings[0] even when the encoder returned 2+
        // rows for the 1 text sent, silently pairing the chunk with a vector
        // that may not even be its own.
        let (conn, _dir, entries) = arrange_chunks(1);
        let mut stats = BackfillStats::default();
        let two_embeddings_encode = |texts: &[String]| {
            let mut out = mock_encode(texts)?;
            let dup = out.clone();
            out.extend(dup);
            Ok(out)
        };
        let batch: Vec<(i64, String, String)> = entries
            .iter()
            .map(|(id, content)| (*id, content.clone(), "daily/notes/many.md".to_string()))
            .collect();

        retry_individually(&batch, &two_embeddings_encode, &conn, &mut stats);

        assert_eq!(stats.filled, 0, "no vector should be written");
        assert_eq!(stats.errors, 1);
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 0);
        let skip_reason: String = conn
            .query_row(
                "SELECT reason FROM chunks_vec_skip WHERE chunk_id = ?",
                rusqlite::params![entries[0].0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(skip_reason, "embedding_count_mismatch");
    }

    #[test]
    fn test_retry_individually_writes_single_matching_embedding() {
        let (conn, _dir, entries) = arrange_chunks(1);
        let mut stats = BackfillStats::default();
        let batch: Vec<(i64, String, String)> = entries
            .iter()
            .map(|(id, content)| (*id, content.clone(), "daily/notes/many.md".to_string()))
            .collect();

        retry_individually(&batch, &mock_encode, &conn, &mut stats);

        assert_eq!(stats.filled, 1);
        assert_eq!(stats.errors, 0);
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 1);
    }

    // ─── backfill_next_batch clamp test ────────────────────────

    #[test]
    fn test_next_batch_clamps_batch_size_to_backfill_cap() {
        let (conn, _dir, entries) = arrange_chunks(20);
        let batch_sizes = std::cell::RefCell::new(Vec::new());
        let recording_encode = |texts: &[String]| {
            batch_sizes.borrow_mut().push(texts.len());
            mock_encode(texts)
        };

        // Caller passes 64 (e.g. tsm vector-fill --batch-size default) — must
        // be clamped to BACKFILL_BATCH_SIZE so the SQL page (and thus the
        // encode_fn call) never exceeds the attention-memory-safe size.
        let mut last_id = 0i64;
        loop {
            let (stats, has_more) =
                backfill_next_batch(&conn, &recording_encode, 64, last_id).unwrap();
            if !has_more {
                break;
            }
            last_id = stats.last_id;
        }

        assert_eq!(entries.len(), 20);
        assert!(!batch_sizes.borrow().is_empty());
        for size in batch_sizes.borrow().iter() {
            assert!(*size <= config::BACKFILL_BATCH_SIZE);
        }
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 20);
    }

    // ─── backfill_vectors clamp test ────────────────────────────

    #[test]
    fn test_backfill_vectors_clamps_batch_size_to_backfill_cap() {
        // Mirrors test_next_batch_clamps_batch_size_to_backfill_cap but for
        // the `tsm vector-fill --batch-size` entry point (backfill_vectors
        // itself), which sends each SQL page whole to encode_fn and must
        // clamp the caller-supplied batch size the same way.
        let (conn, _dir, entries) = arrange_chunks(20);
        let batch_sizes = std::cell::RefCell::new(Vec::new());
        let recording_encode = |texts: &[String]| {
            batch_sizes.borrow_mut().push(texts.len());
            mock_encode(texts)
        };

        let stats = backfill_vectors(&conn, &recording_encode, 64, None).unwrap();

        assert_eq!(entries.len(), 20);
        assert_eq!(stats.filled, 20);
        assert_eq!(stats.errors, 0);
        assert!(!batch_sizes.borrow().is_empty());
        for size in batch_sizes.borrow().iter() {
            assert!(*size <= config::BACKFILL_BATCH_SIZE);
        }
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vecs, 20);
    }
}
