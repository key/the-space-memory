use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use the_space_memory::{config, embedder, indexer, status, tokenizer};

use crate::SHUTDOWN;

// ─── Search-active guard ────────────────────────────────────────────

/// RAII guard that increments a counter on creation and decrements on drop.
pub struct SearchActiveGuard(Arc<AtomicUsize>);

impl SearchActiveGuard {
    pub fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(counter))
    }
}

impl Drop for SearchActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Spin-wait until no search requests are in-flight, checking SHUTDOWN.
/// Returns `true` if shutdown was requested during the wait.
fn yield_to_search(search_active: &Arc<AtomicUsize>) -> bool {
    for _ in 0..200 {
        if search_active.load(Ordering::Acquire) == 0 {
            return false;
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

// ─── Backfill ───────────────────────────────────────────────────────

/// Run one full backfill pass, releasing the DB lock between batches
/// so search/index requests can proceed.
pub fn run_backfill_pass(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    search_active: &Arc<AtomicUsize>,
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

        if yield_to_search(search_active) {
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

/// Run periodic backfill in tsmd, yielding to search requests.
pub fn periodic_backfill(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    search_active: &Arc<AtomicUsize>,
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
            run_backfill_pass(conn, search_active);
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

/// Run a full FTS re-tokenization pass, yielding to search between batches.
///
/// Resets the lindera segmenter (picks up user dict changes) before starting.
pub fn run_reindex_fts_pass(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    search_active: &Arc<AtomicUsize>,
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

    let batch_size = config::REINDEX_FTS_BATCH_SIZE;
    let mut last_id: i64 = 0;
    let mut total_inserted: usize = 0;
    let mut is_first = true;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        yield_to_search(search_active);

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
    search_active: &Arc<AtomicUsize>,
    state_dir: &Path,
) {
    if yield_to_search(search_active) {
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
    run_backfill_pass(conn, search_active);

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
    fn test_search_active_guard_raii() {
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        {
            let _guard = SearchActiveGuard::new(&counter);
            assert_eq!(counter.load(Ordering::Acquire), 1);

            {
                let _guard2 = SearchActiveGuard::new(&counter);
                assert_eq!(counter.load(Ordering::Acquire), 2);
            }
            // guard2 dropped
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        // guard dropped
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }
}
