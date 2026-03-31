use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use the_space_memory::cli;
use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest, DaemonResponse};
use the_space_memory::user_dict::DictFormat;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DictFormatArg {
    Simpledic,
    Ipadic,
}

impl From<DictFormatArg> for DictFormat {
    fn from(arg: DictFormatArg) -> Self {
        match arg {
            DictFormatArg::Simpledic => DictFormat::Simpledic,
            DictFormatArg::Ipadic => DictFormat::Ipadic,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "tsm",
    version,
    about = "The Space Memory — knowledge search engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the database
    Init,
    /// Start the daemon (tsmd)
    Start,
    /// Stop the daemon (tsmd)
    Stop,
    /// Index documents
    Index {
        /// Read file paths from stdin
        #[arg(long)]
        files_from_stdin: bool,
    },
    /// Search documents
    Search {
        /// Search query
        #[arg(short, long)]
        query: String,
        /// Number of results
        #[arg(short = 'k', long, default_value = "5")]
        top_k: usize,
        /// Output format (text or json)
        #[arg(short, long, default_value = "text")]
        format: String,
        /// Include full content for top N results
        #[arg(long)]
        include_content: Option<usize>,
        /// Filter: documents after this date (YYYY-MM-DD, YYYY-MM, or YYYY)
        #[arg(long)]
        after: Option<String>,
        /// Filter: documents before this date (YYYY-MM-DD, YYYY-MM, or YYYY)
        #[arg(long)]
        before: Option<String>,
        /// Filter: documents from the last N days (e.g. "30d", "2w")
        #[arg(long)]
        recent: Option<String>,
        /// Filter: documents from a specific year
        #[arg(long)]
        year: Option<i32>,
    },
    /// Ingest a session JSONL file
    IngestSession {
        /// Path to the JSONL file
        session_file: PathBuf,
    },
    /// Start the embedder daemon
    EmbedderStart,
    /// Download model files from HuggingFace Hub
    Setup,
    /// Fill missing vectors for chunks (needs running embedder)
    VectorFill {
        /// Batch size for processing
        #[arg(long, default_value = "64")]
        batch_size: usize,
    },
    /// Import synonyms from Japanese WordNet
    ImportWordnet {
        /// Path to wnjpn.db
        wordnet_db: PathBuf,
    },
    /// Update user dictionary from collected candidates
    DictUpdate {
        /// Minimum frequency threshold
        #[arg(long, default_value = "5")]
        threshold: i64,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
        /// CSV format: simpledic (janome) or ipadic (lindera)
        #[arg(long, value_enum, default_value = "ipadic")]
        format: DictFormatArg,
    },
    /// Show current system status
    Status,
    /// Check system health
    Doctor {
        /// Output format: text (default) or json
        #[arg(short, long, default_value = "text")]
        format: String,
    },
    /// Rebuild database (backup, delete, init, full index)
    Rebuild {
        /// Proceed without confirmation
        #[arg(long)]
        force: bool,
    },
    /// Internal: backfill worker subprocess (do not call directly)
    #[command(hide = true)]
    BackfillWorker,
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.command {
        // ── Always direct ──
        Commands::Init => cli::cmd_init()?,
        Commands::Start => cmd_start()?,
        Commands::Stop => cmd_stop()?,
        Commands::EmbedderStart => cli::cmd_embedder_start(None)?,
        Commands::Setup => cli::cmd_setup()?,
        Commands::BackfillWorker => cli::cmd_backfill_worker()?,
        Commands::VectorFill { batch_size } => cli::cmd_vector_fill(batch_size)?,

        // ── Direct-only with daemon guard ──
        Commands::Rebuild { force } => {
            guard_daemon_not_running("rebuild")?;
            cli::cmd_rebuild(force)?;
        }
        Commands::DictUpdate {
            threshold,
            yes,
            format,
        } => {
            guard_daemon_not_running("dict-update")?;
            cli::cmd_dict_update(threshold, yes, format.into())?;
        }

        // ── Daemon-routed with direct fallback ──
        Commands::Search {
            query,
            top_k,
            format,
            include_content,
            after,
            before,
            recent,
            year,
        } => {
            let req = DaemonRequest::Search {
                query: query.clone(),
                top_k,
                format: format.clone(),
                include_content,
                after: after.clone(),
                before: before.clone(),
                recent: recent.clone(),
                year,
            };
            if let Some(resp) = try_daemon(&req) {
                render_search(resp, &format);
            } else {
                cli::cmd_search(cli::SearchOptions {
                    query: &query,
                    top_k,
                    format: &format,
                    include_content,
                    after: after.as_deref(),
                    before: before.as_deref(),
                    recent: recent.as_deref(),
                    year,
                })?;
            }
        }

        Commands::Index { files_from_stdin } => {
            if !files_from_stdin {
                // Full index — try daemon first
                let req = DaemonRequest::Index { files: vec![] };
                if let Some(resp) = try_daemon(&req) {
                    render_index(resp);
                } else {
                    cli::cmd_index(false)?;
                }
            } else {
                // stdin paths — collect first, then route
                let project_root = config::project_root();
                let paths = cli::read_paths_from_stdin(&project_root);
                let rel_paths: Vec<String> = paths
                    .iter()
                    .filter_map(|p| p.strip_prefix(&project_root).ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();

                let req = DaemonRequest::Index {
                    files: rel_paths.clone(),
                };
                if let Some(resp) = try_daemon(&req) {
                    render_index(resp);
                } else {
                    let stats = cli::run_index(
                        &the_space_memory::db::get_connection(&config::db_path())?,
                        &paths,
                        &project_root,
                    )?;
                    eprintln!(
                        "Indexed: {}, Skipped: {}, Removed: {}",
                        stats.indexed, stats.skipped, stats.removed
                    );
                }
            }
        }

        Commands::IngestSession { session_file } => {
            let req = DaemonRequest::IngestSession {
                session_file: session_file.to_string_lossy().to_string(),
            };
            if let Some(resp) = try_daemon(&req) {
                render_ingest(resp, &session_file);
            } else {
                cli::cmd_ingest_session(&session_file)?;
            }
        }

        Commands::Status => {
            let req = DaemonRequest::Status;
            if let Some(resp) = try_daemon(&req) {
                render_status(resp);
            } else {
                cli::cmd_status()?;
            }
        }

        Commands::Doctor { format } => {
            let req = DaemonRequest::Doctor {
                format: format.clone(),
            };
            if let Some(resp) = try_daemon(&req) {
                render_doctor(resp, &format);
            } else {
                cli::cmd_doctor(&format)?;
            }
        }

        Commands::ImportWordnet { wordnet_db } => {
            let req = DaemonRequest::ImportWordnet {
                wordnet_db: wordnet_db.to_string_lossy().to_string(),
            };
            if let Some(resp) = try_daemon(&req) {
                if resp.ok {
                    if let Some(payload) = resp.payload {
                        let count = payload["imported"].as_i64().unwrap_or(0);
                        eprintln!("Imported {count} synonym pairs from WordNet.");
                    }
                } else {
                    eprintln!("error: {}", resp.error.unwrap_or_default());
                }
            } else {
                cli::cmd_import_wordnet(&wordnet_db)?;
            }
        }
    }
    Ok(())
}

// ─── Daemon routing helpers ───────────────────────────────────────

/// Try to send a request to the daemon. Returns Some(response) if daemon handled it, None to fallback.
fn try_daemon(req: &DaemonRequest) -> Option<DaemonResponse> {
    let socket = config::daemon_socket_path();
    match daemon_protocol::try_send_request(&socket, req) {
        Some(Ok(resp)) => Some(resp),
        Some(Err(e)) => {
            eprintln!("warning: daemon communication error: {e}");
            None // fallback to direct
        }
        None => None, // daemon not running
    }
}

/// Guard: error if the daemon is running (for commands that can't coexist).
fn guard_daemon_not_running(command: &str) -> anyhow::Result<()> {
    let socket = config::daemon_socket_path();
    if let Some(Ok(resp)) = daemon_protocol::try_send_request(&socket, &DaemonRequest::Ping) {
        if resp.ok {
            anyhow::bail!(
                "tsmd is running. Run `tsm stop` before `{command}`."
            );
        }
    }
    Ok(())
}

// ─── Render helpers (daemon response → terminal output) ───────────

fn render_search(resp: DaemonResponse, format: &str) {
    if !resp.ok {
        eprintln!("error: {}", resp.error.unwrap_or_default());
        return;
    }
    let payload = resp.payload.unwrap_or_default();
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        _ => {
            let results: Vec<the_space_memory::searcher::SearchResult> =
                serde_json::from_value(payload).unwrap_or_default();
            print!("{}", cli::format_text(&results));
        }
    }
}

fn render_index(resp: DaemonResponse) {
    if !resp.ok {
        eprintln!("error: {}", resp.error.unwrap_or_default());
        return;
    }
    if let Some(payload) = resp.payload {
        let indexed = payload["indexed"].as_i64().unwrap_or(0);
        let skipped = payload["skipped"].as_i64().unwrap_or(0);
        let removed = payload["removed"].as_i64().unwrap_or(0);
        eprintln!("Indexed: {indexed}, Skipped: {skipped}, Removed: {removed}");
    }
}

fn render_ingest(resp: DaemonResponse, session_file: &std::path::Path) {
    let name = session_file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if !resp.ok {
        eprintln!("error: {}", resp.error.unwrap_or_default());
        return;
    }
    if let Some(payload) = resp.payload {
        if payload["indexed"].as_bool().unwrap_or(false) {
            eprintln!("Session indexed: {name}");
        } else {
            eprintln!("Session unchanged: {name}");
        }
    }
}

fn render_status(resp: DaemonResponse) {
    if !resp.ok {
        eprintln!("error: {}", resp.error.unwrap_or_default());
        return;
    }
    if let Some(payload) = resp.payload {
        match serde_json::from_value::<cli::StatusInfo>(payload) {
            Ok(info) => cli::print_status_info(&info),
            Err(_) => eprintln!("(could not parse daemon status)"),
        }
    }
}

fn render_doctor(resp: DaemonResponse, format: &str) {
    if !resp.ok {
        eprintln!("error: {}", resp.error.unwrap_or_default());
        return;
    }
    let payload = resp.payload.unwrap_or_default();
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        _ => {
            match serde_json::from_value::<cli::DoctorReport>(payload.clone()) {
                Ok(report) => cli::render_doctor_report(&report),
                Err(_) => {
                    // Fallback: print raw JSON
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&payload).unwrap_or_default()
                    );
                }
            }
        }
    }
}

/// Start the tsmd daemon as a background process.
fn cmd_start() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let socket_path = config::daemon_socket_path();

    // Check if already running
    if socket_path.exists() {
        if let Ok(resp) = daemon_protocol::send_request(&socket_path, &DaemonRequest::Ping) {
            if resp.ok {
                eprintln!("tsmd is already running.");
                return Ok(());
            }
        }
        // Stale socket — remove it
        let _ = std::fs::remove_file(&socket_path);
    }

    // Find the tsmd binary (same directory as tsm)
    let exe_dir = std::env::current_exe()?
        .parent()
        .expect("executable has parent dir")
        .to_path_buf();
    let tsmd_path = exe_dir.join("tsmd");

    if !tsmd_path.exists() {
        anyhow::bail!(
            "tsmd binary not found at {}. Build with `cargo build`.",
            tsmd_path.display()
        );
    }

    // Spawn tsmd in a new session (detached)
    let mut cmd = std::process::Command::new(&tsmd_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("Failed to start tsmd: {e}"))?;

    // Wait for socket to appear (max 30 seconds)
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);
    loop {
        if socket_path.exists() {
            if let Ok(resp) = daemon_protocol::send_request(&socket_path, &DaemonRequest::Ping) {
                if resp.ok {
                    eprintln!("tsmd started.");
                    return Ok(());
                }
            }
        }
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for tsmd to start.");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Stop the tsmd daemon by sending a Shutdown request.
fn cmd_stop() -> anyhow::Result<()> {
    let socket_path = config::daemon_socket_path();

    if !socket_path.exists() {
        eprintln!("tsmd is not running.");
        return Ok(());
    }

    match daemon_protocol::send_request(&socket_path, &DaemonRequest::Shutdown) {
        Ok(resp) => {
            if resp.ok {
                eprintln!("tsmd stopped.");
            } else {
                eprintln!(
                    "tsmd reported error: {}",
                    resp.error.unwrap_or_default()
                );
            }
        }
        Err(e) => {
            eprintln!("Could not connect to tsmd: {e}");
            let _ = std::fs::remove_file(&socket_path);
            eprintln!("Removed stale socket.");
        }
    }

    Ok(())
}
