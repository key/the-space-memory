use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;

use the_space_memory::config;
use the_space_memory::daemon;
use the_space_memory::daemon_protocol::{read_request, write_response, DaemonRequest};
use the_space_memory::db;
use the_space_memory::status;

/// Global shutdown flag for signal handlers.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[derive(Parser)]
#[command(name = "tsmd", version, about = "The Space Memory daemon")]
struct Args {
    /// UNIX socket path
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Database path
    #[arg(long)]
    db: Option<PathBuf>,

    /// Skip embedder startup
    #[arg(long)]
    no_embedder: bool,

    /// Skip watcher startup
    #[arg(long)]
    no_watcher: bool,
}

fn main() -> Result<()> {
    config::ensure_model_cache_env();
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon {
        name: "tsmd",
    })?;
    let args = Args::parse();

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
            signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            signal_handler as *const () as libc::sighandler_t,
        );
    }

    // Start child processes.
    // Each child is guarded by a PID file: if a previous instance is still
    // alive (orphaned from a prior tsmd), we skip spawning a duplicate.
    // Children are NOT auto-restarted on crash to prevent OOM restart loops.
    let embedder_pid_path = state_dir.join("embedder.pid");
    let watcher_pid_path = state_dir.join("watcher.pid");

    let mut embedder_child: Option<Child> = if !args.no_embedder {
        if is_process_alive(&embedder_pid_path) {
            log::info!(
                "embedder already running (PID file: {})",
                embedder_pid_path.display()
            );
            None
        } else {
            let _ = std::fs::remove_file(&embedder_pid_path);
            remove_stale_embedder_socket();
            match start_child("tsm-embedder", &[("TSM_EMBEDDER_IDLE_TIMEOUT", "0")]) {
                Ok(mut child) => {
                    let child_pid = child.id();
                    log::info!("embedder started (PID {child_pid})");
                    if let Err(e) = std::fs::write(&embedder_pid_path, child_pid.to_string()) {
                        log::error!(
                            "failed to write embedder PID file: {e}; \
                             killing child to prevent unguarded spawn"
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        None
                    } else {
                        log::info!("embedder PID file: {}", embedder_pid_path.display());
                        status::update(&state_dir, |s| {
                            s.embedder = Some(status::EmbedderStatus {
                                started_at: chrono::Utc::now().to_rfc3339(),
                                pid: child_pid,
                            });
                        });
                        Some(child)
                    }
                }
                Err(e) => {
                    log::error!("failed to start embedder: {e}");
                    None
                }
            }
        }
    } else {
        log::info!("embedder disabled (--no-embedder)");
        None
    };

    let mut watcher_child: Option<Child> = if !args.no_watcher {
        if is_process_alive(&watcher_pid_path) {
            log::info!(
                "watcher already running (PID file: {})",
                watcher_pid_path.display()
            );
            None
        } else {
            let _ = std::fs::remove_file(&watcher_pid_path);
            match start_child("tsm-watcher", &[]) {
                Ok(mut child) => {
                    let child_pid = child.id();
                    log::info!("watcher started (PID {child_pid})");
                    if let Err(e) = std::fs::write(&watcher_pid_path, child_pid.to_string()) {
                        log::error!(
                            "failed to write watcher PID file: {e}; \
                             killing child to prevent unguarded spawn"
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        None
                    } else {
                        log::info!("watcher PID file: {}", watcher_pid_path.display());
                        status::update(&state_dir, |s| {
                            s.watcher = Some(status::WatcherStatus {
                                started_at: chrono::Utc::now().to_rfc3339(),
                                pid: child_pid,
                            });
                        });
                        Some(child)
                    }
                }
                Err(e) => {
                    log::error!("failed to start watcher: {e}");
                    None
                }
            }
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
            run_backfill_pass(&conn, &search_active);
            log::info!("startup backfill complete");
        });
    }

    // Periodic backfill thread
    let backfill_interval_secs = config::embedder_backfill_interval_secs();
    if backfill_interval_secs > 0 && !args.no_embedder {
        let conn = Arc::clone(&conn);
        let search_active = Arc::clone(&search_active);
        std::thread::spawn(move || {
            periodic_backfill(&conn, &search_active, backfill_interval_secs);
        });
    }

    // Accept loop — children are NOT restarted on crash
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let conn = Arc::clone(&conn);
                let index_root = index_root.clone();
                let search_active = Arc::clone(&search_active);

                std::thread::spawn(move || {
                    if let Err(e) = handle_client(&mut stream, &conn, &index_root, &search_active) {
                        log::warn!("client error: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if reap_child("embedder", &mut embedder_child, &embedder_pid_path) {
                    status::update(&state_dir, |s| s.embedder = None);
                }
                if reap_child("watcher", &mut watcher_child, &watcher_pid_path) {
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

    // Cleanup
    log::info!("shutting down");
    stop_child("embedder", embedder_child, &embedder_pid_path);
    stop_child("watcher", watcher_child, &watcher_pid_path);

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    status::update(&state_dir, |s| {
        s.daemon = None;
        s.embedder = None;
        s.watcher = None;
    });

    Ok(())
}

// ─── Child process management ─────────────────────────────────────

/// Find a sibling binary in the same directory as the current executable.
fn sibling_binary(name: &str) -> Result<PathBuf> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .context("executable has no parent directory")?
        .to_path_buf();
    Ok(exe_dir.join(name))
}

/// Start a child process by binary name, with optional environment variables.
fn start_child(binary: &str, env_vars: &[(&str, &str)]) -> Result<Child> {
    let bin_path = sibling_binary(binary)?;
    let mut cmd = Command::new(&bin_path);
    // Keep stderr inherited so pre-logger startup errors are visible
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for &(k, v) in env_vars {
        cmd.env(k, v);
    }
    cmd.spawn().context(format!("Failed to spawn {binary}"))
}

/// Remove the embedder UNIX socket if it exists.
fn remove_stale_embedder_socket() {
    let path = config::embedder_socket_path();
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            log::warn!("could not remove stale embedder socket: {e}");
        }
    }
}

/// Check if a PID file points to a running process.
fn is_process_alive(pid_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<i32>() else {
        return false;
    };
    // kill(pid, 0) checks process existence without sending a signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Detect child exit. Returns `true` if the child exited.
/// The child is NOT restarted — this only logs and cleans up the PID file.
fn reap_child(label: &str, child: &mut Option<Child>, pid_path: &Path) -> bool {
    let Some(c) = child else { return false };
    match c.try_wait() {
        Ok(Some(exit_status)) => {
            if exit_status.success() {
                log::info!("{label} exited normally");
            } else {
                log::warn!("{label} exited with {exit_status}, not restarting");
            }
            *child = None;
            let _ = std::fs::remove_file(pid_path);
            true
        }
        Ok(None) => false,
        Err(e) => {
            log::warn!("error checking {label}: {e}");
            false
        }
    }
}

/// Stop a child process: SIGTERM → wait (2s grace) → SIGKILL. Removes PID file.
fn stop_child(label: &str, child: Option<Child>, pid_path: &Path) {
    if let Some(mut child) = child {
        let pid = child.id();
        log::info!("stopping {label} (PID {pid})...");

        // Send SIGTERM for graceful shutdown
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }

        // Wait up to 2 seconds for graceful exit
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                let _ = std::fs::remove_file(pid_path);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // Force kill if still running
        if let Err(e) = child.kill() {
            log::warn!("failed to kill {label} (PID {pid}): {e}");
        }
        if let Err(e) = child.wait() {
            log::warn!("failed to wait for {label} (PID {pid}): {e}");
        }
    }
    let _ = std::fs::remove_file(pid_path);
}

// ─── Client handling ──────────────────────────────────────────────

/// RAII guard that increments a counter on creation and decrements on drop.
struct SearchActiveGuard(Arc<AtomicUsize>);

impl SearchActiveGuard {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(counter))
    }
}

impl Drop for SearchActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_client(
    stream: &mut std::os::unix::net::UnixStream,
    conn: &Arc<Mutex<rusqlite::Connection>>,
    index_root: &std::path::Path,
    search_active: &Arc<AtomicUsize>,
) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let req = read_request(stream)?;

    // Track active search requests so backfill can yield
    let _guard = if matches!(req, DaemonRequest::Search { .. }) {
        Some(SearchActiveGuard::new(search_active))
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

// ─── Backfill orchestration ──────────────────────────────────────────

/// Run one full backfill pass, releasing the DB lock between batches
/// so search/index requests can proceed.
fn run_backfill_pass(conn: &Arc<Mutex<rusqlite::Connection>>, search_active: &Arc<AtomicUsize>) {
    let encode_fn = |texts: &[String]| {
        the_space_memory::embedder::embed_via_socket(texts)
            .ok_or_else(|| anyhow::anyhow!("embedder not available"))
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
        let result = the_space_memory::indexer::backfill_next_batch(
            &conn,
            &encode_fn,
            config::BACKFILL_BATCH_SIZE,
            last_id,
        );
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
fn periodic_backfill(
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
fn sleep_interruptible(duration: std::time::Duration) {
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
