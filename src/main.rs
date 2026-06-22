use std::fmt;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use the_space_memory::cli;
use the_space_memory::config;
use the_space_memory::daemon_protocol::{self, DaemonRequest, DaemonResponse, ReindexKind};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SearchFallbackArg {
    Error,
    FtsOnly,
}

impl fmt::Display for SearchFallbackArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchFallbackArg::Error => write!(f, "error"),
            SearchFallbackArg::FtsOnly => write!(f, "fts_only"),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ReindexKindArg {
    All,
    Fts,
    Vectors,
}

impl From<ReindexKindArg> for ReindexKind {
    fn from(arg: ReindexKindArg) -> Self {
        match arg {
            ReindexKindArg::All => ReindexKind::All,
            ReindexKindArg::Fts => ReindexKind::Fts,
            ReindexKindArg::Vectors => ReindexKind::Vectors,
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
    /// Project root: the directory holding `tsm.toml` (ADR-0009 §2).
    /// Used when the current directory has no `tsm.toml`. Without either,
    /// commands fail (except `tsm init`, which scaffolds in the CWD).
    #[arg(long, global = true, value_name = "DIR")]
    project_root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum DictCommands {
    /// Show dictionary update candidates (dry run) / apply to add words
    Update {
        /// Minimum frequency threshold
        #[arg(long, default_value = "5")]
        threshold: i64,
        /// Add words to dict and rebuild FTS
        #[arg(long)]
        apply: bool,
    },
    /// Manage reject list (reject_words.txt)
    Reject {
        /// Sync reject_words.txt to DB
        #[arg(long)]
        apply: bool,
        /// Show all rejected words in DB
        #[arg(long, conflicts_with = "apply")]
        all: bool,
    },
    /// Manage user-defined synonyms (.tsm/synonyms.csv)
    Synonym {
        #[command(subcommand)]
        command: SynonymCommands,
    },
}

#[derive(Subcommand)]
enum SynonymCommands {
    /// Sync synonyms.csv to database
    Sync,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the database
    Init,
    /// Start the daemon (tsmd)
    Start {
        /// Skip watcher startup
        #[arg(long)]
        no_watcher: bool,
    },
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
        /// Embedder fallback mode: error (default) or fts_only
        #[arg(long, value_enum)]
        fallback: Option<SearchFallbackArg>,
        /// Filter by path prefix (can be specified multiple times, OR combined)
        #[arg(long = "path")]
        paths: Vec<String>,
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
    /// Manage user dictionary
    Dict {
        #[command(subcommand)]
        command: DictCommands,
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
        /// Actually perform the rebuild (without this flag: dry run)
        #[arg(long)]
        apply: bool,
    },
    /// Re-index in background (requires running daemon)
    Reindex {
        /// What to re-index: all, fts, or vectors
        #[arg(value_enum)]
        kind: ReindexKindArg,
    },
    /// Reload config (tsm.toml) without restarting the daemon
    Reload,
    /// Restart the daemon (stop + start)
    Restart,
}

fn main() -> anyhow::Result<()> {
    // Restore the default SIGPIPE disposition. Rust's runtime sets SIGPIPE to
    // SIG_IGN at startup, which turns a write to a closed pipe into a panic
    // (`failed printing to stdout`) when results are piped into `head`, `pbcopy`,
    // etc. Resetting to SIG_DFL makes the CLI terminate quietly on a broken pipe,
    // matching standard Unix tools. tsmd is a separate binary and re-establishes
    // SIG_IGN at its own startup, so this does not affect the daemon.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    config::ensure_model_cache_env();
    let args = Cli::parse();

    // ADR-0009 §2: resolve the project root (the dir holding `tsm.toml`) from
    // the CWD, `--project-root`, or `$TSM_CONFIG`, and inject it before any
    // config access. `tsm init` tolerates an unresolved root — it scaffolds in
    // the CWD; every other command fails fast with a guiding error.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let tsm_config = std::env::var_os("TSM_CONFIG").map(PathBuf::from);
    let resolved_root =
        config::resolve_project_root(&cwd, args.project_root.as_deref(), tsm_config.as_deref());
    let project_root = match &args.command {
        Commands::Init => resolved_root.unwrap_or(cwd),
        _ => resolved_root?,
    };
    config::set_project_root(project_root);

    the_space_memory::logging::init_logger(the_space_memory::logging::LogMode::Stderr)?;
    match args.command {
        // ── Always direct ──
        Commands::Init => cli::cmd_init()?,
        Commands::Start { no_watcher } => cmd_start(no_watcher, true)?,
        Commands::Stop => cmd_stop()?,
        Commands::Restart => {
            cmd_stop()?;
            cmd_start(false, true)?;
        }
        Commands::Setup => cli::cmd_setup()?,
        Commands::VectorFill { batch_size } => cli::cmd_vector_fill(batch_size)?,

        // ── Direct-only with daemon guard ──
        Commands::Rebuild { apply } => {
            if apply {
                guard_daemon_not_running("rebuild --apply")?;
            }
            cli::cmd_rebuild(apply)?;
        }
        Commands::Dict { command } => match command {
            DictCommands::Update { threshold, apply } => {
                cli::cmd_dict_update(threshold, apply)?;
            }
            DictCommands::Reject { apply, all } => {
                cli::cmd_dict_reject(apply, all)?;
            }
            DictCommands::Synonym { command } => match command {
                SynonymCommands::Sync => {
                    cli::cmd_synonym_sync()?;
                }
            },
        },

        // ── Daemon-routed (auto-starts tsmd if needed) ──
        Commands::Reindex { kind } => {
            let req = DaemonRequest::Reindex { kind: kind.into() };
            render_reindex(send_to_daemon(&req)?)?;
        }

        Commands::Search {
            query,
            top_k,
            format,
            include_content,
            after,
            before,
            recent,
            year,
            fallback,
            paths,
        } => {
            // Always resolve fallback so the daemon uses the CLI caller's config
            let fallback = Some(
                fallback
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| config::search_fallback().to_string()),
            );
            for p in &paths {
                if p.is_empty() {
                    anyhow::bail!("--path cannot be empty");
                }
                if std::path::Path::new(p).is_absolute() {
                    anyhow::bail!(
                        "--path must be a relative path (e.g. 'daily/'), got absolute: {p}"
                    );
                }
            }
            let paths = if paths.is_empty() { None } else { Some(paths) };
            let req = DaemonRequest::Search {
                query,
                top_k,
                format: format.clone(),
                include_content,
                after,
                before,
                recent,
                year,
                fallback,
                paths,
            };
            render_search(send_to_daemon(&req)?, &format)?;
        }

        Commands::Index { files_from_stdin } => {
            let req = if files_from_stdin {
                let index_root = config::index_root();
                let paths = cli::read_paths_from_stdin(&index_root);
                let rel_paths: Vec<String> = paths
                    .iter()
                    .filter_map(|p| p.strip_prefix(&index_root).ok())
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
            // Doctor is a read-only diagnostic: never auto-start the daemon.
            // Use the daemon's in-process report if it is already running,
            // otherwise fall back to a local check (handles uninitialized DBs).
            let socket = config::daemon_socket_path();
            let req = DaemonRequest::Doctor {
                format: format.clone(),
            };
            match daemon_protocol::try_send_request(&socket, &req) {
                Some(Ok(resp)) => render_doctor(resp, &format)?,
                // Socket present but unresponsive (broken or shutting-down
                // daemon): surface the error so it is not silently hidden, but
                // still produce a local report — a diagnostic must never hard-fail.
                Some(Err(e)) => {
                    eprintln!("warning: daemon socket present but did not respond: {e}");
                    cli::cmd_doctor(&format)?;
                }
                // No daemon running: local read-only check.
                None => cli::cmd_doctor(&format)?,
            }
        }

        Commands::ImportWordnet { wordnet_db } => {
            let req = DaemonRequest::ImportWordnet {
                wordnet_db: wordnet_db.to_string_lossy().to_string(),
            };
            render_import_wordnet(send_to_daemon(&req)?)?;
        }

        Commands::Reload => {
            render_reload(send_to_daemon(&DaemonRequest::Reload)?)?;
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

    // Auto-start tsmd (quiet: this is implicit, not an explicit `tsm start`)
    cmd_start(false, false)?;

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
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
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
            let total_hits = payload["total_hits"].as_u64().unwrap_or(0) as usize;
            let results: Vec<the_space_memory::searcher::SearchResult> =
                serde_json::from_value(payload["results"].clone())
                    .map_err(|e| anyhow::anyhow!("Failed to parse search results: {e}"))?;
            print!("{}", cli::format_text(&results, total_hits));
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
        println!("indexed: {indexed}, skipped: {skipped}, removed: {removed}");
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
            println!("session indexed: {name}");
        } else {
            println!("session unchanged: {name}");
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

fn render_reload(resp: DaemonResponse) -> anyhow::Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = &resp.payload {
        if let Some(warnings) = payload.get("warnings").and_then(|w| w.as_array()) {
            for w in warnings {
                if let Some(s) = w.as_str() {
                    eprintln!("warning: {s}");
                }
            }
        }
    }
    println!("config reloaded");
    Ok(())
}

fn render_reindex(resp: DaemonResponse) -> anyhow::Result<()> {
    check_resp(&resp)?;
    println!("reindex started. Run `tsm doctor` to check progress.");
    Ok(())
}

fn render_import_wordnet(resp: DaemonResponse) -> anyhow::Result<()> {
    check_resp(&resp)?;
    if let Some(payload) = resp.payload {
        let count = payload["imported"].as_i64().unwrap_or(0);
        println!("imported {count} synonym pairs from WordNet");
    }
    Ok(())
}

/// Start the tsmd daemon as a background process.
///
/// `verbose` controls success feedback: an explicit `tsm start`/`restart` prints
/// a confirmation, while an implicit auto-start (from a daemon-routed command)
/// stays quiet so it does not prepend noise to that command's output.
fn cmd_start(no_watcher: bool, verbose: bool) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let socket_path = config::daemon_socket_path();

    // Check if already running
    if socket_path.exists() {
        if let Ok(resp) = daemon_protocol::send_request(&socket_path, &DaemonRequest::Ping) {
            if resp.ok {
                report_daemon_state(verbose, "tsmd is already running");
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

    // Capture the detached daemon tree's stderr to a single file instead of
    // inheriting the terminal: a long-lived background process must not spew
    // warnings into the user's shell. Children (embedder, watcher) inherit this
    // fd and log to it too (they keep no separate files), so this is the daemon
    // tree's combined stderr. It is also read back below to surface startup
    // failures. Truncated each start, so it does not accumulate across runs.
    let stderr_path = config::log_dir().join("tsmd-stderr.log");
    if let Some(parent) = stderr_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stderr_file = std::fs::File::create(&stderr_path)
        .map_err(|e| anyhow::anyhow!("Failed to open daemon log {}: {e}", stderr_path.display()))?;

    // Spawn tsmd in a new session (detached)
    let mut cmd = std::process::Command::new(&tsmd_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file));
    if no_watcher {
        cmd.arg("--no-watcher");
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to start tsmd: {e}"))?;

    wait_for_daemon_ready(&mut child, &socket_path, &stderr_path, verbose)
}

/// Wait for the spawned daemon to bind its socket (max 30s), failing fast if it
/// exits before binding (e.g. uninitialized DB) instead of polling the full
/// timeout. The captured stderr is surfaced so the reason is visible.
fn wait_for_daemon_ready(
    child: &mut std::process::Child,
    socket_path: &std::path::Path,
    stderr_path: &std::path::Path,
    verbose: bool,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);
    loop {
        if socket_path.exists() {
            if let Ok(resp) = daemon_protocol::send_request(socket_path, &DaemonRequest::Ping) {
                if resp.ok {
                    report_daemon_state(verbose, "tsmd started");
                    return Ok(());
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let detail = read_daemon_failure(stderr_path);
                anyhow::bail!("tsmd exited before starting ({status}).{detail}");
            }
            Err(e) => anyhow::bail!("failed to poll tsmd startup status: {e}"),
            Ok(None) => {}
        }
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for tsmd to start.");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Report a daemon lifecycle state: print it for explicit commands (`verbose`),
/// otherwise log at info so an implicit auto-start stays quiet by default.
fn report_daemon_state(verbose: bool, message: &str) {
    if verbose {
        println!("{message}");
    } else {
        log::info!("{message}");
    }
}

/// Read a captured daemon stderr log and format its tail for an error message.
fn read_daemon_failure(stderr_path: &std::path::Path) -> String {
    match std::fs::read_to_string(stderr_path) {
        Ok(s) => format_daemon_failure(&s),
        Err(_) => String::new(),
    }
}

/// Format the tail of captured daemon stderr as an indented detail block.
/// Returns an empty string when there is nothing useful to show.
fn format_daemon_failure(stderr: &str) -> String {
    let tail: Vec<&str> = stderr
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .rev()
        .take(5)
        .collect();
    if tail.is_empty() {
        return String::new();
    }
    let body = tail
        .into_iter()
        .rev()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n{body}")
}

/// Stop the tsmd daemon by sending a Shutdown request.
fn cmd_stop() -> anyhow::Result<()> {
    let socket_path = config::daemon_socket_path();

    if !socket_path.exists() {
        println!("tsmd is not running");
        return Ok(());
    }

    match daemon_protocol::send_request(&socket_path, &DaemonRequest::Shutdown) {
        Ok(resp) => {
            if resp.ok {
                println!("tsmd stopped");
            } else {
                log::warn!("tsmd reported error: {}", resp.error.unwrap_or_default());
            }
        }
        Err(e) => {
            log::warn!("could not connect to tsmd: {e}");
            let _ = std::fs::remove_file(&socket_path);
            println!("removed stale socket");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_daemon_failure_empty() {
        assert_eq!(format_daemon_failure(""), "");
        assert_eq!(format_daemon_failure("\n  \n"), "");
    }

    #[test]
    fn test_format_daemon_failure_indents_tail() {
        let out =
            format_daemon_failure("warming up\nError: Database not initialized. Run `tsm init`.\n");
        assert_eq!(
            out,
            "\n  warming up\n  Error: Database not initialized. Run `tsm init`."
        );
    }

    #[test]
    fn test_format_daemon_failure_keeps_last_five_lines() {
        let input = (1..=8)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = format_daemon_failure(&input);
        assert!(out.starts_with("\n  line4"));
        assert!(out.ends_with("  line8"));
        assert!(!out.contains("line3"));
    }
}
