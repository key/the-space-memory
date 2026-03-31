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
    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Stderr)?;
    let args = Cli::parse();
    match args.command {
        // ── Always direct ──
        Commands::Init => cli::cmd_init()?,
        Commands::Start => cmd_start()?,
        Commands::Stop => cmd_stop()?,
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

        // ── Daemon-routed (auto-starts tsmd if needed) ──
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
                query,
                top_k,
                format: format.clone(),
                include_content,
                after,
                before,
                recent,
                year,
            };
            render_search(send_to_daemon(&req)?, &format)?;
        }

        Commands::Index { files_from_stdin } => {
            let req = if files_from_stdin {
                let project_root = config::project_root();
                let paths = cli::read_paths_from_stdin(&project_root);
                let rel_paths: Vec<String> = paths
                    .iter()
                    .filter_map(|p| p.strip_prefix(&project_root).ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                DaemonRequest::Index { files: rel_paths }
            } else {
                DaemonRequest::Index { files: vec![] }
            };
            render_index(send_to_daemon(&req)?)?;
        }

        Commands::IngestSession { session_file } => {
            let req = DaemonRequest::IngestSession {
                session_file: session_file.to_string_lossy().to_string(),
            };
            render_ingest(send_to_daemon(&req)?, &session_file)?;
        }

        Commands::Status => {
            render_status(send_to_daemon(&DaemonRequest::Status)?)?;
        }

        Commands::Doctor { format } => {
            let req = DaemonRequest::Doctor {
                format: format.clone(),
            };
            render_doctor(send_to_daemon(&req)?, &format)?;
        }

        Commands::ImportWordnet { wordnet_db } => {
            let req = DaemonRequest::ImportWordnet {
                wordnet_db: wordnet_db.to_string_lossy().to_string(),
            };
            render_import_wordnet(send_to_daemon(&req)?)?;
        }
    }
    Ok(())
}

// ─── Daemon routing helpers ───────────────────────────────────────

/// Send a request to the daemon, auto-starting it if necessary.
fn send_to_daemon(req: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let socket = config::daemon_socket_path();

    // First attempt
    match daemon_protocol::try_send_request(&socket, req) {
        Some(Ok(resp)) => return Ok(resp),
        Some(Err(e)) => {
            anyhow::bail!("Daemon communication error: {e}\nRun `tsm stop` and retry.")
        }
        None => {} // daemon not running, auto-start below
    }

    // Auto-start tsmd
    cmd_start()?;

    // Retry after start
    daemon_protocol::send_request(&socket, req)
}

/// Guard: error if the daemon is running (for commands that can't coexist).
fn guard_daemon_not_running(command: &str) -> anyhow::Result<()> {
    let socket = config::daemon_socket_path();
    match daemon_protocol::try_send_request(&socket, &DaemonRequest::Ping) {
        Some(Ok(resp)) if resp.ok => {
            anyhow::bail!("tsmd is running. Run `tsm stop` before `{command}`.");
        }
        Some(Err(e)) => {
            anyhow::bail!(
                "Could not verify daemon status before `{command}`: {e}\nRun `tsm stop` to ensure the daemon is not running."
            );
        }
        _ => Ok(()), // No socket or ping returned ok: false — safe to proceed
    }
}

// ─── Render helpers (daemon response → terminal output) ───────────

fn print_json(value: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

fn check_resp(resp: &DaemonResponse) -> anyhow::Result<()> {
    if !resp.ok {
        anyhow::bail!(
            "{}",
            resp.error
                .clone()
                .unwrap_or_else(|| "(daemon returned error with no message)".into())
        );
    }
    Ok(())
}

fn render_search(resp: DaemonResponse, format: &str) -> anyhow::Result<()> {
    check_resp(&resp)?;
    let payload = resp.payload.unwrap_or_default();
    match format {
        "json" => print_json(&payload),
        _ => {
            let results: Vec<the_space_memory::searcher::SearchResult> =
                serde_json::from_value(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to parse search results: {e}"))?;
            print!("{}", cli::format_text(&results));
        }
    }
    Ok(())
}

fn render_index(resp: DaemonResponse) -> anyhow::Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let indexed = payload["indexed"].as_i64().unwrap_or(0);
        let skipped = payload["skipped"].as_i64().unwrap_or(0);
        let removed = payload["removed"].as_i64().unwrap_or(0);
        log::info!("indexed: {indexed}, skipped: {skipped}, removed: {removed}");
    }
    Ok(())
}

fn render_ingest(resp: DaemonResponse, session_file: &std::path::Path) -> anyhow::Result<()> {
    check_resp(&resp)?;
    let name = session_file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if let Some(payload) = resp.payload {
        if payload["indexed"].as_bool().unwrap_or(false) {
            log::info!("session indexed: {name}");
        } else {
            log::info!("session unchanged: {name}");
        }
    }
    Ok(())
}

fn render_status(resp: DaemonResponse) -> anyhow::Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let info: cli::StatusInfo = serde_json::from_value(payload).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse daemon status: {e}\nTry `tsm stop && tsm start` to refresh."
            )
        })?;
        cli::print_status_info(&info);
    }
    Ok(())
}

fn render_doctor(resp: DaemonResponse, format: &str) -> anyhow::Result<()> {
    check_resp(&resp)?;
    let payload = resp.payload.unwrap_or_default();
    if format == "json" {
        print_json(&payload);
        return Ok(());
    }
    let report: cli::DoctorReport = serde_json::from_value(payload).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse daemon doctor report: {e}\nTry `tsm stop && tsm start` to refresh."
        )
    })?;
    cli::render_doctor_report(&report);
    Ok(())
}

fn render_import_wordnet(resp: DaemonResponse) -> anyhow::Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let count = payload["imported"].as_i64().unwrap_or(0);
        log::info!("imported {count} synonym pairs from WordNet");
    }
    Ok(())
}

/// Start the tsmd daemon as a background process.
fn cmd_start() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let socket_path = config::daemon_socket_path();

    // Check if already running
    if socket_path.exists() {
        if let Ok(resp) = daemon_protocol::send_request(&socket_path, &DaemonRequest::Ping) {
            if resp.ok {
                log::info!("tsmd is already running");
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
    // Keep stderr inherited so pre-logger startup errors are visible
    let mut cmd = std::process::Command::new(&tsmd_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
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
                    log::info!("tsmd started");
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
        log::info!("tsmd is not running");
        return Ok(());
    }

    match daemon_protocol::send_request(&socket_path, &DaemonRequest::Shutdown) {
        Ok(resp) => {
            if resp.ok {
                log::info!("tsmd stopped");
            } else {
                log::warn!(
                    "tsmd reported error: {}",
                    resp.error.unwrap_or_default()
                );
            }
        }
        Err(e) => {
            log::warn!("could not connect to tsmd: {e}");
            let _ = std::fs::remove_file(&socket_path);
            log::info!("removed stale socket");
        }
    }

    Ok(())
}
