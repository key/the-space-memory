use std::os::unix::net::UnixListener;
use std::path::PathBuf;
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
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    status::update(&data_dir, |s| {
        s.daemon = None;
    });

    Ok(())
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
