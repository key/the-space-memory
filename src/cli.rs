use std::io::{BufRead, ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use crate::config;
use crate::db;
use crate::embedder;
use crate::indexer;
use crate::searcher;
use crate::synonyms;
use crate::user_dict;

/// Default `.tsmignore` shipped by `tsm init`. Patterns are
/// `.gitignore`-syntax and resolve relative to `project_root`.
///
/// Hidden directories (`.*/`) and common build/dependency directories are
/// excluded by default — the historical "fallback hidden-dir pattern"
/// behavior is now expressed here, where users can see and edit it.
const DEFAULT_TSMIGNORE: &str = "\
# tsm default ignore patterns. Edit to taste — `tsm init` will not
# overwrite an existing file.

# Hidden directories (.git/, .obsidian/, .venv/, etc.)
.*/

# Build / dependency directories
target/
node_modules/
.venv/
dist/

# Large binary artifacts that bloat the index
*.parquet
*.zip
*.db
";

/// Default `tsm.toml` shipped by `tsm init`. Built from the canonical
/// example template at the repo root so the scaffolded file always matches
/// the documented options surface.
const DEFAULT_TSM_TOML: &str = include_str!("../tsm.toml.example");

/// Default header for `synonyms.csv`. The body is empty — users add pairs
/// and `cmd_init` re-syncs idempotently.
const DEFAULT_SYNONYMS_CSV: &str = "\
# User-defined synonym pairs. One pair per line.
#
# Format: word_a,word_b
# Example:
#   ml,machine learning
#   nlp,自然言語処理
#
# Pairs imported from WordNet are stored separately and are not affected
# by edits to this file.
";

/// Default header for `custom_terms.toml`. The body is empty — users add
/// terms by appending entries.
const DEFAULT_CUSTOM_TERMS: &str = "\
# Custom terminology overrides for the lindera tokenizer / scorer.
#
# Add an entry per term:
#
#   [[terms]]
#   surface = \"自然言語処理\"
#   reading = \"シゼンゲンゴショリ\"
#   pos = \"名詞\"
#   weight = 1.5
";

/// Inputs for `cmd_init_with`. All paths are explicit so init can be
/// driven end-to-end from a tempdir in tests without touching the config
/// singleton.
pub struct InitPaths<'a> {
    pub db_path: &'a Path,
    pub project_root: &'a Path,
    pub state_dir: &'a Path,
    pub user_dict_path: &'a Path,
}

/// Convenience wrapper that builds `InitPaths` from the config singleton
/// and forwards to `cmd_init_with`. Tests bypass this by calling
/// `cmd_init_with` directly with a tempdir.
pub fn cmd_init() -> anyhow::Result<()> {
    let db_path = config::db_path();
    let project_root = config::project_root();
    let state_dir = config::state_dir();
    let user_dict_path = config::user_dict_path();
    cmd_init_with(&InitPaths {
        db_path: &db_path,
        project_root: &project_root,
        state_dir: &state_dir,
        user_dict_path: &user_dict_path,
    })
}

/// Initialize the workspace: schema, scaffold files, WordNet import, user
/// synonym import. All steps are idempotent — re-running `tsm init` after
/// `tsm setup` is the supported recovery path when WordNet is downloaded
/// after the initial init.
///
/// `project_root` and `state_dir` are passed explicitly (rather than
/// derived from `db_path`) so the function is testable without the config
/// singleton — the regression "someone removes the .tsmignore step from
/// cmd_init" is caught by the integration tests below.
pub fn cmd_init_with(paths: &InitPaths<'_>) -> anyhow::Result<()> {
    db::init_db(paths.db_path)?;
    println!("Database initialized at {}", paths.db_path.display());

    // Scaffold default files. Each call is idempotent — pre-existing
    // user-customized files are never overwritten.
    install_default_tsmignore(paths.project_root)?;
    install_default_file(
        &paths.project_root.join("tsm.toml"),
        DEFAULT_TSM_TOML,
        "tsm.toml",
    )?;
    install_default_file(paths.user_dict_path, "", "user_dict.simpledic")?;
    install_default_file(
        &paths.state_dir.join("custom_terms.toml"),
        DEFAULT_CUSTOM_TERMS,
        "custom_terms.toml",
    )?;
    install_default_file(
        &paths.state_dir.join("synonyms.csv"),
        DEFAULT_SYNONYMS_CSV,
        "synonyms.csv",
    )?;
    install_default_hooks(paths.state_dir)?;

    // WordNet import — graceful skip when the resource is missing so
    // offline `tsm init` and pre-`tsm setup` invocations both succeed.
    let wordnet_db = paths.state_dir.join("wnjpn.db");
    let synonyms_csv = paths.state_dir.join("synonyms.csv");
    let conn = db::get_connection(paths.db_path)?;
    if wordnet_db.is_file() {
        println!(
            "Importing WordNet synonyms from {}...",
            wordnet_db.display()
        );
        synonyms::import_wordnet(&conn, &wordnet_db, None)?;
    } else {
        log::warn!(
            "WordNet DB not found at {} — run `tsm setup` to download it, then re-run `tsm init`.",
            wordnet_db.display()
        );
    }

    // User-synonym import. The CSV always exists at this point because we
    // just scaffolded it, but we still gate on `is_file` to keep the
    // behavior obvious if a caller wires in a nonstandard path. Insert-only
    // (mirror = false): re-running init must never delete pairs added via
    // `tsm synonym add` that aren't in the file.
    if synonyms_csv.is_file() {
        let content = std::fs::read_to_string(&synonyms_csv)?;
        let result = synonyms::import_user_synonyms(&conn, &content, false)?;
        println!(
            "User synonyms imported: {} pairs ({} skipped)",
            result.total, result.skipped,
        );
    }

    Ok(())
}

/// Write `content` to `path` if no file exists there. Idempotent —
/// re-running `tsm init` never overwrites a user-customized file. The
/// `display_name` is used in log messages so callers can render
/// human-friendly names for paths whose `file_name()` is unhelpful (or to
/// keep log output consistent across path variations).
///
/// Uses `OpenOptions::create_new` rather than `exists()` + `write()` so
/// the no-overwrite guarantee holds atomically — if the file is created
/// by another process between the check and the write, `create_new`
/// returns `AlreadyExists` and we treat that as the skip case rather
/// than silently clobbering the file.
fn install_default_file(path: &Path, content: &str, display_name: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(content.as_bytes())?;
            println!("Wrote default {} to {}", display_name, path.display());
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            println!(
                "{} already exists at {} — leaving untouched",
                display_name,
                path.display()
            );
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn install_default_tsmignore(project_root: &Path) -> anyhow::Result<()> {
    install_default_file(
        &project_root.join(".tsmignore"),
        DEFAULT_TSMIGNORE,
        ".tsmignore",
    )
}

/// Scaffold editable copies of the two default Lua hooks into
/// `state_dir/hooks/{extract,score}/`. Existing files are never overwritten,
/// so user customizations survive repeated `tsm init` invocations.
fn install_default_hooks(state_dir: &Path) -> anyhow::Result<()> {
    install_default_file(
        &state_dir.join("hooks/extract/10-md_frontmatter.lua"),
        crate::lua_hooks::DEFAULT_EXTRACT_HOOK,
        "hooks/extract/10-md_frontmatter.lua",
    )?;
    install_default_file(
        &state_dir.join("hooks/score/10-default.lua"),
        crate::lua_hooks::DEFAULT_SCORE_HOOK,
        "hooks/score/10-default.lua",
    )
}

/// Run indexing on given file paths under a policy. Returns stats.
///
/// The policy is the non-bypassable correctness gate applied by the
/// indexer — no caller-side pre-filter is required (or permitted, per
/// design: duplicated filter logic was a bug-magnet, see #134).
pub fn run_index(
    conn: &rusqlite::Connection,
    file_paths: &[PathBuf],
    project_root: &Path,
    policy: &dyn indexer::IngestPolicy,
) -> anyhow::Result<indexer::IndexStats> {
    indexer::index_all(conn, file_paths, project_root, policy)
}

pub fn cmd_index(files_from_stdin: bool) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;
    let project_root = config::project_root();

    let walker = indexer::ContentWalker::from_env();
    let file_paths: Vec<PathBuf> = if files_from_stdin {
        read_paths_from_stdin(&project_root)
    } else {
        walker.collect_files()
    };

    let stats = run_index(&conn, &file_paths, &project_root, &walker)?;
    println!(
        "Indexed: {}, Skipped: {}, Removed: {}",
        stats.indexed, stats.skipped, stats.removed
    );
    Ok(())
}

/// Read one-path-per-line from stdin and resolve each against `project_root`.
/// No ignore/extension filtering is performed here — the indexer applies
/// the policy gate itself, so duplicating the check here would add code
/// surface with no behavioral benefit.
pub fn read_paths_from_stdin(project_root: &Path) -> Vec<PathBuf> {
    // `filter_map` (not `map_while`) so a transient stdin I/O error on one
    // line is logged and skipped rather than silently truncating the rest of
    // the input — matters when a post-save hook pipes many paths.
    std::io::stdin()
        .lock()
        .lines()
        .filter_map(|res| match res {
            Ok(line) => Some(line),
            Err(e) => {
                log::warn!("stdin read error (line skipped): {e}");
                None
            }
        })
        .filter(|line| !line.trim().is_empty())
        .map(|line| project_root.join(line.trim()))
        .collect()
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
    pub paths: Option<&'a [String]>,
}

/// Run search and return structured results (no DB open, no output).
pub fn run_search(
    conn: &rusqlite::Connection,
    opts: &SearchOptions,
) -> anyhow::Result<searcher::SearchOutput> {
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
        opts.paths,
    )
}

pub fn cmd_search(opts: SearchOptions) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;

    let output = run_search(&conn, &opts)?;
    match opts.format {
        "json" => print_json(&output.results, output.total_hits, opts.include_content)?,
        _ => print_text(&output.results, output.total_hits),
    }
    Ok(())
}

fn print_text(results: &[searcher::SearchResult], total_hits: usize) {
    print!("{}", searcher::format_text(results, total_hits));
}

fn print_json(
    results: &[searcher::SearchResult],
    total_hits: usize,
    include_content: Option<usize>,
) -> anyhow::Result<()> {
    let project_root = config::project_root();
    println!(
        "{}",
        searcher::format_json(results, total_hits, include_content, &project_root)?
    );
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
        println!("Session indexed: {name}");
    } else {
        println!("Session unchanged: {name}");
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
        // The backfill ran daemon-side; its summary travels back in the
        // payload so we can show it here rather than losing it to the log.
        // Fall back to a generic line if an older daemon omits the field, so
        // a successful run is never silent.
        let summary = resp
            .payload
            .as_ref()
            .and_then(|p| p.get("summary"))
            .and_then(|v| v.as_str())
            .unwrap_or("Vector fill complete.");
        println!("{summary}");
        return Ok(());
    }
    // tsmd not running — run directly
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;
    println!("{}", run_vector_fill(&conn, batch_size)?);
    Ok(())
}

/// Run vector backfill via the embedder socket (daemon-safe).
///
/// Returns the user-facing summary line instead of printing it: when invoked
/// daemon-side the worker's stdout goes to the daemon log, so the caller (the
/// CLI directly, or via the IPC response payload) surfaces it to the user.
pub fn run_vector_fill(conn: &rusqlite::Connection, batch_size: usize) -> anyhow::Result<String> {
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

    let skipped: i64 = conn.query_row("SELECT COUNT(*) FROM chunks_vec_skip", [], |r| r.get(0))?;
    let summary = vector_fill_summary(stats.filled, skipped);
    if stats.errors > 0 {
        log::warn!("{} errors during backfill.", stats.errors);
    }

    // Clear backfill status on completion
    status::update(&state_dir, |s| {
        s.backfill = None;
    });

    Ok(summary)
}

/// Build the user-facing summary line for `vector-fill`.
///
/// Distinguishes "nothing to do" from "chunks are stuck on the skip list",
/// so a `0 filled` result with skip-marked chunks is not silently reported as
/// "No missing vectors." (skip-marked chunks are not retried by `vector-fill`;
/// `tsm reindex vectors` clears the skip list and retries them).
fn vector_fill_summary(filled: usize, skipped: i64) -> String {
    if filled == 0 && skipped == 0 {
        return "No missing vectors.".to_string();
    }
    let mut parts = Vec::new();
    if filled > 0 {
        parts.push(format!("Backfilled {filled} vectors."));
    }
    // Always surface the skip list when present, even alongside a successful
    // fill — otherwise a partial run silently leaves chunks stuck.
    if skipped > 0 {
        parts.push(format!(
            "{skipped} chunk(s) on the skip list after repeated embed failures. \
             Run `tsm reindex vectors` to clear the skip list and retry."
        ));
    }
    parts.join(" ")
}

pub fn cmd_import_wordnet(wordnet_db: &Path) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;

    let progress = |imported: usize, total: usize| {
        if imported.is_multiple_of(10000) || imported == total {
            eprint!("\r  {imported}/{total}");
        }
    };
    let count = crate::synonyms::import_wordnet(&conn, wordnet_db, Some(&progress))?;
    eprint!("\r                              \r"); // clear progress line
    println!("Imported {count} synonym pairs from WordNet.");

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
        .unwrap_or(0);
    println!("Total synonyms: {total}");
    Ok(())
}

pub fn cmd_synonym_add(a: &str, b: &str) -> anyhow::Result<()> {
    let conn = db::get_connection(&config::db_path())?;
    // Echo the normalized pair as actually stored, not the raw input.
    let (lo, hi) = crate::synonyms::add_user_synonym(&conn, a, b)?;
    println!("Added synonym: {lo} <-> {hi}");
    Ok(())
}

pub fn cmd_synonym_rm(a: &str, b: Option<&str>) -> anyhow::Result<()> {
    let conn = db::get_connection(&config::db_path())?;
    let result = crate::synonyms::remove_user_synonym(&conn, a, b)?;
    if result.removed > 0 {
        println!("Removed {} synonym pair(s).", result.removed);
    } else if result.skipped_non_user > 0 {
        println!(
            "No user synonym removed. {} matching pair(s) are not user-defined \
             (e.g. WordNet) and were left intact.",
            result.skipped_non_user
        );
    } else {
        println!("No matching user synonym found.");
    }
    Ok(())
}

pub fn cmd_synonym_export(file: Option<&Path>) -> anyhow::Result<()> {
    let conn = db::get_connection(&config::db_path())?;
    match file {
        Some(path) => {
            // Write to a temp sibling then rename, so a mid-write failure cannot
            // truncate an existing file (matches `download_wordnet`).
            let tmp = path.with_extension("csv.tmp");
            let mut f = std::fs::File::create(&tmp)?;
            let count = synonyms::export_user_synonyms(&conn, &mut f)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            // File destination: CSV went to the file, so the count is user
            // output on stdout.
            println!(
                "Exported {count} user synonym pair(s) to {}",
                path.display()
            );
        }
        None => {
            // stdout destination: CSV is the user output, so the diagnostic
            // count goes to stderr to keep the stream pipeable (ADR-0012).
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            let count = synonyms::export_user_synonyms(&conn, &mut w)?;
            eprintln!("Exported {count} user synonym pair(s).");
        }
    }
    Ok(())
}

pub fn cmd_synonym_import(file: Option<&Path>) -> anyhow::Result<()> {
    let content = match file {
        Some(path) => {
            if !path.is_file() {
                anyhow::bail!("synonyms CSV not found: {}", path.display());
            }
            std::fs::read_to_string(path)?
        }
        None => {
            // Don't block on an interactive terminal waiting for input that isn't
            // coming. (The empty-input mass-delete is guarded separately in
            // `import_user_synonyms`.) Require a pipe/redirect or an explicit --file.
            if std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "no input: pipe CSV into `tsm synonym import` or pass `--file <PATH>`"
                );
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    let conn = db::get_connection(&config::db_path())?;
    // `synonym import` mirrors: pairs absent from the input are deleted.
    let result = synonyms::import_user_synonyms(&conn, &content, true)?;
    println!(
        "User synonyms imported: {} pairs ({} deleted, {} skipped)",
        result.total, result.deleted, result.skipped,
    );
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
    println!("Model files downloaded:");
    println!("  config:    {}", config_path.display());
    println!("  tokenizer: {}", tokenizer_path.display());
    println!("  weights:   {}", weights_path.display());

    // Copy to models_dir for local access
    let dest = config::models_dir();
    std::fs::create_dir_all(&dest)?;
    let sources = [
        (&config_path, "config.json"),
        (&tokenizer_path, "tokenizer.json"),
        (&weights_path, "model.safetensors"),
    ];
    let mut copied: Vec<std::path::PathBuf> = Vec::new();
    let copy_result = (|| -> anyhow::Result<()> {
        for (src, name) in &sources {
            let dst = dest.join(name);
            std::fs::copy(src, &dst)?;
            copied.push(dst.clone());
            println!("  copied: {}", dst.display());
        }
        Ok(())
    })();
    if let Err(e) = copy_result {
        log::warn!("Setup failed; cleaning up partial files");
        for f in &copied {
            let _ = std::fs::remove_file(f);
        }
        return Err(e);
    }
    println!("Model files installed to {}", dest.display());

    // Download Japanese WordNet DB. Importing the synonyms into the
    // workspace DB is `tsm init`'s job: `tsm setup` is the pure
    // resource-fetch layer, with no workspace DB writes.
    setup_wordnet()?;

    println!("Setup complete. Run `tsm init` in your workspace to finish.");

    Ok(())
}

fn setup_wordnet() -> anyhow::Result<()> {
    let wordnet_dest = config::wordnet_db_path();
    if wordnet_dest.is_file() {
        println!("WordNet DB already exists at {}", wordnet_dest.display());
    } else {
        download_wordnet(&wordnet_dest)?;
    }
    Ok(())
}

fn download_wordnet(dest: &Path) -> anyhow::Result<()> {
    const WORDNET_URL: &str = "https://github.com/bond-lab/wnja/releases/download/v1.1/wnjpn.db.gz";
    println!("Downloading WordNet DB from {WORDNET_URL}...");
    let resp = ureq::get(WORDNET_URL).call()?;
    let mut gz_data = Vec::new();
    resp.into_body().as_reader().read_to_end(&mut gz_data)?;
    let mut decoder = flate2::read::GzDecoder::new(&gz_data[..]);
    let parent = dest.parent().expect("dest has parent");
    std::fs::create_dir_all(parent)?;
    let tmp_path = dest.with_extension("db.tmp");
    let mut out = std::fs::File::create(&tmp_path)?;
    std::io::copy(&mut decoder, &mut out)?;
    std::fs::rename(&tmp_path, dest)?;
    println!("WordNet DB installed to {}", dest.display());
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

/// Build the "Build" doctor section from compile-time version metadata.
///
/// `TSM_GIT_DESCRIBE` / `TSM_BUILD_DATE` are injected by `build.rs`
/// (`git describe` + build date). When unavailable (e.g. a build without the
/// build script or git), fall back to the crate version and "unknown".
pub fn build_section() -> DoctorSection {
    let version = match option_env!("TSM_GIT_DESCRIBE") {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => format!("v{}", env!("CARGO_PKG_VERSION")),
    };
    let built = match option_env!("TSM_BUILD_DATE") {
        Some(d) if !d.is_empty() => d,
        _ => "unknown",
    };
    DoctorSection {
        name: "Build".to_string(),
        items: vec![
            CheckItem {
                status: CheckStatus::Ok,
                message: format!("Version: {version}"),
                hint: None,
            },
            CheckItem {
                status: CheckStatus::Ok,
                message: format!("Built: {built}"),
                hint: None,
            },
        ],
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
    // Build metadata first (matches the local `cmd_doctor` path).
    report.sections.insert(0, build_section());
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

    // Check local model files
    let models_dir = config::models_dir();
    let model_files = config::MODEL_FILES;
    let present: Vec<&str> = model_files
        .iter()
        .filter(|f| models_dir.join(f).is_file())
        .copied()
        .collect();
    if present.len() == model_files.len() {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Ok,
            message: format!("Model: {}", models_dir.display()),
            hint: None,
        });
    } else if present.is_empty() {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!("Model: not found in {}", models_dir.display()),
            hint: Some("Run `tsm setup` to download and install model files.".to_string()),
        });
    } else {
        let missing: Vec<&str> = model_files
            .iter()
            .filter(|f| !present.contains(f))
            .copied()
            .collect();
        emb_section.items.push(CheckItem {
            status: CheckStatus::Error,
            message: format!("Model: incomplete (missing: {})", missing.join(", ")),
            hint: Some("Run `tsm setup` to re-download model files.".to_string()),
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

    // Chunks on the skip list never get vectors and are not retried by
    // `vector-fill`, so they are not part of the fillable target. Comparing
    // `vecs` against the full chunk count would otherwise advise `vector-fill`
    // for a gap only `reindex vectors` can close. Log (not swallow) a genuine
    // query error rather than silently treating it as "0 skipped".
    let skipped: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks_vec_skip", [], |r| r.get(0))
        .unwrap_or_else(|e| {
            log::warn!("doctor: could not query chunks_vec_skip: {e}");
            0
        });
    let fillable = (chunks - skipped).max(0);

    if vecs < 0 {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Error,
            message: "Vectors: chunks_vec unreadable".to_string(),
            hint: None,
        });
    } else if vecs == 0 && fillable > 0 {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!("Vectors: 0 / {fillable} chunks"),
            hint: Some(
                backfill_hint.unwrap_or_else(|| {
                    "Run `vector-fill` (needs embedder) or `rebuild`.".to_string()
                }),
            ),
        });
    } else if vecs < fillable {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!("Vectors: {vecs} / {fillable} chunks (mismatch)"),
            hint: Some(
                backfill_hint.unwrap_or_else(|| {
                    "Run `vector-fill` (needs embedder) or `rebuild`.".to_string()
                }),
            ),
        });
    } else {
        let detail = if skipped > 0 {
            "all fillable chunks have vectors"
        } else {
            "matches all chunks"
        };
        emb_section.items.push(CheckItem {
            status: CheckStatus::Ok,
            message: format!("Vectors: {vecs} ({detail})"),
            hint: None,
        });
    }

    // Surface skip-listed chunks explicitly with their own recovery hint,
    // otherwise they read as a permanent unexplained vector mismatch.
    if skipped > 0 {
        emb_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!("Vectors: {skipped} chunk(s) on the skip list after embed failures"),
            hint: Some("Run `tsm reindex vectors` to clear the skip list and retry.".to_string()),
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

    // ── Reindex section ──
    if let Some(ref ri) = sf.reindex {
        let pct = if ri.total > 0 {
            (ri.processed as f64 / ri.total as f64 * 100.0) as u32
        } else {
            0
        };
        let processed = (ri.processed + ri.errors) as usize;
        let eta = if processed > 0 && ri.total > 0 {
            estimate_eta(&ri.started_at, processed, ri.total as usize)
        } else {
            "calculating...".to_string()
        };
        let mut reindex_section = DoctorSection {
            name: "Reindex".to_string(),
            items: Vec::new(),
        };
        reindex_section.items.push(CheckItem {
            status: CheckStatus::Warning,
            message: format!(
                "Reindex ({}) in progress: {}/{} ({pct}%), ETA {eta}",
                ri.kind, ri.processed, ri.total
            ),
            hint: None,
        });
        report.sections.push(reindex_section);
    }
}

pub fn cmd_doctor(format: &str) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let mut report = doctor_check(&db_path);
    // Surface build metadata first so it is visible even when the DB is absent.
    report.sections.insert(0, build_section());
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

/// `tsm dict update` — show frequent, un-judged candidate words (ADR-0014).
/// A read-only discovery view: it lists words seen often enough to be worth a
/// verdict. Acceptance/rejection is per word via `dict add` / `dict reject`;
/// bulk loading is `dict import`. This command only reads.
pub fn cmd_dict_update(threshold: i64) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let conn = db::get_connection(&db_path)?;

    let candidates = user_dict::get_threshold_candidates(&conn, threshold);
    if candidates.is_empty() {
        println!("No candidates meet the threshold (freq >= {threshold}).");
        return Ok(());
    }

    // Interactive TUI output — bypass log system for clean display
    eprintln!("=== Dictionary Update Candidates ===\n");
    for c in &candidates {
        print_candidate(c);
    }
    eprintln!(
        "\n{} candidate(s). Accept with `tsm dict add <word> [reading]` \
         or reject with `tsm dict reject <word>`.",
        candidates.len()
    );
    Ok(())
}

/// Rebuild the FTS index so the tokenizer picks up a user-dictionary change.
/// If the daemon is running, send a reindex over IPC (the daemon resets its own
/// segmenter); otherwise reset the local segmenter and rebuild directly.
fn reindex_fts_after_dict_change() -> anyhow::Result<()> {
    let daemon_socket = config::daemon_socket_path();
    let reindex_req = crate::daemon_protocol::DaemonRequest::Reindex {
        kind: crate::daemon_protocol::ReindexKind::Fts,
    };
    let daemon_accepted = crate::daemon_protocol::try_send_request(&daemon_socket, &reindex_req)
        .and_then(|r| r.ok())
        .is_some_and(|r| r.ok);

    if daemon_accepted {
        println!("\nFTS reindex started in background. Run `tsm doctor` to check progress.");
    } else {
        crate::tokenizer::reset_segmenter();
        println!("\nRebuilding FTS index...");
        cmd_rebuild_fts()?;
    }

    Ok(())
}

/// Apply a verdict change to the user dictionary: when the accepted set changed,
/// regenerate `user_dict.simpledic` from the DB and rebuild FTS (ADR-0014).
/// Takes `conn` by value so it is dropped before the local FTS rebuild opens its
/// own writer. A change that does not touch the accepted set is a no-op.
fn apply_verdict_change(
    conn: rusqlite::Connection,
    t: &user_dict::Transition,
) -> anyhow::Result<()> {
    if !t.affected_dict {
        return Ok(());
    }
    let csv_path = config::user_dict_path();
    let n = user_dict::regenerate_user_dict(&conn, &csv_path)?;
    println!(
        "Regenerated {} ({n} accepted term{}).",
        csv_path.display(),
        if n == 1 { "" } else { "s" }
    );
    drop(conn);
    reindex_fts_after_dict_change()
}

/// `tsm dict add <surface> [<yomi>]` — accept a term (ADR-0014 §1, §4).
pub fn cmd_dict_add(surface: &str, yomi: Option<&str>) -> anyhow::Result<()> {
    user_dict::validate_surface(surface)?;
    let conn = db::get_connection(&config::db_path())?;
    let (reading, warned) = user_dict::resolve_reading(surface, yomi);
    if warned {
        eprintln!(
            "warning: no reading for '{surface}'; storing the surface as a substitute. \
             Provide one with `tsm dict add {surface} <yomi>`."
        );
    }
    let t = user_dict::set_verdict(&conn, surface, user_dict::Verdict::Accepted, Some(&reading))?;
    match t.from {
        Some(user_dict::Verdict::Accepted) => println!("'{surface}' is already accepted."),
        _ => println!("Accepted '{surface}'."),
    }
    apply_verdict_change(conn, &t)
}

/// `tsm dict rm <word>` — reset a term to pending (ADR-0014 §1).
/// Errors when the term was never registered (nothing to reset).
pub fn cmd_dict_rm(word: &str) -> anyhow::Result<()> {
    let conn = db::get_connection(&config::db_path())?;
    let t = user_dict::set_verdict(&conn, word, user_dict::Verdict::Pending, None)?;
    match t.from {
        Some(user_dict::Verdict::Pending) => println!("'{word}' is already pending."),
        _ => println!("Reset '{word}' to pending."),
    }
    apply_verdict_change(conn, &t)
}

/// `tsm dict reject <word>` — move a word to the `rejected` verdict (ADR-0014).
/// Inserts a manual row when the word was never seen (preemptive reject). When
/// the word was accepted, the accepted set shrinks, so `apply_verdict_change`
/// regenerates `user_dict.simpledic` and reloads the tokenizer.
pub fn cmd_dict_reject(word: &str) -> anyhow::Result<()> {
    user_dict::validate_surface(word)?;
    let conn = db::get_connection(&config::db_path())?;
    let t = user_dict::set_verdict(&conn, word, user_dict::Verdict::Rejected, None)?;
    match t.from {
        None => println!("Rejected '{word}' (new entry)."),
        Some(user_dict::Verdict::Rejected) => println!("'{word}' is already rejected."),
        Some(user_dict::Verdict::Accepted) => {
            println!("Rejected '{word}' (was accepted; dictionary regenerated).")
        }
        Some(user_dict::Verdict::Pending) => println!("Rejected '{word}'."),
    }
    apply_verdict_change(conn, &t)
}

/// `tsm dict export` — write the DB's verdicts to disk (ADR-0014 §2): the
/// accepted set to `user_dict.simpledic` and the rejected set to
/// `reject_words.txt`. The DB is the authority; this materializes a portable,
/// git-trackable snapshot. No reindex — the running tokenizer already reflects
/// the DB. Diagnostics go to stderr so stdout stays a clean status line.
pub fn cmd_dict_export() -> anyhow::Result<()> {
    let conn = db::get_connection(&config::db_path())?;

    let dict_path = config::user_dict_path();
    let accepted = user_dict::regenerate_user_dict(&conn, &dict_path)?;
    eprintln!(
        "Wrote {accepted} accepted word(s) to {}",
        dict_path.display()
    );

    let reject_path = config::reject_words_path();
    let rejected = user_dict::export_reject_words_to_file(&conn, &reject_path)?;
    eprintln!(
        "Wrote {rejected} rejected word(s) to {}",
        reject_path.display()
    );

    println!("Exported {accepted} accepted, {rejected} rejected word(s).");
    Ok(())
}

/// `tsm dict import` — load verdicts from disk into the DB (ADR-0014 §2).
/// Insert-only: accepted words from `user_dict.simpledic` and rejected words
/// from `reject_words.txt` are upserted; verdicts absent from the files are left
/// untouched (use `dict rm`/`reject` to remove). This recovers the DB after a
/// rebuild and reflects hand-edits or another machine's files. When the imported
/// verdicts changed the accepted set (a new accepted word, or a reject demoting a
/// previously-accepted one), `user_dict.simpledic` is regenerated from the final
/// DB state and the tokenizer reloaded — once. Counts go to stderr.
pub fn cmd_dict_import() -> anyhow::Result<()> {
    let conn = db::get_connection(&config::db_path())?;

    let dict_path = config::user_dict_path();
    let acc = user_dict::import_user_dict_from_file(&conn, &dict_path)?;
    eprintln!(
        "Imported {} accepted word(s) from {}",
        acc.imported,
        dict_path.display()
    );

    let reject_path = config::reject_words_path();
    let rej = user_dict::import_reject_words_from_file(&conn, &reject_path)?;
    eprintln!(
        "Imported {} rejected word(s) from {}",
        rej.imported,
        reject_path.display()
    );

    println!(
        "Imported {} accepted, {} rejected word(s).",
        acc.imported, rej.imported
    );

    // Regenerate from the final DB state (not the input files) so an overlap
    // between the two files, or a reject that demoted an accepted word, leaves
    // simpledic consistent with the DB before the tokenizer reloads.
    if acc.dict_affected + rej.dict_affected > 0 {
        let n = user_dict::regenerate_user_dict(&conn, &dict_path)?;
        eprintln!(
            "Regenerated {} ({n} accepted term(s)).",
            dict_path.display()
        );
        drop(conn);
        reindex_fts_after_dict_change()?;
    }
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

pub fn cmd_rebuild(apply: bool) -> anyhow::Result<()> {
    let db_path = config::db_path();
    let project_root = config::project_root();
    let socket = config::embedder_socket_path();

    if !apply {
        // Dry run: show what would happen
        if db_path.exists() {
            if let Ok(meta) = std::fs::metadata(&db_path) {
                let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
                eprintln!("DB: {} ({size_mb:.1} MB)", db_path.display());
            }
            let conn = db::get_connection(&db_path)?;
            let chunks: i64 = conn
                .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
                .unwrap_or(0);
            let vecs: i64 = conn
                .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
                .unwrap_or(0);
            let docs: i64 = conn
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap_or(0);
            eprintln!("Documents: {docs}, Chunks: {chunks}, Vectors: {vecs}");
        } else {
            eprintln!("DB does not exist yet.");
        }
        eprintln!("\nThis will delete the DB and rebuild from scratch.");
        eprintln!("Note: dictionary verdicts (dictionary_candidates) will be lost.");
        eprintln!(
            "      The accepted set and reject list are gone until restored from \
             reject_words.txt (`tsm dict reject --apply`) / `tsm dict import` (once available); \
             a `dict add`/`rm` before then regenerates user_dict.simpledic from the empty DB."
        );
        eprintln!("Run with --apply to proceed.");
        return Ok(());
    }

    if !socket.exists() {
        log::warn!("Embedder is not running. Rebuilding without vectors.");
    } else {
        println!("Embedder: running");
    }

    // Backup
    if db_path.exists() {
        let backup = db_path.with_extension("db.bak");
        std::fs::copy(&db_path, &backup)?;
        println!("Backup: {}", backup.display());
        std::fs::remove_file(&db_path)?;
        println!("Deleted: {}", db_path.display());
    }

    // Init
    db::init_db(&db_path)?;
    println!("DB initialized");

    // Full index (synchronous, with progress)
    let conn = db::get_connection(&db_path)?;
    let walker = indexer::ContentWalker::from_env_with_project_root(&project_root);
    let file_paths = walker.collect_files();
    let total = file_paths.len();
    println!("Indexing {total} files...");

    let progress = |current: usize, total: usize, path: &Path| {
        let rel = path.strip_prefix(&project_root).unwrap_or(path).display();
        log::debug!("  [{current}/{total}] {rel}");
    };
    let stats = indexer::index_all_with_progress(
        &conn,
        &file_paths,
        &project_root,
        &walker,
        Some(&progress),
    )?;
    println!(
        "Done: Indexed: {}, Skipped: {}, Removed: {}",
        stats.indexed, stats.skipped, stats.removed
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
        println!("Vectors: {vecs} (matches all chunks)");
    } else if socket.exists() && chunks > 0 {
        let current_status = crate::status::read(&config::state_dir());
        if current_status.backfill.is_some() {
            println!("Vectors: {vecs} / {chunks} — backfill already in progress");
        } else {
            println!("Vectors: {vecs} / {chunks} — starting backfill in background...");
            spawn_background_backfill();
        }
        println!("Run `tsm doctor` to check progress.");
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
    println!("Rebuilding FTS index...");

    let progress = |current: usize, total: usize| {
        log::debug!("  [{current}/{total}]");
    };

    let inserted = indexer::rebuild_fts(&conn, Some(&progress))?;
    println!("FTS rebuild complete: {inserted} chunks re-indexed.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_section_reports_version_and_date() {
        let section = build_section();
        assert_eq!(section.name, "Build");
        assert_eq!(section.items.len(), 2);
        // All build items are informational (never warnings/errors).
        assert!(section.items.iter().all(|i| i.status == CheckStatus::Ok));
        // Version line is always populated: a git describe at build time, or
        // the crate version as a fallback when git info is unavailable.
        let version = &section.items[0].message;
        assert!(version.starts_with("Version: "), "got {version:?}");
        assert!(
            version.len() > "Version: ".len(),
            "version must not be empty"
        );
        // Build date line is always present (real date or "unknown").
        assert!(section.items[1].message.starts_with("Built: "));
    }

    #[test]
    fn test_run_doctor_includes_build_section_first() {
        // The daemon path renders `run_doctor`'s report; it must carry the
        // same build metadata as the local `cmd_doctor` path, listed first.
        let conn = crate::test_utils::setup_db();
        let report = run_doctor(&conn, Path::new(":memory:"));
        assert_eq!(
            report.sections.first().map(|s| s.name.as_str()),
            Some("Build")
        );
    }

    // Walker behavior is covered by indexer::walker::tests. In this module
    // `cmd_rebuild` constructs ContentWalker via from_env_with_project_root
    // (explicit project_root override); other commands use from_env().

    #[test]
    fn install_default_tsmignore_writes_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        super::install_default_tsmignore(dir.path()).unwrap();
        let written = std::fs::read_to_string(dir.path().join(".tsmignore")).unwrap();
        // Spot-check a few patterns from the shipped template — full byte
        // match would be brittle.
        assert!(written.contains(".*/"));
        assert!(written.contains("target/"));
        assert!(written.contains("*.parquet"));
    }

    /// Minimal WordNet-schema fixture for the cli test path. Mirrors the
    /// schema synonyms.rs depends on (word/sense/synset). Tiny pair set so
    /// success is observable in `synonyms` table counts.
    fn create_mock_wordnet_db(path: &Path, pairs: &[(&str, &str)]) {
        let wn = rusqlite::Connection::open(path).unwrap();
        wn.execute_batch(
            "CREATE TABLE word (wordid INTEGER PRIMARY KEY, lemma TEXT, lang TEXT);
             CREATE TABLE synset (synset TEXT PRIMARY KEY);
             CREATE TABLE sense (synset TEXT, wordid INTEGER);",
        )
        .unwrap();
        let mut word_id = 1i64;
        for (i, (a, b)) in pairs.iter().enumerate() {
            let sid = format!("syn{i:04}");
            wn.execute(
                "INSERT INTO synset (synset) VALUES (?)",
                rusqlite::params![sid],
            )
            .unwrap();
            wn.execute(
                "INSERT INTO word (wordid, lemma, lang) VALUES (?, ?, 'jpn')",
                rusqlite::params![word_id, a],
            )
            .unwrap();
            wn.execute(
                "INSERT INTO sense (synset, wordid) VALUES (?, ?)",
                rusqlite::params![sid, word_id],
            )
            .unwrap();
            word_id += 1;
            wn.execute(
                "INSERT INTO word (wordid, lemma, lang) VALUES (?, ?, 'jpn')",
                rusqlite::params![word_id, b],
            )
            .unwrap();
            wn.execute(
                "INSERT INTO sense (synset, wordid) VALUES (?, ?)",
                rusqlite::params![sid, word_id],
            )
            .unwrap();
            word_id += 1;
        }
    }

    /// Drive `cmd_init_with` against a fresh tempdir using the default
    /// state_dir layout. Returns (project_root, state_dir) for
    /// post-condition assertions.
    fn run_init(dir: &Path) -> (PathBuf, PathBuf) {
        let state_dir = dir.join(".tsm");
        let db_path = state_dir.join("tsm.db");
        let user_dict_path = state_dir.join("user_dict.simpledic");
        super::cmd_init_with(&super::InitPaths {
            db_path: &db_path,
            project_root: dir,
            state_dir: &state_dir,
            user_dict_path: &user_dict_path,
        })
        .unwrap();
        (dir.to_path_buf(), state_dir)
    }

    #[test]
    fn cmd_init_with_creates_db_and_all_scaffold_files() {
        // End-to-end regression test: cmd_init_with must produce the DB
        // file plus every scaffold file. Without this test, dropping
        // any one of the install_default_* calls would pass unit tests
        // — silently shipping a partial init.
        let dir = tempfile::TempDir::new().unwrap();
        let (project_root, state_dir) = run_init(dir.path());

        assert!(state_dir.join("tsm.db").is_file(), "DB was not created");
        assert!(
            project_root.join(".tsmignore").is_file(),
            ".tsmignore was not created"
        );
        assert!(
            project_root.join("tsm.toml").is_file(),
            "tsm.toml was not created"
        );
        assert!(
            state_dir.join("user_dict.simpledic").is_file(),
            "user_dict.simpledic was not created"
        );
        assert!(
            state_dir.join("custom_terms.toml").is_file(),
            "custom_terms.toml was not created"
        );
        assert!(
            state_dir.join("synonyms.csv").is_file(),
            "synonyms.csv was not created"
        );
        assert!(
            state_dir
                .join("hooks/extract/10-md_frontmatter.lua")
                .is_file(),
            "hooks/extract/10-md_frontmatter.lua was not created"
        );
        assert!(
            state_dir.join("hooks/score/10-default.lua").is_file(),
            "hooks/score/10-default.lua was not created"
        );
    }

    #[test]
    fn cmd_init_with_does_not_overwrite_existing_scaffold_files() {
        // Idempotency: pre-existing user customizations must survive a
        // second `tsm init`. Asserts every scaffold file individually
        // because adding a new one without no-overwrite is a footgun.
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = dir.path().join(".tsm");
        std::fs::create_dir_all(&state_dir).unwrap();

        let hook_extract_dir = state_dir.join("hooks/extract");
        let hook_score_dir = state_dir.join("hooks/score");
        std::fs::create_dir_all(&hook_extract_dir).unwrap();
        std::fs::create_dir_all(&hook_score_dir).unwrap();

        let user_files = [
            (dir.path().join(".tsmignore"), "user_pattern/\n"),
            (dir.path().join("tsm.toml"), "# user config\n"),
            (state_dir.join("user_dict.simpledic"), "user word\n"),
            (state_dir.join("custom_terms.toml"), "# user terms\n"),
            (state_dir.join("synonyms.csv"), "# user synonyms\n"),
            (
                hook_extract_dir.join("10-md_frontmatter.lua"),
                "-- user extract hook\n",
            ),
            (
                hook_score_dir.join("10-default.lua"),
                "-- user score hook\n",
            ),
        ];
        for (path, content) in &user_files {
            std::fs::write(path, content).unwrap();
        }

        run_init(dir.path());

        for (path, content) in &user_files {
            let after = std::fs::read_to_string(path).unwrap();
            assert_eq!(
                &after,
                content,
                "{} was overwritten by cmd_init_with",
                path.display()
            );
        }
    }

    #[test]
    fn cmd_init_with_handles_missing_wordnet_gracefully() {
        // No wnjpn.db in state_dir — init must succeed (warn-and-continue),
        // not error. This is the new-user path where `tsm setup` hasn't
        // run yet.
        let dir = tempfile::TempDir::new().unwrap();
        let (_, state_dir) = run_init(dir.path());

        let conn = crate::db::get_connection(&state_dir.join("tsm.db")).unwrap();
        let synonym_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(synonym_rows, 0, "no synonyms should be imported");
    }

    #[test]
    fn cmd_init_with_imports_wordnet_when_present() {
        // WordNet DB pre-staged in state_dir → init imports synonyms.
        // Covers the post-`tsm setup` flow where the user re-runs `init`.
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = dir.path().join(".tsm");
        std::fs::create_dir_all(&state_dir).unwrap();
        create_mock_wordnet_db(
            &state_dir.join("wnjpn.db"),
            &[("alpha", "beta"), ("gamma", "delta")],
        );

        run_init(dir.path());

        let conn = crate::db::get_connection(&state_dir.join("tsm.db")).unwrap();
        let synonym_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE source = 'wordnet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(synonym_rows, 2, "wordnet pairs should be imported");
    }

    #[test]
    fn cmd_init_with_is_idempotent() {
        // Running `init` twice — second invocation must succeed and leave
        // DB / synonyms in the same state. Asserts the INSERT-OR-IGNORE /
        // diff-sync semantics required for safe re-runs.
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = dir.path().join(".tsm");
        std::fs::create_dir_all(&state_dir).unwrap();
        create_mock_wordnet_db(&state_dir.join("wnjpn.db"), &[("foo", "bar")]);

        run_init(dir.path());
        run_init(dir.path()); // second run — must not duplicate or fail

        let conn = crate::db::get_connection(&state_dir.join("tsm.db")).unwrap();
        let synonym_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(synonym_rows, 1, "second init should not duplicate rows");
    }

    #[test]
    fn install_default_tsmignore_does_not_overwrite_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        let tsmignore = dir.path().join(".tsmignore");
        std::fs::write(&tsmignore, "user_custom_pattern/\n").unwrap();
        super::install_default_tsmignore(dir.path()).unwrap();
        let after = std::fs::read_to_string(&tsmignore).unwrap();
        assert_eq!(after, "user_custom_pattern/\n");
    }

    #[test]
    fn test_vector_fill_summary_reports_filled() {
        let msg = vector_fill_summary(7, 0);
        assert!(
            msg.contains('7'),
            "should report filled count, got: {msg:?}"
        );
        assert!(
            msg.to_lowercase().contains("backfilled"),
            "should announce backfilled vectors, got: {msg:?}"
        );
    }

    #[test]
    fn test_vector_fill_summary_no_missing_when_nothing_skipped() {
        let msg = vector_fill_summary(0, 0);
        assert_eq!(msg, "No missing vectors.");
    }

    #[test]
    fn test_vector_fill_summary_warns_when_skipped() {
        let msg = vector_fill_summary(0, 3);
        assert!(
            msg.contains('3'),
            "should report skipped count, got: {msg:?}"
        );
        assert!(
            msg.contains("reindex vectors"),
            "should point to `reindex vectors` to retry skipped chunks, got: {msg:?}"
        );
        assert_ne!(
            msg, "No missing vectors.",
            "skip-blocked chunks must not be reported as no-op"
        );
    }

    #[test]
    fn test_vector_fill_summary_reports_both_filled_and_skipped() {
        // A partial run fills some chunks while others stay stuck on the skip
        // list. The skip warning must not be suppressed by the filled count —
        // that is exactly the silent stuck-skip this change exists to prevent.
        let msg = vector_fill_summary(7, 3);
        assert!(
            msg.contains('7'),
            "should report filled count, got: {msg:?}"
        );
        assert!(
            msg.contains('3'),
            "should report skipped count, got: {msg:?}"
        );
        assert!(
            msg.contains("reindex vectors"),
            "should still point to `reindex vectors`, got: {msg:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_doctor_surfaces_skipped_vectors() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", dir.path());
        config::reload();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();
        {
            let conn = db::get_connection(&db_path).unwrap();
            conn.execute(
                "INSERT INTO documents(id, file_path, source_type, file_hash, indexed_at)
                 VALUES (1, 'daily/notes/a.md', 'note', 'h', '2026-01-01')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO chunks(id, document_id, chunk_index, content)
                 VALUES (1, 1, 0, 'one'), (2, 1, 1, 'two')",
                [],
            )
            .unwrap();
            // One chunk is stuck on the skip list (vector-fill will not retry it).
            conn.execute(
                "INSERT INTO chunks_vec_skip(chunk_id, reason) VALUES (1, 'encode_error')",
                [],
            )
            .unwrap();
        }

        let report = doctor_check(&db_path);
        let issues = report.issues();
        assert!(
            issues
                .iter()
                .any(|s| s.contains("skip") && s.contains("reindex vectors")),
            "doctor should surface skip-marked chunks with a `reindex vectors` hint, got: {issues:?}"
        );

        std::env::remove_var("TSM_STATE_DIR");
        config::reload();
    }

    #[test]
    #[serial_test::serial]
    fn test_doctor_skip_only_gap_does_not_advise_vector_fill() {
        // When every vector-less chunk is on the skip list, `vector-fill`
        // cannot help — doctor must not advise it (only `reindex vectors`),
        // otherwise the two hints contradict each other.
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", dir.path());
        config::reload();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();
        {
            let conn = db::get_connection(&db_path).unwrap();
            conn.execute(
                "INSERT INTO documents(id, file_path, source_type, file_hash, indexed_at)
                 VALUES (1, 'daily/notes/a.md', 'note', 'h', '2026-01-01')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO chunks(id, document_id, chunk_index, content)
                 VALUES (1, 1, 0, 'one'), (2, 1, 1, 'two')",
                [],
            )
            .unwrap();
            // Both chunks are stuck on the skip list — nothing is fillable.
            conn.execute(
                "INSERT INTO chunks_vec_skip(chunk_id, reason)
                 VALUES (1, 'encode_error'), (2, 'encode_error')",
                [],
            )
            .unwrap();
        }

        let report = doctor_check(&db_path);
        let issues = report.issues();
        assert!(
            issues
                .iter()
                .any(|s| s.contains("skip") && s.contains("reindex vectors")),
            "should surface the skip list, got: {issues:?}"
        );
        assert!(
            !issues.iter().any(|s| s.contains("vector-fill")),
            "must not advise `vector-fill` when the whole gap is skip-listed, got: {issues:?}"
        );

        std::env::remove_var("TSM_STATE_DIR");
        config::reload();
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
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
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
    #[serial_test::serial]
    fn test_doctor_model_files_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", dir.path());
        config::reload();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();

        let report = doctor_check(&db_path);
        let issues = report.issues();
        assert!(
            issues.iter().any(|s| s.contains("Model:")),
            "expected model warning, got: {issues:?}"
        );
        std::env::remove_var("TSM_STATE_DIR");
        config::reload();
    }

    #[test]
    #[serial_test::serial]
    fn test_doctor_model_files_present() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", dir.path());
        config::reload();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();

        let models_dir = config::models_dir();
        std::fs::create_dir_all(&models_dir).unwrap();
        for f in config::MODEL_FILES {
            std::fs::write(models_dir.join(f), "dummy").unwrap();
        }

        let conn = db::get_connection(&db_path).unwrap();
        let report = run_doctor(&conn, &db_path);
        let ok = report.ok();
        assert!(
            ok.iter().any(|s| s.contains("Model:")),
            "expected model OK, got: {ok:?}"
        );
        std::env::remove_var("TSM_STATE_DIR");
        config::reload();
    }

    #[test]
    #[serial_test::serial]
    fn test_doctor_model_files_partial() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TSM_STATE_DIR", dir.path());
        config::reload();
        let db_path = dir.path().join("test.db");
        db::init_db(&db_path).unwrap();

        let models_dir = config::models_dir();
        std::fs::create_dir_all(&models_dir).unwrap();
        // Only create 2 of 3 files
        std::fs::write(models_dir.join("config.json"), "{}").unwrap();
        std::fs::write(models_dir.join("tokenizer.json"), "{}").unwrap();

        let conn = db::get_connection(&db_path).unwrap();
        let report = run_doctor(&conn, &db_path);
        let issues = report.issues();
        assert!(
            issues.iter().any(|s| s.contains("incomplete")),
            "expected incomplete warning, got: {issues:?}"
        );
        assert!(
            issues.iter().any(|s| s.contains("model.safetensors")),
            "expected missing file name, got: {issues:?}"
        );
        std::env::remove_var("TSM_STATE_DIR");
        config::reload();
    }

    #[test]
    fn test_init_scaffolds_default_hooks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join(".tsm");
        std::fs::create_dir_all(&state).unwrap();
        install_default_hooks(&state).unwrap();
        let ex =
            std::fs::read_to_string(state.join("hooks/extract/10-md_frontmatter.lua")).unwrap();
        let sc = std::fs::read_to_string(state.join("hooks/score/10-default.lua")).unwrap();
        assert!(ex.contains("function extract"));
        assert!(sc.contains("function score"));
    }
}
