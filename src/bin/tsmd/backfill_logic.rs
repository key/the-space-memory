//! Pure, unit-testable logic for the backfill / maintenance paths: the
//! pending-write fairness guard and yield, the dictionary-candidate harvest, and
//! the startup synonym cleanup. Kept separate from `backfill` (the
//! embedder-socket-coupled backfill/reindex passes) so this logic is covered by
//! the coverage gate while the I/O shell stays excluded.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use the_space_memory::{synonyms, tokenizer, user_dict};

use crate::SHUTDOWN;

// ─── Pending-write guard ────────────────────────────────────────────

/// RAII guard that increments a counter on creation and decrements on drop.
///
/// The decrement happens in `Drop`, so leaking the guard (e.g. `mem::forget`)
/// leaves the counter permanently elevated; `yield_to_pending_writes` would then
/// exhaust its full spin budget (~10 s per call) on every invocation instead of
/// short-circuiting at zero.
#[derive(Debug)]
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
pub fn yield_to_pending_writes(writes_pending: &Arc<AtomicUsize>) -> bool {
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

    // ─── yield_to_pending_writes tests ──────────────────────────────

    #[test]
    fn test_yield_returns_false_when_no_writes_pending() {
        // writes_pending == 0 short-circuits before the SHUTDOWN check, so this
        // path is independent of the global flag (no serialization needed).
        let writes_pending = Arc::new(AtomicUsize::new(0));
        assert!(!yield_to_pending_writes(&writes_pending));
    }

    #[test]
    #[serial_test::serial]
    fn test_yield_returns_true_on_shutdown() {
        // A pending write keeps it past the early return; SHUTDOWN then makes it
        // bail on the first iteration (before any sleep). Serialized + reset
        // because it toggles the process-global SHUTDOWN flag.
        let writes_pending = Arc::new(AtomicUsize::new(1));
        SHUTDOWN.store(true, Ordering::SeqCst);
        let result = yield_to_pending_writes(&writes_pending);
        SHUTDOWN.store(false, Ordering::SeqCst);
        assert!(
            result,
            "should return true when shutdown is requested while writes are pending"
        );
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
