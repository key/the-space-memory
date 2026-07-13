use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use the_space_memory::{config, embedder, indexer, status, tokenizer, user_dict};

use crate::backfill_logic::yield_to_pending_writes;
use crate::SHUTDOWN;

// ─── Backfill ───────────────────────────────────────────────────────

/// Outcome of a [`run_backfill_pass`] call.
///
/// `run_reindex_vectors_pass` calls `run_backfill_pass` for its embedding
/// phase (with `state_dir: None`, since it tracks `s.reindex` itself instead
/// of `s.backfill`) and needs to know whether that nested pass actually
/// finished — folding `completed`/`errors` into its own completion decision
/// keeps `s.reindex` from being cleared as "complete" when the backfill
/// underneath it aborted (a poisoned lock or batch error), which would be
/// the same status-lies-about-progress failure mode this module now guards
/// `s.backfill` against, one level up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackfillPassOutcome {
    pub completed: bool,
    pub filled: usize,
    pub errors: usize,
}

/// Run one full backfill pass, releasing the DB lock between batches
/// so pending write requests can proceed.
///
/// `state_dir` is `Some` for the standalone (startup/periodic) passes, which
/// track live progress in `s.backfill` so `tsm status` reflects an in-flight
/// pass instead of showing `idle` throughout. It is `None` when called from
/// `run_reindex_vectors_pass`, which already tracks the same work under
/// `s.reindex` — writing both would double-count one pass under two keys.
pub fn run_backfill_pass(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
    state_dir: Option<&Path>,
) -> BackfillPassOutcome {
    let encode_fn = |texts: &[String]| {
        embedder::embed_via_socket(texts).ok_or_else(|| anyhow::anyhow!("embedder not available"))
    };

    if let Some(dir) = state_dir {
        let total = {
            let Ok(conn) = conn.lock() else {
                log::error!("backfill: DB mutex poisoned; aborting before start");
                return BackfillPassOutcome::default();
            };
            indexer::count_missing(&conn)
        };
        status::update(dir, |s| {
            s.backfill = Some(status::BackfillStatus {
                total,
                filled: 0,
                errors: 0,
                started_at: chrono::Utc::now().to_rfc3339(),
            });
        });
    }

    let mut last_id: i64 = 0;
    let mut total_filled: usize = 0;
    let mut total_errors: usize = 0;
    // Only a clean `!has_more` exit is true completion; a poisoned lock, a
    // shutdown, or a batch error all leave `s.backfill` populated (mirroring
    // `run_reindex_fts_pass`) so `doctor`/`status` can surface the abnormal
    // stop instead of reporting a finished pass that didn't actually finish.
    let mut completed = false;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        if yield_to_pending_writes(writes_pending) {
            break;
        }

        // Lock DB only for this one batch
        let Ok(conn) = conn.lock() else {
            log::error!("backfill: DB mutex poisoned mid-batch (last_id={last_id})");
            break;
        };
        let result =
            indexer::backfill_next_batch(&conn, &encode_fn, config::BACKFILL_BATCH_SIZE, last_id);
        drop(conn); // release lock immediately after batch

        match result {
            Ok((stats, has_more)) => {
                total_filled += stats.filled;
                total_errors += stats.errors;
                last_id = stats.last_id;
                if let Some(dir) = state_dir {
                    status::update(dir, |s| {
                        if let Some(ref mut b) = s.backfill {
                            b.filled = total_filled;
                            b.errors = total_errors;
                        }
                    });
                }
                if !has_more {
                    completed = true;
                    break;
                }
            }
            Err(e) => {
                log::error!("backfill batch error (last_id={last_id}): {e}");
                break;
            }
        }
    }

    if total_filled > 0 || total_errors > 0 {
        log::info!("backfill: {total_filled} filled, {total_errors} errors");
    }

    if let Some(dir) = state_dir {
        if completed {
            status::update(dir, |s| s.backfill = None);
        } else {
            log::warn!("backfill did not complete; status left populated");
            // Leave s.backfill populated so doctor/status can report the
            // incomplete state; the next daemon startup clears it.
        }
    }

    BackfillPassOutcome {
        completed,
        filled: total_filled,
        errors: total_errors,
    }
}

/// Run periodic backfill in tsmd, yielding to pending write requests.
///
/// `reindex_active` is the same flag `handle_client`'s `Reindex` branch
/// claims via `ReindexGuard` for any [`ReindexKind`](the_space_memory::daemon_protocol::ReindexKind)
/// (Fts, Vectors, or All): a `Vectors`/`All` reindex's embedding phase already
/// tracks progress under `s.reindex` (via its own, guard-internal call to
/// `run_backfill_pass` with `state_dir: None`), so a periodic tick landing
/// mid-reindex would race it — both threads writing `s.backfill`/`s.reindex`
/// for the same rows — and reintroduce the misleading-progress problem this
/// module now guards against. The flag is checked unconditionally rather
/// than only for `Vectors`/`All`, since a bare `Fts` reindex holds it too and
/// special-casing which kinds actually touch `s.backfill` isn't worth the
/// extra branching for one skipped tick. The skipped work is simply picked
/// up on the next tick once the reindex (and its own backfill) has finished.
pub fn periodic_backfill(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    writes_pending: &Arc<AtomicUsize>,
    interval_secs: u64,
    state_dir: &Path,
    reindex_active: &Arc<AtomicBool>,
) {
    let interval = std::time::Duration::from_secs(interval_secs);

    // Wait one full interval before first check (startup backfill handles the initial run)
    sleep_interruptible(interval);

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        if reindex_active.load(Ordering::Acquire) {
            log::debug!("periodic backfill: reindex in progress, skipping this tick");
            sleep_interruptible(interval);
            continue;
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
            indexer::count_missing(&conn)
        }; // lock released

        if missing > 0 {
            log::debug!("periodic backfill: {missing} vectors missing");
            run_backfill_pass(conn, writes_pending, Some(state_dir));
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

    // Reset segmenter and existing-surfaces cache so new user dict is picked up
    tokenizer::reset_segmenter();
    user_dict::reset_existing_surfaces();

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
    // `None`: this pass's progress is already tracked under `s.reindex`
    // above; writing `s.backfill` too would double-count the same work.
    let outcome = run_backfill_pass(conn, writes_pending, None);

    if outcome.errors > 0 {
        status::update(state_dir, |s| {
            if let Some(ref mut r) = s.reindex {
                r.errors = outcome.errors as i64;
            }
        });
    }

    // `outcome.completed` folds in the nested backfill's own poisoned-lock
    // and batch-error exits, not just `SHUTDOWN` — otherwise a backfill that
    // aborted abnormally (vectors just cleared, never fully repopulated)
    // would still be reported as a clean "complete" here.
    if SHUTDOWN.load(Ordering::SeqCst) || !outcome.completed {
        log::warn!(
            "reindex vectors did not complete ({} filled, {} errors); status left populated",
            outcome.filled,
            outcome.errors
        );
        // Leave s.reindex populated so doctor can report the incomplete state
    } else {
        log::info!(
            "reindex vectors: complete ({} filled, {} errors)",
            outcome.filled,
            outcome.errors
        );
        status::update(state_dir, |s| s.reindex = None);
    }
}
