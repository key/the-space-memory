use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use the_space_memory::cli;
use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest};
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
        Commands::Init => cli::cmd_init()?,
        Commands::Start => cmd_start()?,
        Commands::Stop => cmd_stop()?,
        Commands::Index { files_from_stdin } => cli::cmd_index(files_from_stdin)?,
        Commands::Search {
            query,
            top_k,
            format,
            include_content,
            after,
            before,
            recent,
            year,
        } => cli::cmd_search(cli::SearchOptions {
            query: &query,
            top_k,
            format: &format,
            include_content,
            after: after.as_deref(),
            before: before.as_deref(),
            recent: recent.as_deref(),
            year,
        })?,
        Commands::IngestSession { session_file } => cli::cmd_ingest_session(&session_file)?,
        Commands::EmbedderStart => cli::cmd_embedder_start(None)?,
        Commands::Setup => cli::cmd_setup()?,
        Commands::VectorFill { batch_size } => cli::cmd_vector_fill(batch_size)?,
        Commands::ImportWordnet { wordnet_db } => cli::cmd_import_wordnet(&wordnet_db)?,
        Commands::DictUpdate {
            threshold,
            yes,
            format,
        } => cli::cmd_dict_update(threshold, yes, format.into())?,
        Commands::Status => cli::cmd_status()?,
        Commands::Doctor { format } => cli::cmd_doctor(&format)?,
        Commands::Rebuild { force } => cli::cmd_rebuild(force)?,
        Commands::BackfillWorker => cli::cmd_backfill_worker()?,
    }
    Ok(())
}

/// Start the tsmd daemon as a background process.
fn cmd_start() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let socket_path = config::daemon_socket_path();

    // Check if already running
    if socket_path.exists() {
        // Try to ping
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
            // Verify it responds to ping
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
            // Try to clean up stale socket
            let _ = std::fs::remove_file(&socket_path);
            eprintln!("Removed stale socket.");
        }
    }

    Ok(())
}
