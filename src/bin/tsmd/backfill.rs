use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use the_space_memory::{config, embedder, indexer};

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

        // Yield while search requests are in-flight (no lock held)
        for _ in 0..200 {
            if search_active.load(Ordering::Acquire) == 0 {
                break;
            }
            if SHUTDOWN.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
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
