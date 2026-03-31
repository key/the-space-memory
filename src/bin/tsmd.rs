use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;

use the_space_memory::config;
use the_space_memory::daemon;
use the_space_memory::daemon_protocol::{read_request, write_response};
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
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Daemon { name: "tsmd" })?;
    let args = Args::parse();

    let socket_path = args.socket.unwrap_or_else(config::daemon_socket_path);
    let db_path = args.db.unwrap_or_else(config::db_path);
    let project_root = config::project_root();

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
    let data_dir = config::data_dir();
    let socket_str = socket_path.to_string_lossy().to_string();
    status::update(&data_dir, |s| {
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

    // Start child processes
    let mut embedder_child: Option<Child> = if !args.no_embedder {
        remove_stale_embedder_socket();
        match start_child("tsm-embedder", &[("TSM_EMBEDDER_IDLE_TIMEOUT", "0")]) {
            Ok(child) => {
                let child_pid = child.id();
                log::info!("embedder started (PID {child_pid})");
                status::update(&data_dir, |s| {
                    s.embedder = Some(status::EmbedderStatus {
                        started_at: chrono::Utc::now().to_rfc3339(),
                        pid: child_pid,
                    });
                });
                Some(child)
            }
            Err(e) => {
                log::error!("failed to start embedder: {e}");
                None
            }
        }
    } else {
        log::info!("embedder disabled (--no-embedder)");
        None
    };

    let mut watcher_child: Option<Child> = if !args.no_watcher {
        match start_child("tsm-watcher", &[]) {
            Ok(child) => {
                let child_pid = child.id();
                log::info!("watcher started (PID {child_pid})");
                status::update(&data_dir, |s| {
                    s.watcher = Some(status::WatcherStatus {
                        started_at: chrono::Utc::now().to_rfc3339(),
                        pid: child_pid,
                    });
                });
                Some(child)
            }
            Err(e) => {
                log::error!("failed to start watcher: {e}");
                None
            }
        }
    } else {
        log::info!("watcher disabled (--no-watcher)");
        None
    };

    let mut embedder_restarts = 0u32;
    let mut watcher_restarts = 0u32;
    const MAX_CHILD_RESTARTS: u32 = 3;

    // Accept loop
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let conn = Arc::clone(&conn);
                let project_root = project_root.clone();

                std::thread::spawn(move || {
                    if let Err(e) = handle_client(&mut stream, &conn, &project_root) {
                        log::warn!("client error: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if maybe_restart_child("embedder", &mut embedder_child, &mut embedder_restarts, MAX_CHILD_RESTARTS) {
                    remove_stale_embedder_socket();
                }
                maybe_restart_child("watcher", &mut watcher_child, &mut watcher_restarts, MAX_CHILD_RESTARTS);
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
    stop_child("embedder", embedder_child);
    stop_child("watcher", watcher_child);

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    status::update(&data_dir, |s| {
        s.daemon = None;
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
    cmd.spawn()
        .context(format!("Failed to spawn {binary}"))
}

/// Remove the embedder UNIX socket if it exists.
fn remove_stale_embedder_socket() {
    let path = Path::new(config::SOCKET_PATH);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            log::warn!("could not remove stale embedder socket: {e}");
        }
    }
}

/// Check if a child has exited and restart it within the retry limit.
/// Returns `true` if a restart was attempted (for pre-restart hooks like socket cleanup).
fn maybe_restart_child(
    label: &str,
    child: &mut Option<Child>,
    restarts: &mut u32,
    max: u32,
) -> bool {
    let exited = match child {
        Some(c) => match c.try_wait() {
            Ok(Some(exit_status)) => {
                if exit_status.success() {
                    log::info!("{label} exited with status: {exit_status}");
                } else {
                    log::warn!("{label} exited with non-zero status: {exit_status}");
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                log::warn!("error checking {label} status: {e}");
                false
            }
        },
        None => return false,
    };

    if !exited {
        return false;
    }

    if *restarts >= max {
        log::error!("{label} crashed {max} times, giving up");
        *child = None;
        return false;
    }

    *restarts += 1;
    log::warn!("{label} exited, restarting ({restarts}/{max})...");

    // Determine binary name and env vars from label
    let (binary, env_vars): (&str, &[(&str, &str)]) = match label {
        "embedder" => ("tsm-embedder", &[("TSM_EMBEDDER_IDLE_TIMEOUT", "0")]),
        "watcher" => ("tsm-watcher", &[]),
        _ => {
            log::error!("unknown child label: {label}");
            *child = None;
            return false;
        }
    };

    match start_child(binary, env_vars) {
        Ok(new_child) => {
            log::info!("{label} restarted (PID {})", new_child.id());
            *child = Some(new_child);
            *restarts = 0;
            true
        }
        Err(e) => {
            log::error!("failed to restart {label}: {e}");
            *child = None;
            false
        }
    }
}

/// Stop a child process: SIGTERM → wait (2s grace) → SIGKILL.
fn stop_child(label: &str, child: Option<Child>) {
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
}

// ─── Client handling ──────────────────────────────────────────────

fn handle_client(
    stream: &mut std::os::unix::net::UnixStream,
    conn: &Arc<Mutex<rusqlite::Connection>>,
    project_root: &std::path::Path,
) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let req = read_request(stream)?;
    let conn = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
    let resp = daemon::handle_request(&conn, req, project_root, &SHUTDOWN);
    write_response(stream, &resp)?;
    Ok(())
}
