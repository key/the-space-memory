use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::config;
use crate::db;
use crate::embedder;
use crate::indexer;
use crate::searcher;
use crate::user_dict;

pub fn cmd_init() -> anyhow::Result<()> {
    let db_path = config::db_path();
    db::init_db(&db_path)?;
    log::info!("Database initialized at {}", db_path.display());
    Ok(())
}

/// Run indexing on given file paths and return stats (no DB open, no output).
pub fn run_index(
    conn: &rusqlite::Connection,
    file_paths: &[PathBuf],
    index_root: &Path,
) -> anyhow::Result<indexer::IndexStats> {
    indexer::index_all(conn, file_paths, index_root)
}

pub fn cmd_index(files_from_stdin: bool) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;
    let index_root = config::index_root();

    let file_paths: Vec<PathBuf> = if files_from_stdin {
        read_paths_from_stdin(&index_root)
    } else {
        collect_content_files(&index_root)
    };

    let stats = run_index(&conn, &file_paths, &index_root)?;
    log::info!(
        "Indexed: {}, Skipped: {}, Removed: {}",
        stats.indexed,
        stats.skipped,
        stats.removed
    );
    Ok(())
}

pub fn read_paths_from_stdin(index_root: &Path) -> Vec<PathBuf> {
    std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .map(|line| index_root.join(line.trim()))
        .collect()
}

pub fn collect_content_files(index_root: &Path) -> Vec<PathBuf> {
    let dirs = config::content_dirs();
    if dirs.is_empty() {
        // Auto-discover: recursively find .md in non-hidden subdirs of index_root
        let mut files = Vec::new();
        for subdir in discover_subdirs(index_root) {
            collect_md_files_excluding_hidden(&subdir, &mut files);
        }
        files
    } else {
        let mut files = Vec::new();
        for dir in dirs {
            let full_dir = index_root.join(&dir.path);
            if full_dir.is_dir() {
                collect_md_files(&full_dir, &mut files);
            } else {
                log::warn!(
                    "content_dir '{}' not found at {}; skipping",
                    dir.path,
                    full_dir.display()
                );
            }
        }
        files
    }
}

fn collect_md_files_excluding_hidden(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("cannot read directory {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("cannot read entry in {}: {e}", dir.display());
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue;
            }
            collect_md_files_excluding_hidden(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// Discover immediate non-hidden subdirectories of a directory.
fn discover_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    match std::fs::read_dir(dir) {
        Err(e) => {
            log::warn!(
                "cannot read {}: {e}; no subdirectories discovered",
                dir.display()
            );
        }
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("cannot read entry in {}: {e}", dir.display());
                        continue;
                    }
                };
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !name.starts_with('.') {
                        result.push(path);
                    }
                }
            }
        }
    }
    result
}

/// List directories to watch (for the watcher thread). When content_dirs is configured,
/// returns those paths. Otherwise discovers non-hidden subdirs under index_root.
pub fn discover_watch_dirs(index_root: &Path) -> Vec<PathBuf> {
    let dirs = config::content_dirs();
    if dirs.is_empty() {
        discover_subdirs(index_root)
    } else {
        let mut result = Vec::new();
        for dir in dirs {
            let full_dir = index_root.join(&dir.path);
            if full_dir.is_dir() {
                result.push(full_dir);
            } else {
                log::warn!(
                    "content_dir '{}' not found at {}; will not be watched",
                    dir.path,
                    full_dir.display()
                );
            }
        }
        result
    }
}

pub fn collect_md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

pub struct SearchOptions<'a> {
    pub query: &'a str,
    pub top_k: usize,
    pub format: &'a str,
    pub include_content: Option<usize>,
    pub after: Option<&'a str>,
    pub before: Option<&'a str>,
    pub recent: Option<&'a str>,
    pub year: Option<i32>,
    pub fallback: Option<&'a str>,
}

/// Run search and return structured results (no DB open, no output).
pub fn run_search(
    conn: &rusqlite::Connection,
    opts: &SearchOptions,
) -> anyhow::Result<Vec<searcher::SearchResult>> {
    use crate::temporal;

    // Resolve fallback policy: CLI flag > config > default (Error)
    let fallback = match opts.fallback {
        Some(s) => s
            .parse::<config::SearchFallback>()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        None => config::search_fallback(),
    };

    let require_vector = fallback == config::SearchFallback::Error;

    if !require_vector {
        let embedder_socket = config::embedder_socket_path();
        if !embedder_socket.exists() {
            log::warn!("Embedder is not running. Falling back to FTS-only search.");
        }
    }

    let parsed = temporal::parse_temporal(opts.query);
    let filter = temporal::merge_filters(
        opts.after,
        opts.before,
        opts.recent,
        opts.year,
        parsed.filter,
    )?;
    let search_query = &parsed.query;
    searcher::search(
        conn,
        search_query,
        opts.top_k,
        filter.as_ref(),
        require_vector,
    )
}

pub fn cmd_search(opts: SearchOptions) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;

    let results = run_search(&conn, &opts)?;
    match opts.format {
        "json" => print_json(&results, opts.include_content)?,
        _ => print_text(&results),
    }
    Ok(())
}

pub fn format_text(results: &[searcher::SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {} — {} (score: {:.4})\n",
            i + 1,
            r.source_type,
            r.source_file,
            r.section_path,
            r.score
        ));
        out.push_str(&format!("   {}\n", r.snippet));
        if let Some(ref status) = r.status {
            out.push_str(&format!("   status: {status}\n"));
        }
        if !r.related_docs.is_empty() {
            out.push_str("   related:\n");
            for rd in &r.related_docs {
                out.push_str(&format!(
                    "     - [{}] {} (strength: {:.2})\n",
                    rd.link_type, rd.file_path, rd.strength
                ));
            }
        }
        out.push('\n');
    }
    out
}

fn print_text(results: &[searcher::SearchResult]) {
    print!("{}", format_text(results));
}

pub fn format_json(
    results: &[searcher::SearchResult],
    include_content: Option<usize>,
    index_root: &Path,
) -> anyhow::Result<String> {
    let mut json_results: Vec<serde_json::Value> = Vec::new();

    for (i, r) in results.iter().enumerate() {
        let related: Vec<serde_json::Value> = r
            .related_docs
            .iter()
            .map(|rd| {
                serde_json::json!({
                    "file_path": rd.file_path,
                    "link_type": rd.link_type,
                    "strength": rd.strength,
                })
            })
            .collect();

        let mut obj = serde_json::json!({
            "source_file": r.source_file,
            "source_type": r.source_type,
            "section_path": r.section_path,
            "snippet": r.snippet,
            "score": r.score,
            "status": r.status,
            "related_docs": related,
        });

        if let Some(n) = include_content {
            if i < n {
                let full_path = index_root.join(&r.source_file);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    obj["content"] = serde_json::Value::String(content);
                }
            }
        }

        json_results.push(obj);
    }

    Ok(serde_json::to_string_pretty(&json_results)?)
}

fn print_json(
    results: &[searcher::SearchResult],
    include_content: Option<usize>,
) -> anyhow::Result<()> {
    let index_root = config::index_root();
    println!("{}", format_json(results, include_content, &index_root)?);
    Ok(())
}

/// Run session ingestion and return whether the session was newly indexed.
pub fn run_ingest_session(
    conn: &rusqlite::Connection,
    session_file: &Path,
) -> anyhow::Result<bool> {
    if !session_file.exists() {
        anyhow::bail!("File not found: {}", session_file.display());
    }
    indexer::index_session(conn, session_file)
}

pub fn cmd_ingest_session(session_file: &Path) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;
    let indexed = run_ingest_session(&conn, session_file)?;
    let name = session_file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if indexed {
        log::info!("Session indexed: {name}");
    } else {
        log::info!("Session unchanged: {name}");
    }
    Ok(())
}

pub fn cmd_vector_fill(batch_size: usize) -> anyhow::Result<()> {
    // Delegate to tsmd if running
    let sock = config::daemon_socket_path();
    if sock.exists() {
        use crate::daemon_protocol::{send_request, DaemonRequest};
        let resp = send_request(&sock, &DaemonRequest::VectorFill { batch_size })?;
        if !resp.ok {
            anyhow::bail!("{}", resp.error.unwrap_or_default());
        }
        return Ok(());
    }
    // tsmd not running — run directly
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;
    run_vector_fill(&conn, batch_size)
}

/// Run vector backfill via the embedder socket (daemon-safe).
pub fn run_vector_fill(conn: &rusqlite::Connection, batch_size: usize) -> anyhow::Result<()> {
    use crate::status;

    let state_dir = config::state_dir();
    let started_at = chrono::Utc::now().to_rfc3339();

    // Write initial backfill status
    let started_at_clone = started_at.clone();
    status::update(&state_dir, |s| {
        s.backfill = Some(status::BackfillStatus {
            total: 0,
            filled: 0,
            errors: 0,
            started_at: started_at_clone,
        });
    });

    let progress_cb = |total: i64, filled: usize, errors: usize| {
        status::update(&state_dir, |s| {
            if let Some(ref mut b) = s.backfill {
                b.total = total;
                b.filled = filled;
                b.errors = errors;
            }
        });
    };

    let encode_fn = |texts: &[String]| {
        embedder::embed_via_socket(texts).ok_or_else(|| anyhow::anyhow!("embedder not available"))
    };

    let stats = indexer::backfill_vectors(conn, &encode_fn, batch_size, Some(&progress_cb))?;

    if stats.filled > 0 {
        log::info!("Backfilled {} vectors.", stats.filled);
    } else {
        log::info!("No missing vectors.");
    }
    if stats.errors > 0 {
        log::warn!("{} errors during backfill.", stats.errors);
    }

    // Clear backfill status on completion
    status::update(&state_dir, |s| {
        s.backfill = None;
    });

    Ok(())
}

pub fn cmd_import_wordnet(wordnet_db: &Path) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;

    let count = crate::synonyms::import_wordnet(&conn, wordnet_db)?;
    log::info!("Imported {count} synonym pairs from WordNet.");

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
        .unwrap_or(0);
    log::info!("Total synonyms: {total}");
    Ok(())
}

pub fn cmd_setup() -> anyhow::Result<()> {
    // Download model files from HuggingFace Hub
    let api = hf_hub::api::sync::Api::new()?;
    let repo = api.repo(hf_hub::Repo::new(
        "cl-nagoya/ruri-v3-30m".to_string(),
        hf_hub::RepoType::Model,
    ));
    let config_path = repo.get("config.json")?;
    let tokenizer_path = repo.get("tokenizer.json")?;
    let weights_path = repo.get("model.safetensors")?;
    log::info!("Model files downloaded:");
    log::info!("  config:    {}", config_path.display());
    log::info!("  tokenizer: {}", tokenizer_path.display());
    log::info!("  weights:   {}", weights_path.display());
    Ok(())
}

/// Doctor output as a structured result for testability.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CheckItem {
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DoctorSection {
    pub name: String,
    pub items: Vec<CheckItem>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DoctorReport {
    pub sections: Vec<DoctorSection>,
}

impl DoctorReport {
    /// Backward-compatible: collect all OK messages.
    pub fn ok(&self) -> Vec<String> {
        self.sections
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| i.status == CheckStatus::Ok)
            .map(|i| i.message.clone())
            .collect()
    }

    /// Backward-compatible: collect all issue messages.
    pub fn issues(&self) -> Vec<String> {
        self.sections
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| i.status != CheckStatus::Ok)
            .map(|i| match &i.hint {
                Some(hint) => format!("{} {hint}", i.message),
                None => i.message.clone(),
            })
            .collect()
    }

    pub fn issue_count(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| i.status != CheckStatus::Ok)
            .count()
    }

    pub fn to_json(&self) -> String {
        serde_json::json!({
            "sections": self.sections,
            "issue_count": self.issue_count(),
        })
        .to_string()
    }
}

/// Run doctor check with an existing DB connection.
pub fn run_doctor(conn: &rusqlite::Connection, db_path: &Path) -> DoctorReport {
    let mut report = DoctorReport::default();

    // ── Database section ──
    let mut db_section = DoctorSection {
        name: "Database".to_string(),
        items: Vec::new(),
    };

    if let Ok(meta) = std::fs::metadata(db_path) {
        let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
        db_section.items.push(CheckItem {
            status: CheckStatus::Ok,
            message: format!("DB: {} ({size_mb:.1} MB)", db_path.display()),
            hint: None,
        });
    }

    doctor_check_with_conn(conn, &mut report, db_section);
    report
}

pub fn doctor_check(db_path: &Path) -> DoctorReport {
    let mut report = DoctorReport::default();

    // ── Database section ──
    let mut db_section = DoctorSection {
        name: "Database".to_string(),
        items: Vec::new(),
    };

    if db_path.exists() {
        if let Ok(meta) = std::fs::metadata(db_path) {
            let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
            db_section.items.push(CheckItem {
                status: CheckStatus::Ok,
                message: format!("DB: {} ({size_mb:.1} MB)", db_path.display()),
                hint: None,
            });
        }
    } else {
        db_section.items.push(CheckItem {
            status: CheckStatus::Error,
            message: format!("DB: {} does not exist", db_path.display()),
            hint: Some("Run `init`.".to_string()),
        });
        report.sections.push(db_section);
        return report;
    }

    if let Ok(conn) = db::get_connection(db_path) {
        doctor_check_with_conn(&conn, &mut report, db_section);
    } else {
        report.sections.push(db_section);
    }

    report
}

fn doctor_check_with_conn(
    conn: &rusqlite::Connection,
    report: &mut DoctorReport,
    mut db_section: DoctorSection,
) {
    let docs: i64 = match conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)) {
        Ok(n) => n,
        Err(e) => {
            db_section.items.push(CheckItem {
                status: CheckStatus::Error,
                message: format!("Failed to query documents: {e}"),
                hint: Some("Run `init` to initialize the database.".to_string()),
            });
            report.sections.push(db_section);
            return;
        }
    };
    let chunks: i64 = match conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)) {
        Ok(n) => n,
        Err(e) => {
            db_section.items.push(CheckItem {
                status: CheckStatus::Error,
                message: format!("Failed to query chunks: {e}"),
                hint: Some("Run `init` to initialize the database.".to_string()),
            });
            report.sections.push(db_section);
            return;
        }
    };
    db_section.items.push(CheckItem {
        status: CheckStatus::Ok,
        message: format!("Documents: {docs}"),
        hint: None,
    });
    db_section.items.push(CheckItem {
        status: CheckStatus::Ok,
        message: format!("Chunks: {chunks}"),
        hint: None,
    });

    report.sections.push(db_section);

    // ── Embedder section ──
    let mut emb_section = DoctorSection {
        name: "Embedder".to_string(),
        items: Vec::new(),
    };

    let socket = config::embedder_socket_path();
    let timeout = config::embedder_idle_timeout_secs();
    if socket.exists() {
        let timeout_info = if timeout == 0 {
            "idle timeout: disabled".to_string()
        } else {
            format!("idle timeout: {timeout}s")
        };
        emb_section.items.push(CheckItem {
            status: CheckStatus::Ok,
            message: format!("Running ({timeout_info})"),
            hint: None,
        });
    } else {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: "Stopped".to_string(),
            hint: Some("Run `tsmd` to start the daemon (includes embedder).".to_string()),
        });
    }

    let vecs: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
        .unwrap_or(-1);

    // Check if backfill is in progress
    let state_dir = config::state_dir();
    let sf = crate::status::read(&state_dir);
    let backfill_hint = if let Some(ref bf) = sf.backfill {
        let pct = if bf.total > 0 {
            (bf.filled as f64 / bf.total as f64 * 100.0) as u32
        } else {
            0
        };
        let processed = bf.filled + bf.errors;
        let eta = if processed > 0 && bf.total > 0 {
            estimate_eta(&bf.started_at, processed, bf.total as usize)
        } else {
            "calculating...".to_string()
        };
        Some(format!(
            "Backfill in progress: {}/{} ({pct}%), ETA {eta}",
            bf.filled, bf.total
        ))
    } else {
        None
    };

    if vecs < 0 {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Error,
            message: "Vectors: chunks_vec unreadable".to_string(),
            hint: None,
        });
    } else if vecs == 0 && chunks > 0 {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!("Vectors: 0 / {chunks} chunks"),
            hint: Some(
                backfill_hint.unwrap_or_else(|| {
                    "Run `vector-fill` (needs embedder) or `rebuild`.".to_string()
                }),
            ),
        });
    } else if vecs < chunks {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!("Vectors: {vecs} / {chunks} chunks (mismatch)"),
            hint: Some(
                backfill_hint.unwrap_or_else(|| {
                    "Run `vector-fill` (needs embedder) or `rebuild`.".to_string()
                }),
            ),
        });
    } else {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Ok,
            message: format!("Vectors: {vecs} (matches all chunks)"),
            hint: None,
        });
    }

    report.sections.push(emb_section);

    // ── Dictionary section ──
    if db::has_candidates_table(conn) {
        let mut dict_section = DoctorSection {
            name: "Dictionary".to_string(),
            items: Vec::new(),
        };

        let summary = user_dict::candidate_summary(conn);
        dict_section.items.push(CheckItem {
            status: CheckStatus::Ok,
            message: format!(
                "User dict: {} words, {} pending, {} rejected",
                summary.dict_word_count, summary.total_pending, summary.rejected_count
            ),
            hint: None,
        });

        if summary.ready_count > 0 {
            dict_section.items.push(CheckItem {
                status: CheckStatus::Warning,
                message: format!(
                    "{} candidates ready (freq >= {})",
                    summary.ready_count,
                    config::DICT_CANDIDATE_FREQ_THRESHOLD
                ),
                hint: Some("Run `tsm dict update`.".to_string()),
            });
        }

        report.sections.push(dict_section);
    }
}

pub fn cmd_doctor(format: &str) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let report = doctor_check(&db_path);
    match format {
        "json" => {
            let json = report.to_json();
            println!("{json}");
        }
        _ => render_doctor_report(&report),
    }
    Ok(())
}

pub fn render_doctor_report(report: &DoctorReport) {
    let use_color = std::env::var("NO_COLOR").is_err();

    let (green, yellow, red, bold, dim, reset) = if use_color {
        (
            "\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[1m", "\x1b[2m", "\x1b[0m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    // Collect all rendered lines to compute box width
    let title = "Knowledge Search Doctor";
    let mut body_lines: Vec<String> = Vec::new();

    for (i, section) in report.sections.iter().enumerate() {
        if i > 0 {
            body_lines.push(String::new()); // blank separator
        }
        body_lines.push(format!("{bold}  {}{reset}", section.name));
        for item in &section.items {
            let (icon, color) = match item.status {
                CheckStatus::Ok => ("\u{2714}", green),       // ✔
                CheckStatus::Warning => ("\u{26a0}", yellow), // ⚠
                CheckStatus::Error => ("\u{2718}", red),      // ✘
            };
            let line = match &item.hint {
                Some(hint) => format!(
                    "    {color}{icon}{reset} {}  {dim}{hint}{reset}",
                    item.message
                ),
                None => format!("    {color}{icon}{reset} {}", item.message),
            };
            body_lines.push(line);
        }
    }

    // Summary line
    let issue_count = report.issue_count();
    body_lines.push(String::new());
    if issue_count > 0 {
        body_lines.push(format!("  {yellow}{issue_count} issue(s) found.{reset}"));
    } else {
        body_lines.push(format!("  {green}All good.{reset}"));
    }

    // Strip ANSI for width calculation
    let strip_ansi = |s: &str| -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for c in s.chars() {
            if c == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else {
                out.push(c);
            }
        }
        out
    };

    let content_width = body_lines
        .iter()
        .map(|l| strip_ansi(l).chars().count())
        .max()
        .unwrap_or(0)
        .max(title.len() + 4);
    let box_width = content_width + 2; // padding

    // Render box
    println!(
        "{dim}\u{256d}\u{2500} {reset}{bold}{title}{reset} {dim}{}\u{256e}{reset}",
        "\u{2500}".repeat(box_width - title.len() - 3)
    );
    println!(
        "{dim}\u{2502}{reset}{}{dim}\u{2502}{reset}",
        " ".repeat(box_width)
    );

    for line in &body_lines {
        let visible_len = strip_ansi(line).chars().count();
        let pad = box_width.saturating_sub(visible_len);
        println!(
            "{dim}\u{2502}{reset}{line}{}{dim}\u{2502}{reset}",
            " ".repeat(pad)
        );
    }

    println!(
        "{dim}\u{2502}{reset}{}{dim}\u{2502}{reset}",
        " ".repeat(box_width)
    );
    println!(
        "{dim}\u{2570}{}\u{256f}{reset}",
        "\u{2500}".repeat(box_width)
    );
}

/// Structured status information for the system.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StatusInfo {
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub daemon_socket: Option<String>,
    pub embedder_running: bool,
    pub embedder_pid: Option<u32>,
    pub embedder_since: Option<String>,
    pub watcher_running: bool,
    pub watcher_since: Option<String>,
    pub backfill: Option<BackfillInfo>,
    pub documents: Option<i64>,
    pub chunks: Option<i64>,
    pub vectors: Option<i64>,
    pub dict_candidates_ready: Option<i64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct BackfillInfo {
    pub filled: usize,
    pub total: i64,
    pub errors: usize,
    pub since: String,
}

/// Collect system status as structured data.
pub fn run_status(conn: Option<&rusqlite::Connection>) -> StatusInfo {
    use crate::status;

    let state_dir = config::state_dir();
    let sf = status::read(&state_dir);

    let daemon_socket_path = config::daemon_socket_path();
    let daemon_running = daemon_socket_path.exists();
    let daemon_pid = sf.daemon.as_ref().map(|d| d.pid);
    let daemon_socket = sf.daemon.as_ref().map(|d| d.socket.clone());

    let socket = config::embedder_socket_path();
    let embedder_running = socket.exists();
    let embedder_pid = sf.embedder.as_ref().map(|e| e.pid);
    let embedder_since = sf.embedder.as_ref().map(|e| e.started_at.clone());

    let watcher_running = sf.watcher.is_some();
    let watcher_since = sf.watcher.as_ref().map(|w| w.started_at.clone());

    let backfill = sf.backfill.as_ref().map(|bf| BackfillInfo {
        filled: bf.filled,
        total: bf.total,
        errors: bf.errors,
        since: bf.started_at.clone(),
    });

    let (documents, chunks, vectors, dict_candidates_ready) = if let Some(conn) = conn {
        let docs: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap_or(0);
        let ch: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap_or(0);
        let vecs: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap_or(0);
        let dict_ready = if db::has_candidates_table(conn) {
            let summary = user_dict::candidate_summary(conn);
            Some(summary.ready_count)
        } else {
            None
        };
        (Some(docs), Some(ch), Some(vecs), dict_ready)
    } else {
        (None, None, None, None)
    };

    StatusInfo {
        daemon_running,
        daemon_pid,
        daemon_socket,
        embedder_running,
        embedder_pid,
        embedder_since,
        watcher_running,
        watcher_since,
        backfill,
        documents,
        chunks,
        vectors,
        dict_candidates_ready,
    }
}

pub fn print_status_info(info: &StatusInfo) {
    println!("=== The Space Memory Status ===\n");

    // Daemon
    if info.daemon_running {
        if let Some(pid) = info.daemon_pid {
            println!("  Daemon:    running (PID {pid})");
        } else {
            println!("  Daemon:    running");
        }
    } else {
        println!("  Daemon:    stopped");
    }

    // Embedder
    if info.embedder_running {
        if let (Some(pid), Some(ref since)) = (info.embedder_pid, &info.embedder_since) {
            let since_fmt = format_since(since);
            println!("  Embedder:  running (since {since_fmt}, PID {pid})");
        } else {
            println!("  Embedder:  running");
        }
    } else {
        println!("  Embedder:  stopped");
    }

    // Watcher
    if info.watcher_running {
        if let Some(ref since) = info.watcher_since {
            let since_fmt = format_since(since);
            println!("  Watcher:   running (since {since_fmt})");
        } else {
            println!("  Watcher:   running");
        }
    } else {
        println!("  Watcher:   stopped");
    }

    // Backfill
    if let Some(ref bf) = info.backfill {
        let pct = if bf.total > 0 {
            (bf.filled as f64 / bf.total as f64 * 100.0) as u32
        } else {
            0
        };
        let since = format_since(&bf.since);
        let processed = bf.filled + bf.errors;
        let eta = if processed > 0 && bf.total > 0 {
            estimate_eta(&bf.since, processed, bf.total as usize)
        } else {
            "calculating...".to_string()
        };
        println!(
            "  Backfill:  {}/{} ({pct}%) — running since {since}, ETA {eta}",
            bf.filled, bf.total
        );
        if bf.errors > 0 {
            println!("             {} errors", bf.errors);
        }
    } else {
        println!("  Backfill:  idle");
    }

    // DB stats
    if let (Some(docs), Some(chunks), Some(vecs)) = (info.documents, info.chunks, info.vectors) {
        println!("  Documents: {docs}");
        println!("  Chunks:    {chunks}");
        if chunks > 0 {
            let pct = (vecs as f64 / chunks as f64 * 100.0) as u32;
            println!("  Vectors:   {vecs}/{chunks} ({pct}%)");
        } else {
            println!("  Vectors:   0");
        }

        if let Some(ready) = info.dict_candidates_ready {
            if ready > 0 {
                println!("  Dict:      {ready} candidates ready");
            } else {
                println!("  Dict:      no candidates ready");
            }
        }
    } else {
        println!("  DB:        not found");
    }
}

pub fn cmd_status() -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path).ok();
    let info = run_status(conn.as_ref());
    print_status_info(&info);
    Ok(())
}

fn format_since(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

fn estimate_eta(started_at: &str, processed: usize, total: usize) -> String {
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return "unknown".to_string();
    };
    let elapsed = chrono::Utc::now().signed_duration_since(start);
    let elapsed_secs = elapsed.num_seconds() as f64;
    if elapsed_secs <= 0.0 || processed == 0 {
        return "calculating...".to_string();
    }
    let remaining = total.saturating_sub(processed);
    let rate = processed as f64 / elapsed_secs;
    let eta_secs = (remaining as f64 / rate) as i64;
    if eta_secs < 60 {
        format!("~{eta_secs}s")
    } else {
        format!("~{}m", eta_secs / 60)
    }
}

pub fn cmd_dict_update(
    threshold: i64,
    apply: bool,
    format: user_dict::DictFormat,
) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;

    let candidates = user_dict::get_threshold_candidates(&conn, threshold);
    if candidates.is_empty() {
        log::info!("No candidates meet the threshold (freq >= {threshold}).");
        return Ok(());
    }

    // Interactive TUI output — bypass log system for clean display
    eprintln!("=== Dictionary Update Candidates ===\n");
    for c in &candidates {
        print_candidate(c);
    }
    eprintln!(
        "\n{} word(s) will be added to user dictionary.",
        candidates.len()
    );

    if !apply {
        log::info!("Dry run. Pass --apply to add words and rebuild FTS.");
        return Ok(());
    }

    // Export to CSV
    let csv_path = config::user_dict_path();
    let exported = user_dict::export_candidates_to_csv(&conn, &csv_path, threshold, format)?;
    let count = exported.len();
    log::info!("Wrote {count} word(s) to {}", csv_path.display());

    if count == 0 {
        log::info!("All candidates were already in the dict file. Nothing to do.");
        return Ok(());
    }

    drop(conn);

    // Rebuild FTS only (dict changes only affect tokenization, not vectors)
    log::info!("\nRebuilding FTS index...");
    cmd_rebuild_fts()?;

    // Save current branch to return to later
    let original_branch =
        get_command_output("git", &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();

    // Create branch and PR via gh
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let branch = format!("user-dict-{timestamp}");
    let csv_path_str = csv_path.to_string_lossy();

    run_command("git", &["checkout", "-b", &branch])?;
    run_command("git", &["add", &csv_path_str])?;
    run_command(
        "git",
        &[
            "commit",
            "-m",
            &format!("feat: user dict update ({count} words)"),
        ],
    )?;

    if let Err(e) = run_command("git", &["push", "-u", "origin", &branch]) {
        // Return to original branch before failing
        let _ = std::process::Command::new("git")
            .args(["checkout", &original_branch])
            .status();
        anyhow::bail!(
            "git push failed: {e}. CSV was updated and index rebuilt, but PR not created."
        );
    }

    let pr_title = format!("feat: user dict update ({count} words)");
    let pr_body = format!(
        "## Summary\n\n- Added {count} word(s) to user dictionary\n- Auto-generated by `tsm dict update`\n\n## Words added\n\n{}",
        exported
            .iter()
            .map(|c| format!("- {} ({} hits)", c.surface, c.frequency))
            .collect::<Vec<_>>()
            .join("\n")
    );

    if let Err(e) = run_command(
        "gh",
        &["pr", "create", "--title", &pr_title, "--body", &pr_body],
    ) {
        log::warn!("`gh pr create` failed ({e}). Push succeeded — create the PR manually.");
    }

    // Return to original branch
    if !original_branch.is_empty() {
        let _ = std::process::Command::new("git")
            .args(["checkout", &original_branch])
            .status();
    }

    Ok(())
}

pub fn cmd_dict_reject(apply: bool, all: bool) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;
    let reject_path = config::reject_words_path();

    if all {
        let rejected = user_dict::get_rejected_candidates(&conn);
        if rejected.is_empty() {
            log::info!("No rejected candidates.");
            return Ok(());
        }
        eprintln!("=== Rejected Candidates ({} total) ===\n", rejected.len());
        for c in &rejected {
            print_candidate(c);
        }
        return Ok(());
    }

    let reject_words = user_dict::load_reject_words(&reject_path)?;

    if reject_words.is_empty() {
        log::info!(
            "reject_words.txt is empty or not found at {}",
            reject_path.display()
        );
        return Ok(());
    }

    let candidates = user_dict::get_pending_in_reject_list(&conn, &reject_words);

    if candidates.is_empty() {
        log::info!("No pending candidates match reject_words.txt.");
        return Ok(());
    }

    eprintln!("=== Candidates to Reject ===\n");
    for c in &candidates {
        print_candidate(c);
    }
    eprintln!("\n{} word(s) will be marked rejected.", candidates.len());

    if !apply {
        log::info!("Dry run. Pass --apply to reject these words.");
        return Ok(());
    }

    let newly_rejected = user_dict::apply_reject_list(&conn, &reject_words)?;
    log::info!("Rejected {} word(s).", newly_rejected.len());
    Ok(())
}

fn print_candidate(c: &user_dict::Candidate) {
    eprintln!(
        "  {:<20} {:>3} hits  (first: {}, last: {})",
        c.surface,
        c.frequency,
        &c.first_seen[..10.min(c.first_seen.len())],
        &c.last_seen[..10.min(c.last_seen.len())]
    );
}

fn run_command(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute `{cmd}`: {e}"))?;
    if !status.success() {
        anyhow::bail!("`{cmd} {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

fn get_command_output(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Spawn `tsm vector-fill` as a detached child process in a new session.
fn spawn_background_backfill() {
    use std::os::unix::process::CommandExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::error!("Cannot determine executable path: {e}");
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("vector-fill")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach into a new session so Ctrl-C on the parent doesn't kill the child
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(_) => {}
        Err(e) => log::error!("Failed to start background backfill: {e}"),
    }
}

pub fn cmd_rebuild(force: bool) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let index_root = config::index_root();
    let socket = config::embedder_socket_path();

    if !socket.exists() {
        log::warn!("Embedder is not running. Rebuilding without vectors.");
        if !force {
            anyhow::bail!("Use --force to proceed without embedder.");
        }
    } else {
        log::info!("Embedder: running");
    }

    // Backup
    if db_path.exists() {
        let backup = db_path.with_extension("db.bak");
        std::fs::copy(&db_path, &backup)?;
        log::info!("Backup: {}", backup.display());
        std::fs::remove_file(&db_path)?;
        log::info!("Deleted: {}", db_path.display());
    }

    // Init
    db::init_db(&db_path)?;
    log::info!("DB initialized");

    // Full index (synchronous, with progress)
    let conn = db::get_connection(&db_path)?;
    let file_paths = collect_content_files(&index_root);
    let total = file_paths.len();
    log::info!("Indexing {total} files...");

    let progress = |current: usize, total: usize, path: &Path| {
        let rel = path.strip_prefix(&index_root).unwrap_or(path).display();
        log::debug!("  [{current}/{total}] {rel}");
    };
    let stats = indexer::index_all_with_progress(&conn, &file_paths, &index_root, Some(&progress))?;
    log::info!(
        "Done: Indexed: {}, Skipped: {}, Removed: {}",
        stats.indexed,
        stats.skipped,
        stats.removed
    );

    // Report & async backfill
    let chunks: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap_or(0);
    let vecs: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
        .unwrap_or(0);
    drop(conn);

    if vecs >= chunks {
        log::info!("Vectors: {vecs} (matches all chunks)");
    } else if socket.exists() && chunks > 0 {
        let current_status = crate::status::read(&config::state_dir());
        if current_status.backfill.is_some() {
            log::info!("Vectors: {vecs} / {chunks} — backfill already in progress");
        } else {
            log::info!("Vectors: {vecs} / {chunks} — starting backfill in background...");
            spawn_background_backfill();
        }
        log::info!("Run `tsm doctor` to check progress.");
    } else if chunks > 0 {
        log::warn!("Vectors: {vecs} / {chunks} — embedder not running, skipping backfill");
    }

    Ok(())
}

pub fn cmd_rebuild_fts() -> anyhow::Result<()> {
    let db_path = config::db_path();

    if !db_path.exists() {
        anyhow::bail!("Database does not exist. Run `tsm init` first.");
    }

    let conn = db::get_connection(&db_path)?;

    let chunk_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap_or(0);
    log::warn!("This will clear and repopulate the FTS index ({chunk_count} chunks).");
    log::info!("Rebuilding FTS index...");

    let progress = |current: usize, total: usize| {
        log::debug!("  [{current}/{total}]");
    };

    let inserted = indexer::rebuild_fts(&conn, Some(&progress))?;
    log::info!("FTS rebuild complete: {inserted} chunks re-indexed.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::searcher::SearchResult;

    #[test]
    fn test_format_text_empty() {
        let result = format_text(&[]);
        assert_eq!(result, "No results found.");
    }

    #[test]
    fn test_format_text_with_results() {
        let results = vec![SearchResult {
            source_file: "daily/notes/test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Test > Section".to_string(),
            snippet: "Some content".to_string(),
            score: 0.5,
            status: Some("current".to_string()),
            related_docs: vec![],
        }];
        let text = format_text(&results);
        assert!(text.contains("1. [note]"));
        assert!(text.contains("daily/notes/test.md"));
        assert!(text.contains("0.5000"));
        assert!(text.contains("status: current"));
    }

    #[test]
    fn test_format_text_no_status() {
        let results = vec![SearchResult {
            source_file: "test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.3,
            status: None,
            related_docs: vec![],
        }];
        let text = format_text(&results);
        assert!(!text.contains("status:"));
    }

    #[test]
    fn test_format_json_empty() {
        let result = format_json(&[], None, Path::new("/tmp")).unwrap();
        assert_eq!(result, "[]");
    }

    #[test]
    fn test_format_json_with_results() {
        let results = vec![SearchResult {
            source_file: "test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.5,
            status: None,
            related_docs: vec![],
        }];
        let json = format_json(&results, None, Path::new("/tmp")).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["source_file"], "test.md");
        assert_eq!(parsed[0]["score"], 0.5);
        // No content field when include_content is None
        assert!(parsed[0].get("content").is_none());
    }

    #[test]
    fn test_format_json_with_include_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.md");
        std::fs::write(&file_path, "# Hello\n\nWorld.").unwrap();

        let results = vec![SearchResult {
            source_file: "test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.5,
            status: None,
            related_docs: vec![],
        }];
        let json = format_json(&results, Some(1), dir.path()).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["content"], "# Hello\n\nWorld.");
    }

    #[test]
    fn test_collect_md_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("a.md"), "test").unwrap();
        std::fs::write(sub.join("b.md"), "test").unwrap();
        std::fs::write(dir.path().join("c.txt"), "test").unwrap();

        let mut files = Vec::new();
        collect_md_files(dir.path(), &mut files);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.extension().unwrap() == "md"));
    }

    #[test]
    fn test_collect_md_files_empty_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut files = Vec::new();
        collect_md_files(dir.path(), &mut files);
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_md_files_nonexistent() {
        let mut files = Vec::new();
        collect_md_files(Path::new("/nonexistent/path"), &mut files);
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_content_files() {
        let dir = tempfile::TempDir::new().unwrap();
        // Create one CONTENT_DIR
        let notes_dir = dir.path().join("daily/notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        std::fs::write(notes_dir.join("test.md"), "# Test").unwrap();
        std::fs::write(notes_dir.join("ignore.txt"), "not md").unwrap();

        let files = collect_content_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].to_string_lossy().contains("test.md"));
    }

    #[test]
    fn test_collect_content_files_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let files = collect_content_files(dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn test_format_json_include_content_file_missing() {
        let results = vec![SearchResult {
            source_file: "nonexistent.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.5,
            status: None,
            related_docs: vec![],
        }];
        // File doesn't exist, so content field should not be present
        let json = format_json(&results, Some(1), Path::new("/tmp/empty")).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert!(parsed[0].get("content").is_none());
    }

    #[test]
    fn test_format_json_include_content_beyond_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.md"), "aaa").unwrap();
        std::fs::write(dir.path().join("b.md"), "bbb").unwrap();

        let results = vec![
            SearchResult {
                source_file: "a.md".to_string(),
                source_type: "note".to_string(),
                section_path: "A".to_string(),
                snippet: "aaa".to_string(),
                score: 0.5,
                status: None,
                related_docs: vec![],
            },
            SearchResult {
                source_file: "b.md".to_string(),
                source_type: "note".to_string(),
                section_path: "B".to_string(),
                snippet: "bbb".to_string(),
                score: 0.4,
                status: None,
                related_docs: vec![],
            },
        ];
        // include_content=1, so only first result gets content
        let json = format_json(&results, Some(1), dir.path()).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["content"], "aaa");
        assert!(parsed[1].get("content").is_none());
    }

    #[test]
    fn test_format_text_multiple_results() {
        let results = vec![
            SearchResult {
                source_file: "a.md".to_string(),
                source_type: "note".to_string(),
                section_path: "A".to_string(),
                snippet: "aaa".to_string(),
                score: 0.5,
                status: None,
                related_docs: vec![],
            },
            SearchResult {
                source_file: "b.md".to_string(),
                source_type: "research".to_string(),
                section_path: "B".to_string(),
                snippet: "bbb".to_string(),
                score: 0.3,
                status: Some("outdated".to_string()),
                related_docs: vec![],
            },
        ];
        let text = format_text(&results);
        assert!(text.contains("1. [note]"));
        assert!(text.contains("2. [research]"));
        assert!(text.contains("status: outdated"));
        assert!(!text.contains("No results found"));
    }

    #[test]
    fn test_doctor_no_db() {
        let report = doctor_check(Path::new("/nonexistent/knowledge.db"));
        let issues = report.issues();
        assert!(!issues.is_empty());
        assert!(issues[0].contains("does not exist"));
    }

    #[test]
    fn test_doctor_with_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();

        let report = doctor_check(&db_path);
        let ok = report.ok();
        // DB exists, so should have OK entries
        assert!(ok.iter().any(|s| s.contains("DB:")));
        assert!(ok.iter().any(|s| s.contains("Documents:")));
        assert!(ok.iter().any(|s| s.contains("Chunks:")));
    }

    #[test]
    fn test_doctor_vectors_zero_no_chunks() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();

        let report = doctor_check(&db_path);
        let ok = report.ok();
        // 0 chunks, 0 vectors — should be OK (matches)
        assert!(ok.iter().any(|s| s.contains("Vectors: 0")));
    }

    #[test]
    fn test_doctor_reports_dict_candidates() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();

        let conn = db::get_connection(&db_path).unwrap();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
            rusqlite::params![now, now],
        ).unwrap();
        drop(conn);

        let report = doctor_check(&db_path);
        let issues = report.issues();
        let ok = report.ok();
        // Should report ready candidates as an issue
        assert!(
            issues.iter().any(|s| s.contains("candidates ready")),
            "should report dict candidates: {:?}",
            issues
        );
        assert!(
            ok.iter().any(|s| s.contains("User dict")),
            "should show user dict summary: {:?}",
            ok
        );
    }

    #[test]
    fn test_ingest_session_file_not_found() {
        let result = cmd_ingest_session(Path::new("/nonexistent/session.jsonl"));
        assert!(result.is_err());
    }

    #[test]
    fn test_doctor_report_serde_roundtrip() {
        let report = DoctorReport {
            sections: vec![DoctorSection {
                name: "Database".to_string(),
                items: vec![
                    CheckItem {
                        status: CheckStatus::Ok,
                        message: "DB: /tmp/test.db (1.0 MB)".to_string(),
                        hint: None,
                    },
                    CheckItem {
                        status: CheckStatus::Warning,
                        message: "Vectors: 0 / 100 chunks".to_string(),
                        hint: Some("Run `vector-fill`.".to_string()),
                    },
                    CheckItem {
                        status: CheckStatus::Error,
                        message: "DB missing".to_string(),
                        hint: Some("Run `init`.".to_string()),
                    },
                ],
            }],
        };
        let json = serde_json::to_value(&report).unwrap();
        let decoded: DoctorReport = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.sections.len(), 1);
        assert_eq!(decoded.sections[0].items.len(), 3);
        assert_eq!(decoded.sections[0].items[0].status, CheckStatus::Ok);
        assert_eq!(decoded.sections[0].items[1].status, CheckStatus::Warning);
        assert_eq!(decoded.sections[0].items[2].status, CheckStatus::Error);
        assert!(decoded.sections[0].items[0].hint.is_none());
        assert!(decoded.sections[0].items[1].hint.is_some());
    }

    #[test]
    fn test_doctor_to_json_output_shape() {
        let report = DoctorReport {
            sections: vec![DoctorSection {
                name: "Test".to_string(),
                items: vec![CheckItem {
                    status: CheckStatus::Ok,
                    message: "All good".to_string(),
                    hint: None,
                }],
            }],
        };
        let json_str = report.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed["sections"].is_array());
        assert_eq!(parsed["issue_count"], 0);
        assert_eq!(parsed["sections"][0]["name"], "Test");
        assert_eq!(parsed["sections"][0]["items"][0]["status"], "ok");
    }

    #[test]
    fn test_discover_subdirs_excludes_hidden() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("visible")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::create_dir(dir.path().join("another")).unwrap();

        let dirs = super::discover_subdirs(dir.path());
        let names: Vec<&str> = dirs
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        assert!(names.contains(&"visible"));
        assert!(names.contains(&"another"));
        assert!(!names.contains(&".hidden"));
    }

    #[test]
    fn test_collect_md_files_excluding_hidden() {
        let dir = tempfile::TempDir::new().unwrap();
        let visible = dir.path().join("visible");
        std::fs::create_dir(&visible).unwrap();
        std::fs::write(visible.join("note.md"), "# Note").unwrap();

        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.md"), "# Secret").unwrap();

        let mut files = Vec::new();
        super::collect_md_files_excluding_hidden(dir.path(), &mut files);

        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        assert!(names.contains(&"note.md"));
        assert!(!names.contains(&"secret.md"));
    }
}
