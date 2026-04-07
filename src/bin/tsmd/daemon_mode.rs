use std::os::unix::net::UnixListener;
use std::process::Child;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use the_space_memory::config;
use the_space_memory::daemon;
use the_space_memory::daemon_protocol::{
    read_request, write_response, DaemonRequest, DaemonResponse,
};
use the_space_memory::db;
use the_space_memory::status;

use crate::{backfill, child, Args, SHUTDOWN};

pub fn run(args: Args) -> Result<()> {
    config::ensure_model_cache_env();
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
        name: "tsmd",
    })?;

    let socket_path = args.socket.unwrap_or_else(config::daemon_socket_path);
    let db_path = args.db.unwrap_or_else(config::db_path);
    let index_root = config::index_root();

    // Open DB connection
    let conn = db::get_connection(&db_path)
        .context(format!("Failed to open DB at {}", db_path.display()))?;
    let conn = Arc::new(Mutex::new(conn));

    // Clean up stale socket
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
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
            let spawned = child::spawn_child(
                "embedder",
                &["--embedder", "--no-idle-timeout"],
                &embedder_pid_path,
            );
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

    // Search-active counter: backfill yields when search requests are in-flight.
    let search_active = Arc::new(AtomicUsize::new(0));

    // Startup backfill — waits for embedder socket then runs one pass.
    if !args.no_embedder {
        let conn = Arc::clone(&conn);
        let search_active = Arc::clone(&search_active);
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
            backfill::run_backfill_pass(&conn, &search_active);
            log::info!("startup backfill complete");
        });
    }

    // Periodic backfill thread
    let backfill_interval_secs = config::embedder_backfill_interval_secs();
    if backfill_interval_secs > 0 && !args.no_embedder {
        let conn = Arc::clone(&conn);
        let search_active = Arc::clone(&search_active);
        std::thread::spawn(move || {
            backfill::periodic_backfill(&conn, &search_active, backfill_interval_secs);
        });
    }

    // ─── Accept loop — children are NOT restarted on crash ───────────
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let conn = Arc::clone(&conn);
                let index_root = index_root.clone();
                let search_active = Arc::clone(&search_active);
                let watcher_pid = Arc::clone(&watcher_pid);

                std::thread::spawn(move || {
                    if let Err(e) = handle_client(
                        &mut stream,
                        &conn,
                        &index_root,
                        &search_active,
                        &watcher_pid,
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

fn handle_client(
    stream: &mut std::os::unix::net::UnixStream,
    conn: &Arc<Mutex<rusqlite::Connection>>,
    index_root: &std::path::Path,
    search_active: &Arc<AtomicUsize>,
    watcher_pid: &Arc<AtomicU32>,
) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let req = read_request(stream)?;

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

    // Track active search requests so backfill can yield
    let _guard = if matches!(req, DaemonRequest::Search { .. }) {
        Some(backfill::SearchActiveGuard::new(search_active))
    } else {
        None
    };

    let conn = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
    let resp = daemon::handle_request(&conn, req, index_root, &SHUTDOWN);
    write_response(stream, &resp)?;
    Ok(())
}
