use std::os::unix::net::UnixListener;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use the_space_memory::config;
use the_space_memory::daemon;
use the_space_memory::daemon_lock;
use the_space_memory::daemon_protocol::{
    read_request, write_response, DaemonRequest, DaemonResponse,
};
use the_space_memory::db;
use the_space_memory::ipc::accept_blocking;
use the_space_memory::status;

use crate::{backfill, child, Args, SHUTDOWN};

pub fn run(args: Args) -> Result<()> {
    config::ensure_model_cache_env();

    let db_path = args.db.unwrap_or_else(config::db_path);

    // Fail fast on an uninitialized DB BEFORE the logger's `create_dir_all` or
    // the lock/socket operations touch the state directory. `init` is an
    // explicit, separate step; the daemon must never auto-create its
    // schema, nor leave a stray `.tsm` when started in an unconfigured
    // directory. The probe never creates the DB file; a genuine open failure
    // (permissions, corruption) propagates here instead of being misreported.
    if !db::probe_initialized(&db_path)? {
        return Err(db::uninitialized_error(&db_path));
    }

    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
        name: "tsmd",
    })?;

    let socket_path = args.socket.unwrap_or_else(config::daemon_socket_path);
    let project_root = config::project_root();

    // Acquire the per-project startup lock FIRST — before opening the DB or
    // touching the socket — and hold it for the daemon's whole lifetime. The
    // lock (not the PID file) is the sole ownership oracle: acquiring it proves
    // no other live daemon owns this project, so a leftover socket is stale and
    // safe to reclaim; failing to acquire means a live (possibly hung) daemon
    // owns it and we must not steal its socket. Taking it before the DB open
    // makes ownership the first semantic gate, so a losing concurrent
    // `tsm start` sees a clear message rather than a spurious DB error. This
    // serializes concurrent starts on one atomic gate and closes the silent
    // socket clobber.
    let lock_path = config::state_dir().join("tsmd.lock");
    let _startup_lock = match daemon_lock::try_acquire(&lock_path)
        .context(format!("Failed to open lock file {}", lock_path.display()))?
    {
        daemon_lock::LockOutcome::Acquired(guard) => guard,
        daemon_lock::LockOutcome::Held => {
            // current_dir() is the project root tsmd chdir'd to, so it
            // matches what `ps`/`pgrep` show for the owning daemon — unlike
            // config::project_root(), which can diverge under $TSM_CONFIG.
            let here = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            anyhow::bail!(
                "another tsmd already owns this project ({}). Run `tsm stop` first.",
                here.display()
            );
        }
    };

    // Open DB connection. The probe at the top of `run` confirmed the schema was
    // present; `get_connection` opens read-write but never creates the schema
    // (that is `tsm init`'s job).
    let conn = db::get_connection(&db_path)
        .context(format!("Failed to open DB at {}", db_path.display()))?;

    // Re-check under the startup lock: closes the narrow window where the DB is
    // deleted or replaced between the pre-flight probe and this open (the open
    // has create-on-miss semantics, so a vanished DB would otherwise be served
    // schemaless).
    if !db::is_initialized(&conn) {
        return Err(db::uninitialized_error(&db_path));
    }

    // Reject an index that predates absolute-path storage (a relative
    // file_path row would silently mis-match every --path). Same fail-fast path
    // as the guards above — surfaced to the user via `tsm start`.
    db::check_path_schema(&conn)?;

    // Reject a config that still carries the removed `index_root`
    // key.  `anyhow::bail!` here keeps the accept loop clean — no socket is
    // bound, no children are spawned, and `tsm start` surfaces the stderr via
    // `wait_for_daemon_ready` (same path as the uninitialized-DB guard above).
    if let Some(path) = config::legacy_index_root() {
        anyhow::bail!(
            "`index_root = \"{}\"` in tsm.toml is no longer supported. \
             Replace it with `[[index.content_dirs]]` entries with paths relative to \
             the project root.",
            path.display()
        );
    }

    let conn = Arc::new(Mutex::new(conn));

    // Read-only connection pool: serves Status/Doctor/Search/Ping concurrently
    // with the writer (WAL snapshots).
    let read_pool = Arc::new(
        the_space_memory::read_pool::ReadPool::new(&db_path, config::reader_pool_size())
            .context("Failed to open reader pool")?,
    );

    // We hold the lock, so no live daemon exists: a leftover socket is stale.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("Failed to remove stale socket {}", socket_path.display()))?;
    }

    // Bind listener
    let listener = UnixListener::bind(&socket_path)
        .context(format!("Failed to bind socket {}", socket_path.display()))?;
    listener.set_nonblocking(true)?;

    // Write PID file
    let pid = std::process::id();
    let pid_path = config::daemon_pid_path();
    std::fs::write(&pid_path, pid.to_string()).context("Failed to write PID file")?;

    // Update status
    let state_dir = config::state_dir();
    let socket_str = socket_path.to_string_lossy().to_string();
    status::update(&state_dir, |s| {
        s.daemon = Some(status::DaemonStatus {
            started_at: chrono::Utc::now().to_rfc3339(),
            pid,
            socket: socket_str.clone(),
        });
        // Clear stale reindex status from a previous daemon's interrupted run
        if s.reindex.is_some() {
            log::info!("clearing stale reindex status from previous run");
            s.reindex = None;
        }
    });

    log::info!("listening on {} (PID {pid})", socket_path.display());

    // Install signal handlers BEFORE spawning children
    unsafe {
        libc::signal(
            libc::SIGTERM,
            crate::signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            crate::signal_handler as *const () as libc::sighandler_t,
        );
    }

    // Load + validate Lua hooks before serving. Fail-fast on a broken hook —
    // refuse to start in a broken state.
    the_space_memory::lua_hooks::init_hooks()
        .map_err(|e| anyhow::anyhow!("hook load failed: {e}"))?;

    // ─── Start embedder child process ────────────────────────────────
    let embedder_pid_path = state_dir.join("embedder.pid");

    let mut embedder_child: Option<Child> = if !args.no_embedder {
        if child::is_process_alive(&embedder_pid_path) {
            if let Some(pid) = child::read_pid_from_file(&embedder_pid_path) {
                log::info!("embedder already running (PID {pid})");
            } else {
                log::info!(
                    "embedder already running (PID file: {})",
                    embedder_pid_path.display()
                );
            }
            None
        } else {
            let _ = std::fs::remove_file(&embedder_pid_path);
            child::remove_stale_socket(&config::embedder_socket_path());
            let spawned = if let Some(dir) = config::models_dir_complete() {
                let model_arg = format!("--model={}", dir.display());
                child::spawn_child(
                    "embedder",
                    &["--embedder", "--no-idle-timeout", &model_arg],
                    &embedder_pid_path,
                )
            } else {
                log::warn!(
                    "Local model files not found in {}; embedder will use HF Hub cache",
                    config::models_dir().display()
                );
                child::spawn_child(
                    "embedder",
                    &["--embedder", "--no-idle-timeout"],
                    &embedder_pid_path,
                )
            };
            if let Some(ref c) = spawned {
                status::update(&state_dir, |s| {
                    s.embedder = Some(status::EmbedderStatus {
                        started_at: chrono::Utc::now().to_rfc3339(),
                        pid: c.id(),
                    });
                });
            }
            spawned
        }
    } else {
        log::info!("embedder disabled (--no-embedder)");
        None
    };

    // ─── Start watcher child process ─────────────────────────────────
    let watcher_pid_path = state_dir.join("watcher.pid");
    let watcher_pid = Arc::new(AtomicU32::new(0));

    let mut watcher_child: Option<Child> = if !args.no_watcher {
        if child::is_process_alive(&watcher_pid_path) {
            if let Some(existing_pid) = child::read_pid_from_file(&watcher_pid_path) {
                watcher_pid.store(existing_pid, Ordering::Release);
                log::info!("watcher already running (PID {existing_pid})");
            } else {
                log::warn!("watcher PID file unreadable; reload will not reach watcher");
            }
            None
        } else {
            let _ = std::fs::remove_file(&watcher_pid_path);
            let spawned = child::spawn_child("watcher", &["--fs-watcher"], &watcher_pid_path);
            if let Some(ref c) = spawned {
                watcher_pid.store(c.id(), Ordering::Release);
                status::update(&state_dir, |s| {
                    s.watcher = Some(status::WatcherStatus {
                        started_at: chrono::Utc::now().to_rfc3339(),
                        pid: c.id(),
                    });
                });
            }
            spawned
        }
    } else {
        log::info!("watcher disabled (--no-watcher)");
        None
    };

    // Writes-pending counter: backfill yields when write requests are in-flight.
    let writes_pending = Arc::new(AtomicUsize::new(0));
    // Reindex-active flag: at most one reindex runs at a time.
    let reindex_active = Arc::new(AtomicBool::new(false));

    // Startup synonym cleanup — one pass through the writer,
    // independent of the embedder.
    {
        let conn = Arc::clone(&conn);
        let writes_pending = Arc::clone(&writes_pending);
        std::thread::spawn(move || {
            backfill::cleanup_stale_synonyms(&conn, &writes_pending);
        });
    }

    // Startup backfill — waits for embedder socket then runs one pass.
    if !args.no_embedder {
        let conn = Arc::clone(&conn);
        let writes_pending = Arc::clone(&writes_pending);
        std::thread::spawn(move || {
            let sock = config::embedder_socket_path();
            for _ in 0..120 {
                if SHUTDOWN.load(Ordering::SeqCst) || sock.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            if !sock.exists() || SHUTDOWN.load(Ordering::SeqCst) {
                log::warn!("embedder socket not ready; skipping startup backfill");
                return;
            }
            log::info!("starting startup backfill...");
            backfill::run_backfill_pass(&conn, &writes_pending);
            log::info!("startup backfill complete");
        });
    }

    // Periodic backfill thread
    let backfill_interval_secs = config::embedder_backfill_interval_secs();
    if backfill_interval_secs > 0 && !args.no_embedder {
        let conn = Arc::clone(&conn);
        let writes_pending = Arc::clone(&writes_pending);
        std::thread::spawn(move || {
            backfill::periodic_backfill(&conn, &writes_pending, backfill_interval_secs);
        });
    }

    // ─── Accept loop — children are NOT restarted on crash ───────────
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match accept_blocking(&listener) {
            Ok((mut stream, _)) => {
                let conn = Arc::clone(&conn);
                let read_pool = Arc::clone(&read_pool);
                let project_root = project_root.clone();
                let writes_pending = Arc::clone(&writes_pending);
                let watcher_pid = Arc::clone(&watcher_pid);
                let reindex_active = Arc::clone(&reindex_active);
                let state_dir = state_dir.clone();

                std::thread::spawn(move || {
                    if let Err(e) = handle_client(
                        &mut stream,
                        &conn,
                        &read_pool,
                        &project_root,
                        &writes_pending,
                        &watcher_pid,
                        &reindex_active,
                        &state_dir,
                    ) {
                        log::warn!("client error: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if child::reap_child("embedder", &mut embedder_child, &embedder_pid_path) {
                    status::update(&state_dir, |s| s.embedder = None);
                }
                if child::reap_child("watcher", &mut watcher_child, &watcher_pid_path) {
                    watcher_pid.store(0, Ordering::Release);
                    status::update(&state_dir, |s| s.watcher = None);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                log::error!("fatal accept error: {e}");
                break;
            }
        }
    }

    // ─── Cleanup ─────────────────────────────────────────────────────
    log::info!("shutting down");
    child::stop_child("embedder", embedder_child, &embedder_pid_path);
    child::stop_child("watcher", watcher_child, &watcher_pid_path);

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    status::update(&state_dir, |s| {
        s.daemon = None;
        s.embedder = None;
        s.watcher = None;
    });

    Ok(())
}

// ─── Client handling ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_client(
    stream: &mut std::os::unix::net::UnixStream,
    conn: &Arc<Mutex<rusqlite::Connection>>,
    read_pool: &Arc<the_space_memory::read_pool::ReadPool>,
    project_root: &std::path::Path,
    writes_pending: &Arc<AtomicUsize>,
    watcher_pid: &Arc<AtomicU32>,
    reindex_active: &Arc<AtomicBool>,
    state_dir: &std::path::Path,
) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let req = read_request(stream)?;

    // Handle Reindex: spawn background thread and respond immediately
    if let DaemonRequest::Reindex { kind } = req {
        use the_space_memory::daemon_protocol::ReindexKind;

        if reindex_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            write_response(
                stream,
                &DaemonResponse::error("reindex already in progress"),
            )?;
            return Ok(());
        }

        let conn = Arc::clone(conn);
        let writes_pending = Arc::clone(writes_pending);
        let reindex_active = Arc::clone(reindex_active);
        let state_dir = state_dir.to_path_buf();
        std::thread::spawn(move || {
            // RAII guard: reset flag even if the thread panics
            struct ReindexGuard(Arc<AtomicBool>);
            impl Drop for ReindexGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _guard = ReindexGuard(Arc::clone(&reindex_active));

            match kind {
                ReindexKind::Fts => {
                    backfill::run_reindex_fts_pass(&conn, &writes_pending, &state_dir);
                }
                ReindexKind::Vectors => {
                    backfill::run_reindex_vectors_pass(&conn, &writes_pending, &state_dir);
                }
                ReindexKind::All => {
                    backfill::run_reindex_fts_pass(&conn, &writes_pending, &state_dir);
                    if !SHUTDOWN.load(Ordering::SeqCst) {
                        backfill::run_reindex_vectors_pass(&conn, &writes_pending, &state_dir);
                    }
                }
            }
        });

        write_response(stream, &DaemonResponse::success_empty())?;
        return Ok(());
    }

    // Handle Reload: notify watcher child via SIGHUP
    if matches!(req, DaemonRequest::Reload) {
        let mut warnings = config::reload();
        let pid = watcher_pid.load(Ordering::Acquire);
        if pid > 0 {
            let rc = unsafe { libc::kill(pid as i32, libc::SIGHUP) };
            if rc != 0 {
                warnings.push("failed to send SIGHUP to watcher (may have exited)".to_string());
            }
        } else {
            warnings.push("watcher is not running; watch targets not updated".to_string());
        }
        let resp = if warnings.is_empty() {
            DaemonResponse::success_empty()
        } else {
            DaemonResponse::success(serde_json::json!({
                "warnings": warnings,
            }))
        };
        write_response(stream, &resp)?;
        return Ok(());
    }

    // A search harvests its query terms as dictionary candidates. Capture the
    // harvest query before `req` is consumed; the write itself runs after the
    // response.
    let harvest_query = backfill::harvest_query_for(&req);

    let resp = if req.is_read_only() {
        let conn = read_pool.checkout();
        daemon::handle_request(&conn, req, project_root, &SHUTDOWN)
    } else {
        // Mark a write pending BEFORE locking so reindex/backfill yields the
        // writer to us within one batch (mirror of the retired search yield).
        let _pending = backfill::PendingWriteGuard::new(writes_pending);
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
        daemon::handle_request(&conn, req, project_root, &SHUTDOWN)
    };
    write_response(stream, &resp)?;

    // Best-effort dict-candidate harvest, routed through the shared writer and
    // fairness counter rather than a private connection. Runs after
    // the response so it adds nothing to search latency; `lock()` blocks under
    // contention so the write is never lost.
    if let Some(query) = harvest_query {
        backfill::harvest_query_candidates(conn, writes_pending, &query);
    }
    Ok(())
}
