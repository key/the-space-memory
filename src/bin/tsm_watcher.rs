use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest};

/// Global shutdown flag for signal handlers.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[derive(Parser)]
#[command(
    name = "tsm-watcher",
    version,
    about = "File watcher for automatic indexing"
)]
struct Args {
    /// Daemon socket path
    #[arg(long)]
    daemon_socket: Option<PathBuf>,

    /// Project root directory
    #[arg(long)]
    project_root: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let daemon_socket = args
        .daemon_socket
        .unwrap_or_else(config::daemon_socket_path);
    let project_root = args.project_root.unwrap_or_else(config::project_root);

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

    // Set up debounced file watcher
    let (tx, rx) = mpsc::channel();
    let mut debouncer =
        new_debouncer(Duration::from_secs(2), tx).context("Failed to create file watcher")?;

    // Watch content directories
    let mut watched = 0;
    for &(dir, _) in config::CONTENT_DIRS {
        let full_dir = project_root.join(dir);
        if full_dir.is_dir() {
            if let Err(e) =
                debouncer
                    .watcher()
                    .watch(&full_dir, RecursiveMode::Recursive)
            {
                eprintln!("tsm-watcher: warning: cannot watch {}: {e}", full_dir.display());
            } else {
                watched += 1;
            }
        }
    }

    if watched == 0 {
        anyhow::bail!("No content directories found to watch under {}", project_root.display());
    }

    eprintln!(
        "tsm-watcher: watching {watched} directories under {}",
        project_root.display()
    );

    // Event loop
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                let mut files_to_index: HashSet<String> = HashSet::new();
                for event in &events {
                    if event.kind != DebouncedEventKind::Any {
                        continue;
                    }
                    if event.path.extension().is_none_or(|ext| ext != "md") {
                        continue;
                    }
                    match event.path.strip_prefix(&project_root) {
                        Ok(rel) => {
                            files_to_index.insert(rel.to_string_lossy().into_owned());
                        }
                        Err(_) => {
                            eprintln!(
                                "tsm-watcher: warning: path {} outside project root, skipping",
                                event.path.display()
                            );
                        }
                    }
                }

                if !files_to_index.is_empty() {
                    let count = files_to_index.len();
                    let req = DaemonRequest::Index {
                        files: files_to_index.into_iter().collect(),
                    };
                    match daemon_protocol::send_request(&daemon_socket, &req) {
                        Ok(resp) => {
                            if resp.ok {
                                if let Some(payload) = resp.payload {
                                    let indexed = payload["indexed"].as_i64().unwrap_or(0);
                                    let removed = payload["removed"].as_i64().unwrap_or(0);
                                    if indexed > 0 || removed > 0 {
                                        eprintln!(
                                            "tsm-watcher: indexed {indexed}, removed {removed} ({count} file(s))"
                                        );
                                    }
                                }
                            } else {
                                eprintln!(
                                    "tsm-watcher: index error: {}",
                                    resp.error.unwrap_or_default()
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("tsm-watcher: daemon communication error: {e}");
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("tsm-watcher: watch error: {e}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Normal timeout, check shutdown flag
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("tsm-watcher: watcher channel disconnected");
                break;
            }
        }
    }

    eprintln!("tsm-watcher: shutting down");
    Ok(())
}
