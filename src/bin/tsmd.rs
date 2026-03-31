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
}

fn main() -> Result<()> {
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

    eprintln!("tsmd: listening on {} (PID {pid})", socket_path.display());

    // Start embedder as a child process
    let mut embedder_child: Option<Child> = if !args.no_embedder {
        match start_embedder() {
            Ok(child) => {
                eprintln!("tsmd: embedder started (PID {})", child.id());
                Some(child)
            }
            Err(e) => {
                eprintln!("tsmd: warning: failed to start embedder: {e}");
                None
            }
        }
    } else {
        eprintln!("tsmd: embedder disabled (--no-embedder)");
        None
    };

    // Install signal handlers
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

    let mut embedder_restarts = 0u32;
    const MAX_EMBEDDER_RESTARTS: u32 = 3;

    // Accept loop
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let conn = Arc::clone(&conn);
                let project_root = project_root.clone();

                std::thread::spawn(move || {
                    if let Err(e) = handle_client(&mut stream, &conn, &project_root) {
                        eprintln!("tsmd: client error: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Check embedder health on each idle poll (every 100ms)
                maybe_restart_embedder(&mut embedder_child, &mut embedder_restarts, MAX_EMBEDDER_RESTARTS);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("tsmd: fatal accept error: {e}");
                break;
            }
        }
    }

    // Cleanup
    eprintln!("tsmd: shutting down");

    // Stop embedder child
    if let Some(mut child) = embedder_child {
        eprintln!("tsmd: stopping embedder (PID {})...", child.id());
        let _ = child.kill();
        let _ = child.wait();
    }

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    status::update(&data_dir, |s| {
        s.daemon = None;
    });

    Ok(())
}

/// Check if the embedder child has exited and restart it if within the retry limit.
/// Sets `child` to `None` once the limit is exhausted or when a restart fails to spawn.
fn maybe_restart_embedder(child: &mut Option<Child>, restarts: &mut u32, max: u32) {
    let exited = match child {
        Some(c) => matches!(c.try_wait(), Ok(Some(_))),
        None => return,
    };

    if !exited {
        return;
    }

    if *restarts >= max {
        eprintln!("tsmd: embedder crashed {max} times, giving up");
        *child = None;
        return;
    }

    *restarts += 1;
    eprintln!("tsmd: embedder exited, restarting ({restarts}/{max})...");
    match start_embedder() {
        Ok(new_child) => {
            eprintln!("tsmd: embedder restarted (PID {})", new_child.id());
            *child = Some(new_child);
            *restarts = 0; // reset on successful restart
        }
        Err(e) => {
            eprintln!("tsmd: failed to restart embedder: {e}");
            *child = None;
        }
    }
}

/// Start the embedder as a child process with idle timeout disabled.
fn start_embedder() -> Result<Child> {
    let embedder_socket = Path::new(config::SOCKET_PATH);

    // Clean up stale embedder socket
    if embedder_socket.exists() {
        let _ = std::fs::remove_file(embedder_socket);
    }

    // Find the tsm binary (same directory as tsmd)
    let exe_dir = std::env::current_exe()?
        .parent()
        .context("executable has no parent directory")?
        .to_path_buf();
    let tsm_path = exe_dir.join("tsm");

    let child = Command::new(&tsm_path)
        .arg("embedder-start")
        .env("TSM_EMBEDDER_IDLE_TIMEOUT", "0") // disable idle timeout
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // inherit stderr for embedder logs
        .spawn()
        .context("Failed to spawn embedder process")?;

    // Don't block waiting for embedder socket — model loading can take tens of seconds.
    // The accept loop starts immediately; embed_via_socket returns None until ready,
    // and backfill handles missing vectors later.
    Ok(child)
}

fn handle_client(
    stream: &mut std::os::unix::net::UnixStream,
    conn: &Arc<Mutex<rusqlite::Connection>>,
    project_root: &std::path::Path,
) -> Result<()> {
    // Prevent slow/disconnected clients from holding threads indefinitely
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
