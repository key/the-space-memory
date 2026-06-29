use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use the_space_memory::{config, embedder, indexer, status, synonyms, tokenizer, user_dict};

use crate::SHUTDOWN;

// ─── Pending-write guard ────────────────────────────────────────────

/// RAII guard that increments a counter on creation and decrements on drop.
pub struct PendingWriteGuard(Arc<AtomicUsize>);

impl PendingWriteGuard {
    pub fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(counter))
    }
}

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Spin-wait while a write request is pending, checking SHUTDOWN.
/// Returns `true` if shutdown was requested during the wait.
fn yield_to_pending_writes(writes_pending: &Arc<AtomicUsize>) -> bool {
    for _ in 0..200 {
        if writes_pending.load(Ordering::Acquire) == 0 {
            return false;
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

// ─── Best-effort query-candidate harvest ────────────────────────────

/// The dictionary-candidate harvest query for a request, if it should harvest.
///
/// Only a `Search` harvests. The query is reduced to the same keyword form the
/// search itself tokenizes: `run_search` strips temporal expressions via
/// `parse_temporal` before tokenizing, so the harvest strips them too —
/// otherwise a temporal phrase ("先月" etc.) would leak into candidates.
pub fn harvest_query_for(req: &the_space_memory::daemon_protocol::DaemonRequest) -> Option<String> {
    use the_space_memory::{daemon_protocol::DaemonRequest, temporal};
    match req {
        DaemonRequest::Search { query, .. } => {
            let stripped = temporal::parse_temporal(query).query;
            Some(tokenizer::extract_search_keywords(&stripped).join(" "))
        }
        _ => None,
    }
}

/// Harvest dictionary candidates from a search query through the daemon writer.
///
/// Routes the best-effort write through the shared writer `Arc<Mutex<Connection>>`
/// instead of opening a private `db::get_connection` writer, so it serializes
/// with every other write and is visible to the `writes_pending` fairness yield.
/// Tokenization runs before the lock; `lock()` then blocks until the
/// writer is free for the DB upsert only, so a contended write waits rather than
/// racing at the SQLite level and being lost. The caller runs this after the
/// search response, so search latency is unaffected.
pub fn harvest_query_candidates(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
    query: &str,
) {
    // Tokenize + validate outside the writer lock.
    let candidates = user_dict::extract_query_candidates(query);
    if candidates.is_empty() {
        return; // nothing to write — skip the lock entirely
    }
    let _pending = PendingWriteGuard::new(writes_pending);
    match conn.lock() {
        Ok(conn) => user_dict::upsert_candidates(&conn, &candidates, "query"),
        Err(e) => log::warn!("dict candidate harvest: writer lock poisoned: {e}"),
    }
}

/// Delete stale synonym pairs through the daemon writer (once at startup).
///
/// Routing through the shared writer keeps the daemon the sole writer;
/// the `PendingWriteGuard` makes the pass visible to the fairness
/// yield. The pass is independent of the embedder.
pub fn cleanup_stale_synonyms(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
) {
    let _pending = PendingWriteGuard::new(writes_pending);
    match conn.lock() {
        Ok(conn) => synonyms::cleanup_stale(&conn),
        Err(e) => log::warn!("synonym cleanup: writer lock poisoned: {e}"),
    }
}

// ─── Backfill ───────────────────────────────────────────────────────

/// Run one full backfill pass, releasing the DB lock between batches
/// so pending write requests can proceed.
pub fn run_backfill_pass(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
) {
    let encode_fn = |texts: &[String]| {
        embedder::embed_via_socket(texts).ok_or_else(|| anyhow::anyhow!("embedder not available"))
    };

    let mut last_id: i64 = 0;
    let mut total_filled: usize = 0;
    let mut total_errors: usize = 0;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        if yield_to_pending_writes(writes_pending) {
            return;
        }

        // Lock DB only for this one batch
        let Ok(conn) = conn.lock() else { break };
        let result =
            indexer::backfill_next_batch(&conn, &encode_fn, config::BACKFILL_BATCH_SIZE, last_id);
        drop(conn); // release lock immediately after batch

        match result {
            Ok((stats, has_more)) => {
                total_filled += stats.filled;
                total_errors += stats.errors;
                last_id = stats.last_id;
                if !has_more {
                    break;
                }
            }
            Err(e) => {
                log::warn!("backfill batch error: {e}");
                break;
            }
        }
    }

    if total_filled > 0 || total_errors > 0 {
        log::info!("backfill: {total_filled} filled, {total_errors} errors");
    }
}

/// Run periodic backfill in tsmd, yielding to pending write requests.
pub fn periodic_backfill(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
    interval_secs: u64,
) {
    let interval = std::time::Duration::from_secs(interval_secs);

    // Wait one full interval before first check (startup backfill handles the initial run)
    sleep_interruptible(interval);

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        let sock = config::embedder_socket_path();
        if !sock.exists() {
            log::debug!("periodic backfill: embedder socket not found, skipping");
            sleep_interruptible(interval);
            continue;
        }

        // Quick count check (short lock)
        let missing: i64 = {
            let Ok(conn) = conn.lock() else { break };
            conn.query_row(
                "SELECT COUNT(*) FROM chunks c
                 LEFT JOIN chunks_vec v ON c.id = v.rowid
                 LEFT JOIN chunks_vec_skip s ON c.id = s.chunk_id
                 WHERE v.rowid IS NULL AND s.chunk_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0)
        }; // lock released

        if missing > 0 {
            log::debug!("periodic backfill: {missing} vectors missing");
            run_backfill_pass(conn, writes_pending);
        }

        sleep_interruptible(interval);
    }
}

/// Sleep in small increments, checking the shutdown flag.
pub fn sleep_interruptible(duration: std::time::Duration) {
    let step = std::time::Duration::from_secs(10).min(duration);
    let mut remaining = duration;
    while remaining > std::time::Duration::ZERO {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return;
        }
        let sleep_for = step.min(remaining);
        std::thread::sleep(sleep_for);
        remaining = remaining.saturating_sub(sleep_for);
    }
}

// ─── Reindex passes ────────────────────────────────────────────────

/// Run a full FTS re-tokenization pass, yielding to pending writes between batches.
///
/// Resets the lindera segmenter (picks up user dict changes) before starting.
pub fn run_reindex_fts_pass(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
    state_dir: &Path,
) {
    // Get total chunk count (short lock)
    let total: i64 = {
        let Ok(conn) = conn.lock() else {
            log::error!("reindex fts: DB mutex poisoned; aborting before start");
            return;
        };
        conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap_or(0)
    };

    // Reset segmenter in daemon process so new user dict is picked up
    tokenizer::reset_segmenter();

    let started_at = chrono::Utc::now().to_rfc3339();
    status::update(state_dir, |s| {
        s.reindex = Some(status::ReindexStatus {
            kind: the_space_memory::daemon_protocol::ReindexKind::Fts,
            total,
            processed: 0,
            errors: 0,
            started_at: started_at.clone(),
        });
    });

    let batch_size = config::reindex_fts_batch_size();
    let mut last_id: i64 = 0;
    let mut total_inserted: usize = 0;
    let mut is_first = true;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        if yield_to_pending_writes(writes_pending) {
            break;
        }

        let Ok(conn) = conn.lock() else {
            log::error!(
                "reindex fts: DB mutex poisoned mid-batch (last_id={last_id}); \
                 FTS index is partially rebuilt"
            );
            status::update(state_dir, |s| {
                if let Some(ref mut r) = s.reindex {
                    r.errors += 1;
                }
            });
            break;
        };
        let result = indexer::rebuild_fts_next_batch(&conn, last_id, batch_size, is_first);
        drop(conn);

        match result {
            Ok((inserted, new_last_id, has_more)) => {
                total_inserted += inserted;
                last_id = new_last_id;
                is_first = false;

                status::update(state_dir, |s| {
                    if let Some(ref mut r) = s.reindex {
                        r.processed = total_inserted as i64;
                    }
                });

                if !has_more {
                    break;
                }
            }
            Err(e) => {
                log::error!("reindex fts batch error (last_id={last_id}): {e}");
                status::update(state_dir, |s| {
                    if let Some(ref mut r) = s.reindex {
                        r.errors += 1;
                    }
                });
                // Return early — leave s.reindex populated so doctor shows the error state
                return;
            }
        }
    }

    if SHUTDOWN.load(Ordering::SeqCst) {
        log::warn!("reindex fts interrupted by shutdown; FTS index is partially rebuilt");
        // Leave s.reindex populated so doctor can report the incomplete state
    } else {
        log::info!("reindex fts: {total_inserted} chunks re-tokenized");
        status::update(state_dir, |s| s.reindex = None);
    }
}

/// Clear all vectors and re-run backfill from scratch.
///
/// Vector search results will be unavailable from the moment tables are
/// cleared until backfill completes. FTS results remain unaffected.
pub fn run_reindex_vectors_pass(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
    state_dir: &Path,
) {
    if yield_to_pending_writes(writes_pending) {
        return;
    }

    // Get total chunk count and clear vector tables (short lock)
    let total: i64 = {
        let Ok(conn) = conn.lock() else {
            log::error!("reindex vectors: DB mutex poisoned; aborting");
            return;
        };
        let count = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap_or(0);
        if let Err(e) = conn.execute_batch("DELETE FROM chunks_vec; DELETE FROM chunks_vec_skip;") {
            log::error!("reindex vectors: failed to clear tables: {e}");
            return;
        }
        count
    };

    let started_at = chrono::Utc::now().to_rfc3339();
    status::update(state_dir, |s| {
        s.reindex = Some(status::ReindexStatus {
            kind: the_space_memory::daemon_protocol::ReindexKind::Vectors,
            total,
            processed: 0,
            errors: 0,
            started_at,
        });
    });

    log::info!("reindex vectors: cleared, starting backfill...");
    run_backfill_pass(conn, writes_pending);

    if SHUTDOWN.load(Ordering::SeqCst) {
        log::warn!("reindex vectors interrupted by shutdown");
        // Leave s.reindex populated so doctor can report the incomplete state
    } else {
        log::info!("reindex vectors: complete");
        status::update(state_dir, |s| s.reindex = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_write_guard_raii() {
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        {
            let _guard = PendingWriteGuard::new(&counter);
            assert_eq!(counter.load(Ordering::Acquire), 1);

            {
                let _guard2 = PendingWriteGuard::new(&counter);
                assert_eq!(counter.load(Ordering::Acquire), 2);
            }
            // guard2 dropped
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        // guard dropped
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    // ─── harvest_query_candidates tests ─────────────────────────────

    fn writer_arc() -> Arc<Mutex<rusqlite::Connection>> {
        Arc::new(Mutex::new(
            the_space_memory::db::get_memory_connection().unwrap(),
        ))
    }

    fn candidate_count(conn: &Arc<Mutex<rusqlite::Connection>>) -> i64 {
        conn.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn test_harvest_writes_query_candidate_through_writer() {
        let conn = writer_arc();
        let writes_pending = Arc::new(AtomicUsize::new(0));

        harvest_query_candidates(&conn, &writes_pending, "田中さんが東京に行った");

        assert!(
            candidate_count(&conn) > 0,
            "harvest should write at least one candidate through the writer"
        );
        assert_eq!(
            writes_pending.load(Ordering::Acquire),
            0,
            "pending-write guard must be released after harvest"
        );
    }

    #[test]
    fn test_harvest_blocks_until_writer_free_no_loss() {
        let conn = writer_arc();
        let writes_pending = Arc::new(AtomicUsize::new(0));

        // A competing writer grabs the lock first and holds it briefly. The
        // harvest must wait for the lock (not drop via busy_timeout on a
        // separate connection) and still land its write — proving no lost
        // write under writer contention.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let held = Arc::clone(&conn);
        let holder_barrier = Arc::clone(&barrier);
        let holder = std::thread::spawn(move || {
            let guard = held.lock().unwrap();
            holder_barrier.wait(); // signal: lock is held
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(guard);
        });

        barrier.wait(); // proceed only once the holder owns the lock
        harvest_query_candidates(&conn, &writes_pending, "田中さんが東京に行った");
        holder.join().unwrap();

        assert!(
            candidate_count(&conn) > 0,
            "harvest must not lose its write while the writer was contended"
        );
        assert_eq!(writes_pending.load(Ordering::Acquire), 0);
    }

    // ─── harvest_query_for tests ────────────────────────────────────

    fn search_req(query: &str) -> the_space_memory::daemon_protocol::DaemonRequest {
        the_space_memory::daemon_protocol::DaemonRequest::Search {
            query: query.to_string(),
            top_k: 5,
            format: "text".to_string(),
            include_content: None,
            after: None,
            before: None,
            recent: None,
            year: None,
            fallback: None,
            paths: None,
        }
    }

    #[test]
    fn test_harvest_query_for_search_uses_extracted_keywords() {
        let req = search_req("走れメロス");

        let harvested = harvest_query_for(&req).expect("a Search must yield a harvest query");

        // Harvest the extracted keyword form (matching what the search itself
        // tokenizes), not the raw request string.
        assert_eq!(
            harvested,
            the_space_memory::tokenizer::extract_search_keywords("走れメロス").join(" ")
        );
    }

    #[test]
    fn test_harvest_query_for_non_search_is_none() {
        use the_space_memory::daemon_protocol::DaemonRequest;
        assert!(harvest_query_for(&DaemonRequest::Ping).is_none());
        assert!(harvest_query_for(&DaemonRequest::Status).is_none());
    }

    #[test]
    fn test_harvest_query_for_strips_temporal_expressions() {
        use the_space_memory::{temporal, tokenizer};

        // The real search tokenizes the temporal-stripped query
        // (`run_search` → `parse_temporal`), so the harvest must too: a temporal
        // phrase like "先月" must not leak into dictionary candidates.
        let raw = "先月の調査レポート";
        let harvested = harvest_query_for(&search_req(raw)).expect("Search yields a harvest query");

        let stripped = temporal::parse_temporal(raw).query;
        assert_eq!(
            harvested,
            tokenizer::extract_search_keywords(&stripped).join(" ")
        );
        // Guard against regression to raw-query extraction: the temporal token
        // is actually removed (premise: parse_temporal strips "先月").
        assert_ne!(
            harvested,
            tokenizer::extract_search_keywords(raw).join(" "),
            "temporal stripping must change the harvested keywords"
        );
        assert!(
            !harvested.contains("先月"),
            "temporal expression must not appear in harvest keywords: {harvested}"
        );
    }

    // ─── cleanup_stale_synonyms tests ───────────────────────────────

    #[test]
    fn test_cleanup_stale_synonyms_runs_through_writer() {
        let conn = writer_arc();
        let writes_pending = Arc::new(AtomicUsize::new(0));
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
                 VALUES ('old_a', 'old_b', 0.1, 'feedback', 0, '2025-01-01T00:00:00Z')",
                [],
            )
            .unwrap();

        cleanup_stale_synonyms(&conn, &writes_pending);

        let remaining: i64 = conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE word_a = 'old_a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 0,
            "stale synonym pair should be deleted through the writer"
        );
        assert_eq!(
            writes_pending.load(Ordering::Acquire),
            0,
            "pending-write guard must be released after cleanup"
        );
    }
}
