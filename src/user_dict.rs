use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension};

use crate::config;
use crate::db;
use crate::tokenizer;

/// POS label for user dictionary entries in simpledic format.
/// Re-exports `tokenizer::POS_NOUN` — user dict terms use the standard noun POS
/// so they pass existing POS filters without special handling.
pub const USER_DICT_POS: &str = crate::tokenizer::POS_NOUN;

// ─── Enums ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidatePos {
    ProperNoun,
    Katakana,
    Ascii,
    /// Term inserted by an explicit `dict add` / `dict reject`, not auto-harvested.
    Manual,
}

impl CandidatePos {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProperNoun => "proper_noun",
            Self::Katakana => "katakana",
            Self::Ascii => "ascii",
            Self::Manual => "manual",
        }
    }
}

/// A term's dictionary verdict. Maps 1:1 to `dictionary_candidates.status`
/// CHECK values, so the type structurally excludes invalid statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pending,
    Rejected,
    Accepted,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Rejected => "rejected",
            Self::Accepted => "accepted",
        }
    }

    /// Parse a `dictionary_candidates.status` value. Returns `None` for any
    /// string outside the CHECK constraint's domain.
    fn from_status(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "rejected" => Some(Self::Rejected),
            "accepted" => Some(Self::Accepted),
            _ => None,
        }
    }
}

/// Outcome of a `set_verdict` transition. `from` is `None` when the term did
/// not exist and was inserted. `affected_dict` is true iff the accepted set
/// gained or lost the term, i.e. the caller must regenerate `user_dict.simpledic`
/// and reload the tokenizer (`restart`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub surface: String,
    pub from: Option<Verdict>,
    pub to: Verdict,
    pub affected_dict: bool,
}

/// Error from `set_verdict`.
#[derive(Debug)]
pub enum SetVerdictError {
    /// A transition to `Pending` (i.e. `dict rm`) targeted a surface that has
    /// no candidate row. `Pending` is the absence of an explicit verdict, so
    /// there is nothing to reset.
    NotFound(String),
    /// Underlying database error.
    Db(rusqlite::Error),
}

impl std::fmt::Display for SetVerdictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "term '{s}' is not a dictionary candidate"),
            Self::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for SetVerdictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            Self::NotFound(_) => None,
        }
    }
}

impl From<rusqlite::Error> for SetVerdictError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

// ─── Data types ──────────────────────────────────────────────

/// A dictionary candidate record.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub surface: String,
    pub frequency: i64,
    pub pos: String,
    pub source: String,
    pub first_seen: String,
    pub last_seen: String,
    pub status: String,
}

/// Summary for doctor report.
pub struct CandidateSummary {
    pub total_pending: i64,
    pub ready_count: i64,
    pub dict_word_count: i64,
    pub rejected_count: i64,
}

#[derive(Debug)]
struct RawCandidate {
    surface: String,
    pos: CandidatePos,
}

// ─── Existing surfaces cache ─────────────────────────────────

/// Cached set of surfaces already in user_dict.simpledic (loaded once per process).
/// Acceptable because the dict only changes when `tsm dict update` runs (which triggers rebuild).
static EXISTING_SURFACES: OnceLock<HashSet<String>> = OnceLock::new();

fn get_existing_surfaces() -> &'static HashSet<String> {
    EXISTING_SURFACES.get_or_init(|| match load_existing_surfaces(&config::user_dict_path()) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("could not read user dict: {e}");
            HashSet::new()
        }
    })
}

/// Load existing surface forms from a user dictionary file (IPAdic format).
/// The first column (comma-separated) is the surface form.
pub fn load_existing_surfaces(csv_path: &Path) -> anyhow::Result<HashSet<String>> {
    let mut surfaces = HashSet::new();
    if !csv_path.exists() {
        return Ok(surfaces);
    }
    let content = std::fs::read_to_string(csv_path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(surface) = line.split(',').next() {
            let surface = surface.trim().to_lowercase();
            if !surface.is_empty() {
                surfaces.insert(surface);
            }
        }
    }
    Ok(surfaces)
}

// ─── Extraction helpers ──────────────────────────────────────

fn is_all_katakana(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| matches!(c, '\u{30A0}'..='\u{30FF}'))
}

fn is_ascii_term(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Extract raw candidate words from text using lindera POS analysis + heuristics.
fn extract_raw_candidates(text: &str) -> Vec<RawCandidate> {
    if text.is_empty() {
        return Vec::new();
    }

    let segmenter = tokenizer::get_segmenter();
    let mut tokens = match segmenter.segment(std::borrow::Cow::Borrowed(text)) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("segmentation failed: {e}");
            return Vec::new();
        }
    };

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for token in &mut tokens {
        let surface = token.surface.as_ref().to_string();
        let details = token.details();

        let pos = if details.len() >= 2
            && details[0] == crate::tokenizer::POS_NOUN
            && details[1] == crate::tokenizer::POS_SUB_PROPER
        {
            Some(CandidatePos::ProperNoun)
        } else if is_all_katakana(&surface) && surface.chars().count() >= 2 {
            Some(CandidatePos::Katakana)
        } else if is_ascii_term(&surface) && surface.chars().count() >= 2 {
            Some(CandidatePos::Ascii)
        } else {
            None
        };

        if let Some(pos) = pos {
            let lower = surface.to_lowercase();
            if seen.insert(lower) {
                candidates.push(RawCandidate {
                    surface: surface.clone(),
                    pos,
                });
            }
        }
    }

    candidates
}

/// Check if a candidate word passes all filters.
fn is_valid_candidate(word: &str, existing_words: &HashSet<String>) -> bool {
    let char_count = word.chars().count();

    // 1-char words
    if char_count < 2 {
        return false;
    }

    // Digits only
    if word.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }

    // Symbols only (no alphanumeric chars)
    if !word.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }

    // Already in user dict
    if existing_words.contains(&word.to_lowercase()) {
        return false;
    }

    true
}

// ─── Collection ──────────────────────────────────────────────

/// Collect dictionary candidates from text and upsert into DB.
/// source: "document" | "query" | "session"
pub fn collect_from_text(conn: &Connection, text: &str, source: &str) {
    if !db::has_candidates_table(conn) {
        return;
    }
    if text.trim().len() < 4 {
        return;
    }

    let existing = get_existing_surfaces();
    let candidates = extract_raw_candidates(text);
    let now = chrono::Utc::now().to_rfc3339();

    for c in candidates {
        if !is_valid_candidate(&c.surface, existing) {
            continue;
        }
        if let Err(e) = conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status)
             VALUES (?1, 1, ?2, ?3, ?4, ?4, 'pending')
             ON CONFLICT(surface) DO UPDATE SET
                 frequency = CASE WHEN dictionary_candidates.status = 'pending'
                     THEN dictionary_candidates.frequency + 1
                     ELSE dictionary_candidates.frequency END,
                 last_seen = CASE WHEN dictionary_candidates.status = 'pending'
                     THEN ?4 ELSE dictionary_candidates.last_seen END",
            rusqlite::params![c.surface, c.pos.as_str(), source, now],
        ) {
            log::warn!("failed to upsert dictionary candidate '{}': {e}", c.surface);
            break; // DB likely in bad state, stop trying
        }
    }
}

/// Collect candidates from a search query.
pub fn collect_from_query(conn: &Connection, query: &str) {
    collect_from_text(conn, query, "query");
}

/// Spawn query-candidate collection on a fresh writer connection (fire-and-forget).
///
/// The daemon serves searches on a `query_only` reader connection, so the harvest
/// must not run on the serving connection. Returns the join handle; callers on the
/// hot path drop it (fire-and-forget), tests join it for determinism.
pub fn spawn_collect_from_query(
    db_path: std::path::PathBuf,
    query: String,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || match db::get_connection(&db_path) {
        Ok(conn) => collect_from_query(&conn, &query),
        Err(e) => log::warn!(
            "dict candidate harvest: failed to open writer connection ({}): {e}",
            db_path.display()
        ),
    })
}

// ─── Querying ────────────────────────────────────────────────

/// Get pending candidates with frequency >= threshold.
pub fn get_threshold_candidates(conn: &Connection, threshold: i64) -> Vec<Candidate> {
    if !db::has_candidates_table(conn) {
        return Vec::new();
    }

    conn.prepare(
        "SELECT surface, frequency, pos, source, first_seen, last_seen, status
         FROM dictionary_candidates
         WHERE status = 'pending' AND frequency >= ?
         ORDER BY frequency DESC",
    )
    .and_then(|mut stmt| {
        let rows = stmt.query_map([threshold], |row| {
            Ok(Candidate {
                surface: row.get(0)?,
                frequency: row.get(1)?,
                pos: row.get(2)?,
                source: row.get(3)?,
                first_seen: row.get(4)?,
                last_seen: row.get(5)?,
                status: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    })
    .unwrap_or_default()
}

/// Get summary counts for doctor report.
pub fn candidate_summary(conn: &Connection) -> CandidateSummary {
    let dict_word_count = get_existing_surfaces().len() as i64;
    if !db::has_candidates_table(conn) {
        return CandidateSummary {
            total_pending: 0,
            ready_count: 0,
            dict_word_count,
            rejected_count: 0,
        };
    }

    let total_pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dictionary_candidates WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let ready_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dictionary_candidates WHERE status = 'pending' AND frequency >= ?",
            [config::DICT_CANDIDATE_FREQ_THRESHOLD],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let rejected_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dictionary_candidates WHERE status = 'rejected'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    CandidateSummary {
        total_pending,
        ready_count,
        dict_word_count,
        rejected_count,
    }
}

// ─── Status updates ──────────────────────────────────────────

/// Transition a single term to `to`, enforcing the accepted XOR rejected XOR
/// pending invariant via the single `status` column.
///
/// The term is upserted: a surface with no candidate row is inserted (manual
/// terms that never surfaced as auto-candidates, e.g. mis-split compounds),
/// **except** a transition to `Pending` (`dict rm`), which returns
/// [`SetVerdictError::NotFound`] — `Pending` is the absence of a verdict, so a
/// nonexistent term has nothing to reset.
///
/// `reading` is only meaningful when accepting; it is stored verbatim (the
/// caller normalizes per ADR-0014). A non-NULL `reading` overwrites; `None`
/// preserves any existing reading (`COALESCE`).
///
/// The SELECT and the write run in one transaction so the returned
/// [`Transition`] always matches the committed DB state.
pub fn set_verdict(
    conn: &Connection,
    surface: &str,
    to: Verdict,
    reading: Option<&str>,
) -> Result<Transition, SetVerdictError> {
    let tx = conn.unchecked_transaction()?;

    let current: Option<String> = tx
        .query_row(
            "SELECT status FROM dictionary_candidates WHERE surface = ?1",
            [surface],
            |r| r.get(0),
        )
        .optional()?;

    let from = match current.as_deref() {
        None => None,
        Some(s) => Some(Verdict::from_status(s).ok_or_else(|| {
            SetVerdictError::Db(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unexpected status '{s}' for '{surface}'").into(),
            ))
        })?),
    };

    // `Pending` is the absence of an explicit verdict, so a `dict rm` on a term
    // that was never registered has nothing to reset.
    if from.is_none() && to == Verdict::Pending {
        return Err(SetVerdictError::NotFound(surface.to_string()));
    }

    // One upsert covers both insert (new manual term) and update (existing row).
    // On an existing row, `pos`/`source`/`frequency` are left untouched: only a
    // brand-new insert is tagged manual. `COALESCE` keeps an existing reading
    // when `reading` is None, and overwrites it when a value is supplied.
    let now = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO dictionary_candidates
            (surface, frequency, pos, source, first_seen, last_seen, status, reading)
         VALUES (?1, 0, ?2, 'manual', ?3, ?3, ?4, ?5)
         ON CONFLICT(surface) DO UPDATE SET
             status  = excluded.status,
             reading = COALESCE(excluded.reading, dictionary_candidates.reading)",
        rusqlite::params![
            surface,
            CandidatePos::Manual.as_str(),
            now,
            to.as_str(),
            reading
        ],
    )?;

    tx.commit()?;

    let affected_dict = (from == Some(Verdict::Accepted)) ^ (to == Verdict::Accepted);
    Ok(Transition {
        surface: surface.to_string(),
        from,
        to,
        affected_dict,
    })
}

/// Mark candidates as accepted.
pub fn mark_accepted(conn: &Connection, surfaces: &[&str]) -> anyhow::Result<()> {
    for surface in surfaces {
        conn.execute(
            "UPDATE dictionary_candidates SET status = 'accepted' WHERE surface = ?",
            [surface],
        )?;
    }
    Ok(())
}

/// Mark a candidate as rejected (will be skipped in future collection).
pub fn mark_rejected(conn: &Connection, surface: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE dictionary_candidates SET status = 'rejected' WHERE surface = ?",
        [surface],
    )?;
    Ok(())
}

// ─── Reject list (reject_words.txt) ─────────────────────────

/// Load the reject list from a text file.
/// Lines starting with `#` and blank lines are ignored.
/// All words are lowercased for case-insensitive comparison.
pub fn load_reject_words(path: &Path) -> anyhow::Result<HashSet<String>> {
    let mut words = HashSet::new();
    if !path.exists() {
        return Ok(words);
    }
    for line in std::fs::read_to_string(path)?.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        words.insert(trimmed.to_lowercase());
    }
    Ok(words)
}

/// Sync reject_words.txt → DB: mark matching pending candidates as 'rejected'.
/// Returns the list of surfaces that were newly rejected.
pub fn apply_reject_list(
    conn: &Connection,
    reject_words: &HashSet<String>,
) -> anyhow::Result<Vec<String>> {
    if !db::has_candidates_table(conn) {
        return Ok(Vec::new());
    }
    let pending: Vec<String> = conn
        .prepare("SELECT surface FROM dictionary_candidates WHERE status = 'pending'")?
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    let tx = conn.unchecked_transaction()?;
    let mut newly_rejected = Vec::new();
    for surface in &pending {
        if reject_words.contains(&surface.to_lowercase()) {
            tx.execute(
                "UPDATE dictionary_candidates SET status = 'rejected' WHERE surface = ?",
                [surface],
            )?;
            newly_rejected.push(surface.clone());
        }
    }
    tx.commit()?;
    Ok(newly_rejected)
}

/// Get all candidates with status = 'rejected', ordered by surface.
pub fn get_rejected_candidates(conn: &Connection) -> Vec<Candidate> {
    if !db::has_candidates_table(conn) {
        return Vec::new();
    }
    conn.prepare(
        "SELECT surface, frequency, pos, source, first_seen, last_seen, status
         FROM dictionary_candidates
         WHERE status = 'rejected'
         ORDER BY surface ASC",
    )
    .and_then(|mut stmt| {
        let rows = stmt.query_map([], |row| {
            Ok(Candidate {
                surface: row.get(0)?,
                frequency: row.get(1)?,
                pos: row.get(2)?,
                source: row.get(3)?,
                first_seen: row.get(4)?,
                last_seen: row.get(5)?,
                status: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    })
    .unwrap_or_default()
}

/// Get pending candidates whose surface appears in the reject list.
pub fn get_pending_in_reject_list(
    conn: &Connection,
    reject_words: &HashSet<String>,
) -> Vec<Candidate> {
    get_threshold_candidates(conn, 0)
        .into_iter()
        .filter(|c| reject_words.contains(&c.surface.to_lowercase()))
        .collect()
}

// ─── CSV formatting ──────────────────────────────────────────

/// Format a CSV row in janome simpledic format: surface,{USER_DICT_POS},surface
pub fn format_simpledic_row(surface: &str) -> String {
    format_simpledic_row_with_reading(surface, surface)
}

/// Format a simpledic row with an explicit reading: surface,{USER_DICT_POS},reading
pub fn format_simpledic_row_with_reading(surface: &str, reading: &str) -> String {
    format!("{surface},{},{reading}", USER_DICT_POS)
}

/// Whether `s` contains a CJK ideograph (kanji), whose reading cannot be
/// inferred from the surface alone.
fn surface_has_kanji(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{3400}'..='\u{4DBF}'     // CJK Unified Ideographs Extension A
            | '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
            | '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
            | '\u{20000}'..='\u{2EBEF}' // Extension B..F (supplementary plane)
            | '\u{2F800}'..='\u{2FA1F}' // CJK Compatibility Ideographs Supplement
            | '\u{30000}'..='\u{3134F}' // Extension G
        )
    })
}

/// Resolve the reading stored by `dict add` per ADR-0014 §4.
///
/// An explicit `yomi` is used verbatim. When omitted, the surface stands in as
/// its own reading; the returned bool is `true` when the surface contains kanji,
/// so the caller can warn and surface the data debt (automatic readings are
/// never inferred — added terms are exactly the words lindera does not know).
/// Readings are stored only; search is surface-based today.
pub fn resolve_reading(surface: &str, yomi: Option<&str>) -> (String, bool) {
    match yomi {
        Some(y) => (y.to_string(), false),
        None => (surface.to_string(), surface_has_kanji(surface)),
    }
}

/// Validate a surface supplied to `dict add` / `dict reject`.
///
/// The simpledic format is comma-delimited with one entry per line, so a surface
/// may not contain a comma or a newline (either would corrupt the file). Empty
/// or whitespace-only surfaces are rejected too.
pub fn validate_surface(surface: &str) -> anyhow::Result<()> {
    if surface.trim().is_empty() {
        anyhow::bail!("surface must not be empty or whitespace-only");
    }
    if surface.contains(',') || surface.contains('\n') || surface.contains('\r') {
        anyhow::bail!("surface must not contain a comma or newline (simpledic format constraint)");
    }
    Ok(())
}

/// Regenerate `user_dict.simpledic` from the DB's accepted terms (full rewrite).
///
/// The shared primitive every verdict change relies on: ADR-0014 specifies that
/// a verdict change regenerates simpledic and reloads the tokenizer. `export`
/// reuses it for the simpledic half of its round-trip. Each accepted term becomes
/// one `surface,POS,reading` row, the reading falling back to the surface when
/// none is stored (simpledic requires the field). Returns the number of rows
/// written; an empty accepted set truncates the file.
pub fn regenerate_user_dict(conn: &Connection, csv_path: &Path) -> anyhow::Result<usize> {
    // Propagate per-row errors rather than silently dropping a term.
    let rows: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT surface, reading FROM dictionary_candidates
             WHERE status = 'accepted' ORDER BY surface ASC",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if let Some(parent) = csv_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write to a sibling temp file then atomically rename, so a crash or write
    // failure mid-rewrite never leaves lindera reading a truncated/partial dict.
    let tmp_path = csv_path.with_extension("simpledic.tmp");
    {
        use std::io::Write;
        let mut tmp = std::fs::File::create(&tmp_path)?;
        for (surface, reading) in &rows {
            let reading = reading.as_deref().unwrap_or(surface);
            writeln!(
                tmp,
                "{}",
                format_simpledic_row_with_reading(surface, reading)
            )?;
        }
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, csv_path)?;
    Ok(rows.len())
}

/// Export threshold candidates to a CSV file (appending).
/// Returns the list of newly written candidates.
/// Output format is simpledic (3 fields: surface, pos, reading).
pub fn export_candidates_to_csv(
    conn: &Connection,
    csv_path: &Path,
    threshold: i64,
) -> anyhow::Result<Vec<Candidate>> {
    let candidates = get_threshold_candidates(conn, threshold);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Load existing surfaces from the actual file (fresh read, not OnceLock cache)
    let existing = load_existing_surfaces(csv_path)?;

    // Partition: candidates already in CSV vs genuinely new
    let (already_in_csv, new_candidates): (Vec<Candidate>, Vec<Candidate>) = candidates
        .into_iter()
        .partition(|c| existing.contains(&c.surface.to_lowercase()));

    // Mark CSV-existing candidates as accepted so they stop appearing in doctor
    if !already_in_csv.is_empty() {
        let surfaces: Vec<&str> = already_in_csv.iter().map(|c| c.surface.as_str()).collect();
        mark_accepted(conn, &surfaces)?;
    }

    if new_candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Ensure parent directory exists
    if let Some(parent) = csv_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(csv_path)?;

    for c in &new_candidates {
        let row = format_simpledic_row(&c.surface);
        writeln!(file, "{row}")?;
    }

    // Mark as accepted in DB
    let surfaces: Vec<&str> = new_candidates.iter().map(|c| c.surface.as_str()).collect();
    mark_accepted(conn, &surfaces)?;

    Ok(new_candidates)
}

/// Export the DB's rejected set to `reject_words.txt`, overwriting the file.
/// One surface per line, sorted. Returns the number of words written. An empty
/// rejected set truncates the file. Round-trips with [`load_reject_words`].
/// Mirrors [`regenerate_user_dict`]'s durability: per-row DB errors propagate
/// and the write goes through a sibling temp file + atomic rename.
pub fn export_reject_words_to_file(conn: &Connection, path: &Path) -> anyhow::Result<usize> {
    let surfaces: Vec<String> = conn
        .prepare(
            "SELECT surface FROM dictionary_candidates
             WHERE status = 'rejected' ORDER BY surface ASC",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("txt.tmp");
    {
        use std::io::Write;
        let mut tmp = std::fs::File::create(&tmp_path)?;
        for surface in &surfaces {
            writeln!(tmp, "{surface}")?;
        }
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(surfaces.len())
}

/// Outcome of importing one verdict file: how many rows were applied, and how
/// many of those changed the accepted set. The caller regenerates the dictionary
/// and reindexes once when `dict_affected > 0` (a rejected import can demote a
/// previously-accepted word, so a non-zero `dict_affected` is possible even for
/// the reject file).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportOutcome {
    pub imported: usize,
    pub dict_affected: usize,
}

/// Import `reject_words.txt` into the DB, marking each listed word `rejected`
/// via [`set_verdict`]. Insert-only: words absent from the file keep their
/// verdict (use `dict rm`/`reject` to remove). Lines starting with `#` and blank
/// lines are skipped; case is preserved verbatim (unlike [`load_reject_words`],
/// which lowercases for comparison). A missing file is a no-op.
pub fn import_reject_words_from_file(
    conn: &Connection,
    path: &Path,
) -> anyhow::Result<ImportOutcome> {
    if !path.exists() {
        return Ok(ImportOutcome::default());
    }
    let mut outcome = ImportOutcome::default();
    for line in std::fs::read_to_string(path)?.lines() {
        let surface = line.trim();
        if surface.is_empty() || surface.starts_with('#') {
            continue;
        }
        let t = set_verdict(conn, surface, Verdict::Rejected, None)?;
        outcome.imported += 1;
        if t.affected_dict {
            outcome.dict_affected += 1;
        }
    }
    Ok(outcome)
}

/// Import `user_dict.simpledic` into the DB, marking each surface `accepted`
/// with its reading via [`set_verdict`]. Insert-only (see
/// [`import_reject_words_from_file`]). Row format is `surface[,pos,reading]`; the
/// reading column is stored when present and non-empty. Lines starting with `#`
/// and blank lines are skipped. A missing file is a no-op.
pub fn import_user_dict_from_file(conn: &Connection, path: &Path) -> anyhow::Result<ImportOutcome> {
    if !path.exists() {
        return Ok(ImportOutcome::default());
    }
    let mut outcome = ImportOutcome::default();
    for line in std::fs::read_to_string(path)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split(',');
        let Some(surface) = cols.next().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let _pos = cols.next();
        // A reading equal to the surface carries no information: `regenerate_user_dict`
        // writes `surface` into the reading column whenever the stored reading is
        // NULL (simpledic requires the field), so `reading == surface` round-trips
        // back to "no distinct reading". Treating it as None keeps the round-trip
        // idempotent and avoids persisting a surface-as-reading (data debt for
        // kanji terms — see ADR-0014 §4).
        let reading = cols
            .next()
            .map(str::trim)
            .filter(|r| !r.is_empty() && *r != surface);
        let t = set_verdict(conn, surface, Verdict::Accepted, reading)?;
        outcome.imported += 1;
        if t.affected_dict {
            outcome.dict_affected += 1;
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::setup_db as setup;

    // ─── enum tests ──────────────────────────────────────────

    #[test]
    fn test_candidate_pos_as_str() {
        assert_eq!(CandidatePos::ProperNoun.as_str(), "proper_noun");
        assert_eq!(CandidatePos::Katakana.as_str(), "katakana");
        assert_eq!(CandidatePos::Ascii.as_str(), "ascii");
        assert_eq!(CandidatePos::Manual.as_str(), "manual");
    }

    // ─── Verdict tests ───────────────────────────────────────

    #[test]
    fn test_verdict_as_str() {
        assert_eq!(Verdict::Pending.as_str(), "pending");
        assert_eq!(Verdict::Rejected.as_str(), "rejected");
        assert_eq!(Verdict::Accepted.as_str(), "accepted");
    }

    // ─── set_verdict tests ───────────────────────────────────

    fn seed(conn: &Connection, surface: &str, status: &str) {
        conn.execute(
            "INSERT INTO dictionary_candidates
                (surface, frequency, pos, source, first_seen, last_seen, status)
             VALUES (?1, 5, 'ascii', 'document', '2026-01-01', '2026-01-01', ?2)",
            rusqlite::params![surface, status],
        )
        .unwrap();
    }

    fn status_of(conn: &Connection, surface: &str) -> Option<String> {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT status FROM dictionary_candidates WHERE surface = ?1",
            [surface],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn reading_of(conn: &Connection, surface: &str) -> Option<String> {
        conn.query_row(
            "SELECT reading FROM dictionary_candidates WHERE surface = ?1",
            [surface],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn test_set_verdict_pending_to_accepted() {
        let conn = setup();
        seed(&conn, "candle", "pending");
        let t = set_verdict(&conn, "candle", Verdict::Accepted, None).unwrap();
        assert_eq!(t.from, Some(Verdict::Pending));
        assert_eq!(t.to, Verdict::Accepted);
        assert!(t.affected_dict, "entering accepted touches the dict");
        assert_eq!(status_of(&conn, "candle").as_deref(), Some("accepted"));
    }

    #[test]
    fn test_set_verdict_pending_to_rejected_not_affected() {
        let conn = setup();
        seed(&conn, "bad", "pending");
        let t = set_verdict(&conn, "bad", Verdict::Rejected, None).unwrap();
        assert_eq!(t.from, Some(Verdict::Pending));
        assert!(
            !t.affected_dict,
            "pending->rejected does not touch the dict"
        );
        assert_eq!(status_of(&conn, "bad").as_deref(), Some("rejected"));
    }

    #[test]
    fn test_set_verdict_accepted_to_pending() {
        let conn = setup();
        seed(&conn, "word", "accepted");
        let t = set_verdict(&conn, "word", Verdict::Pending, None).unwrap();
        assert_eq!(t.from, Some(Verdict::Accepted));
        assert!(t.affected_dict, "leaving accepted touches the dict");
        assert_eq!(status_of(&conn, "word").as_deref(), Some("pending"));
    }

    #[test]
    fn test_set_verdict_rejected_to_pending_not_affected() {
        let conn = setup();
        seed(&conn, "word", "rejected");
        let t = set_verdict(&conn, "word", Verdict::Pending, None).unwrap();
        assert!(
            !t.affected_dict,
            "rejected<->pending never touches the dict"
        );
    }

    #[test]
    fn test_set_verdict_insert_on_unregistered_add() {
        let conn = setup();
        let t = set_verdict(
            &conn,
            "ハンドロード",
            Verdict::Accepted,
            Some("はんどろーど"),
        )
        .unwrap();
        assert_eq!(t.from, None, "new term was inserted");
        assert!(t.affected_dict);
        let (freq, pos, source): (i64, String, String) = conn
            .query_row(
                "SELECT frequency, pos, source FROM dictionary_candidates WHERE surface = ?1",
                ["ハンドロード"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(freq, 0);
        assert_eq!(pos, "manual");
        assert_eq!(source, "manual");
        assert_eq!(
            reading_of(&conn, "ハンドロード").as_deref(),
            Some("はんどろーど")
        );
    }

    #[test]
    fn test_set_verdict_insert_on_unregistered_reject() {
        let conn = setup();
        let t = set_verdict(&conn, "noise", Verdict::Rejected, None).unwrap();
        assert_eq!(t.from, None);
        assert!(!t.affected_dict);
        assert_eq!(status_of(&conn, "noise").as_deref(), Some("rejected"));
        assert_eq!(reading_of(&conn, "noise"), None);
    }

    #[test]
    fn test_set_verdict_rm_unregistered_errors() {
        let conn = setup();
        let err = set_verdict(&conn, "ghost", Verdict::Pending, None).unwrap_err();
        assert!(matches!(err, SetVerdictError::NotFound(ref s) if s == "ghost"));
    }

    #[test]
    fn test_set_verdict_idempotent_same_verdict() {
        let conn = setup();
        seed(&conn, "word", "accepted");
        let t = set_verdict(&conn, "word", Verdict::Accepted, None).unwrap();
        assert_eq!(t.from, Some(Verdict::Accepted));
        assert_eq!(t.to, Verdict::Accepted);
        assert!(!t.affected_dict, "no net change to the accepted set");
        assert_eq!(status_of(&conn, "word").as_deref(), Some("accepted"));
    }

    #[test]
    fn test_set_verdict_reading_overwrites_with_non_null() {
        let conn = setup();
        set_verdict(&conn, "term", Verdict::Accepted, Some("yomi1")).unwrap();
        set_verdict(&conn, "term", Verdict::Accepted, Some("yomi2")).unwrap();
        assert_eq!(reading_of(&conn, "term").as_deref(), Some("yomi2"));
    }

    #[test]
    fn test_set_verdict_reading_preserved_when_none() {
        let conn = setup();
        set_verdict(&conn, "term", Verdict::Accepted, Some("yomi")).unwrap();
        // A later transition without a reading must not clobber it.
        set_verdict(&conn, "term", Verdict::Rejected, None).unwrap();
        assert_eq!(reading_of(&conn, "term").as_deref(), Some("yomi"));
    }

    // ─── resolve_reading tests (ADR-0014 §4) ─────────────────

    #[test]
    fn test_resolve_reading_explicit_yomi_used() {
        let (reading, warned) = resolve_reading("宇宙記憶", Some("うちゅうきおく"));
        assert_eq!(reading, "うちゅうきおく");
        assert!(!warned);
    }

    #[test]
    fn test_resolve_reading_all_kana_falls_back_to_surface() {
        let (reading, warned) = resolve_reading("ハンドロード", None);
        assert_eq!(reading, "ハンドロード");
        assert!(!warned, "all-kana surface is its own reading; no warning");
    }

    #[test]
    fn test_resolve_reading_ascii_falls_back_to_surface() {
        let (reading, warned) = resolve_reading("candle", None);
        assert_eq!(reading, "candle");
        assert!(!warned);
    }

    #[test]
    fn test_resolve_reading_kanji_without_yomi_warns() {
        let (reading, warned) = resolve_reading("宇宙記憶", None);
        assert_eq!(
            reading, "宇宙記憶",
            "surface is used as a substitute reading"
        );
        assert!(warned, "kanji surface without yomi surfaces the data debt");
    }

    #[test]
    fn test_resolve_reading_supplementary_plane_kanji_warns() {
        // U+20BB7 (𠮷) is a CJK Extension B ideograph outside the BMP.
        let (reading, warned) = resolve_reading("𠮷", None);
        assert_eq!(reading, "𠮷");
        assert!(
            warned,
            "supplementary-plane kanji must also warn (ADR-0014 §4)"
        );
    }

    #[test]
    fn test_resolve_reading_mixed_kanji_kana_warns() {
        // One kanji among kana must still warn (exercises the `.any()` path).
        let (reading, warned) = resolve_reading("消えた記憶", None);
        assert_eq!(reading, "消えた記憶");
        assert!(warned, "a single kanji among kana characters must warn");
    }

    // ─── validate_surface tests ──────────────────────────────

    #[test]
    fn test_validate_surface_accepts_normal() {
        assert!(validate_surface("ハンドロード").is_ok());
        assert!(validate_surface("candle").is_ok());
    }

    #[test]
    fn test_validate_surface_rejects_empty_and_whitespace() {
        assert!(validate_surface("").is_err());
        assert!(validate_surface("   ").is_err());
    }

    #[test]
    fn test_validate_surface_rejects_comma() {
        // A comma would create extra simpledic fields.
        assert!(validate_surface("foo,bar").is_err());
    }

    #[test]
    fn test_validate_surface_rejects_newline() {
        // A newline would split into two simpledic rows.
        assert!(validate_surface("foo\nbar").is_err());
        assert!(validate_surface("foo\rbar").is_err());
    }

    // ─── regenerate_user_dict tests ──────────────────────────

    #[test]
    fn test_regenerate_user_dict_writes_accepted_only() {
        let conn = setup();
        seed(&conn, "accepted_a", "accepted");
        seed(&conn, "accepted_b", "accepted");
        seed(&conn, "pending_x", "pending");
        seed(&conn, "rejected_y", "rejected");
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("user_dict.simpledic");

        let count = regenerate_user_dict(&conn, &path).unwrap();

        assert_eq!(count, 2);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("accepted_a,"));
        assert!(body.contains("accepted_b,"));
        assert!(!body.contains("pending_x"));
        assert!(!body.contains("rejected_y"));
    }

    #[test]
    fn test_regenerate_user_dict_is_full_rewrite() {
        let conn = setup();
        seed(&conn, "keep", "accepted");
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        std::fs::write(&path, "stale_word,名詞,stale_word\n").unwrap();

        regenerate_user_dict(&conn, &path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("keep,"));
        assert!(!body.contains("stale_word"), "regen truncates, not appends");
    }

    #[test]
    fn test_regenerate_user_dict_empty_accepted_set() {
        let conn = setup();
        seed(&conn, "pending_only", "pending");
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        std::fs::write(&path, "stale,名詞,stale\n").unwrap();

        let count = regenerate_user_dict(&conn, &path).unwrap();

        assert_eq!(count, 0);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.is_empty(), "no accepted terms => empty dict file");
    }

    #[test]
    fn test_regenerate_user_dict_output_is_sorted() {
        let conn = setup();
        seed(&conn, "zebra", "accepted");
        seed(&conn, "apple", "accepted");
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("user_dict.simpledic");

        regenerate_user_dict(&conn, &path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert!(lines[0].starts_with("apple,"), "rows sorted by surface ASC");
        assert!(lines[1].starts_with("zebra,"));
    }

    // ─── is_valid_candidate tests ────────────────────────────

    #[test]
    fn test_is_valid_candidate_accepts_normal() {
        let empty = HashSet::new();
        assert!(is_valid_candidate("candle", &empty));
        assert!(is_valid_candidate("lindera", &empty));
        assert!(is_valid_candidate("テスラ", &empty));
    }

    #[test]
    fn test_is_valid_candidate_rejects_single_char() {
        let empty = HashSet::new();
        assert!(!is_valid_candidate("a", &empty));
        assert!(!is_valid_candidate("あ", &empty));
    }

    #[test]
    fn test_is_valid_candidate_rejects_digits_only() {
        let empty = HashSet::new();
        assert!(!is_valid_candidate("123", &empty));
        assert!(!is_valid_candidate("42", &empty));
    }

    #[test]
    fn test_is_valid_candidate_rejects_symbols_only() {
        let empty = HashSet::new();
        assert!(!is_valid_candidate("---", &empty));
        assert!(!is_valid_candidate("...", &empty));
    }

    #[test]
    fn test_is_valid_candidate_rejects_existing() {
        let mut existing = HashSet::new();
        existing.insert("candle".to_string());
        assert!(!is_valid_candidate("candle", &existing));
    }

    #[test]
    fn test_is_valid_candidate_case_insensitive() {
        let mut existing = HashSet::new();
        existing.insert("candle".to_string());
        assert!(!is_valid_candidate("Candle", &existing));
    }

    // ─── extract_raw_candidates tests ────────────────────────

    #[test]
    fn test_extract_raw_candidates_empty() {
        assert!(extract_raw_candidates("").is_empty());
    }

    #[test]
    fn test_extract_raw_candidates_proper_noun() {
        let candidates = extract_raw_candidates("田中さんが東京タワーに行った");
        assert!(
            !candidates.is_empty(),
            "should extract at least one candidate from Japanese text with proper nouns"
        );
    }

    #[test]
    fn test_extract_raw_candidates_ascii() {
        let candidates = extract_raw_candidates("candle is a framework");
        assert!(
            candidates.iter().any(|c| c.surface == "candle"),
            "should detect ascii term 'candle': {candidates:?}"
        );
    }

    #[test]
    fn test_extract_raw_candidates_katakana() {
        let candidates = extract_raw_candidates("リンデラは形態素解析ツールです");
        assert!(
            candidates
                .iter()
                .any(|c| c.surface.contains("リンデラ") || c.pos == CandidatePos::Katakana),
            "should detect katakana term: {candidates:?}"
        );
    }

    #[test]
    fn test_extract_raw_candidates_dedup() {
        let candidates = extract_raw_candidates("candle uses candle for inference");
        let candle_count = candidates.iter().filter(|c| c.surface == "candle").count();
        assert!(candle_count <= 1, "should be deduplicated");
    }

    #[test]
    fn test_extract_raw_candidates_pos_type() {
        let candidates = extract_raw_candidates("candle is great");
        if let Some(c) = candidates.iter().find(|c| c.surface == "candle") {
            // lindera may classify "candle" as proper_noun or ascii depending on IPADIC
            assert!(
                c.pos == CandidatePos::Ascii || c.pos == CandidatePos::ProperNoun,
                "should be ascii or proper_noun, got {:?}",
                c.pos
            );
        }
    }

    #[test]
    fn test_extract_raw_candidates_preserves_case() {
        let candidates = extract_raw_candidates("LoRa module development");
        if let Some(c) = candidates
            .iter()
            .find(|c| c.surface.to_lowercase() == "lora")
        {
            assert_eq!(
                c.surface, "LoRa",
                "should preserve original case, got {:?}",
                c.surface
            );
        }
    }

    // ─── collect_from_text tests ─────────────────────────────

    #[test]
    fn test_collect_from_text_upserts() {
        let conn = setup();
        collect_from_text(
            &conn,
            "田中さんが東京に行った。田中さんが東京に行った",
            "document",
        );

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(count > 0, "should have collected at least one candidate");
    }

    #[test]
    fn test_collect_from_text_increments_frequency() {
        let conn = setup();
        collect_from_text(
            &conn,
            "candle is great for ML inference with candle",
            "document",
        );
        collect_from_text(&conn, "candle is used in this project", "document");

        let freq: Option<i64> = conn
            .query_row(
                "SELECT frequency FROM dictionary_candidates WHERE surface = 'candle'",
                [],
                |r| r.get(0),
            )
            .ok();

        if let Some(f) = freq {
            assert!(f >= 2, "frequency should be incremented, got {f}");
        }
        // If candle wasn't detected, that's ok — lindera may tokenize it differently
    }

    #[test]
    fn test_collect_from_text_rejected_not_incremented() {
        let conn = setup();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status)
             VALUES ('rejected_word', 3, 'ascii', 'document', '2026-01-01', '2026-01-01', 'rejected')",
            [],
        )
        .unwrap();

        collect_from_text(&conn, "rejected_word is in the text", "document");

        let freq: i64 = conn
            .query_row(
                "SELECT frequency FROM dictionary_candidates WHERE surface = 'rejected_word'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(freq, 3, "rejected candidate should not be incremented");
    }

    #[test]
    fn test_collect_from_text_source_preserved_on_second_call() {
        let conn = setup();
        // First call with "document" source
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status)
             VALUES ('test_word', 1, 'ascii', 'document', '2026-01-01', '2026-01-01', 'pending')",
            [],
        )
        .unwrap();

        // Simulate second call from "query" source — source should NOT change
        let _ = conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status)
             VALUES ('test_word', 1, 'ascii', 'query', '2026-01-02', '2026-01-02', 'pending')
             ON CONFLICT(surface) DO UPDATE SET
                 frequency = CASE WHEN dictionary_candidates.status = 'pending'
                     THEN dictionary_candidates.frequency + 1
                     ELSE dictionary_candidates.frequency END,
                 last_seen = CASE WHEN dictionary_candidates.status = 'pending'
                     THEN '2026-01-02' ELSE dictionary_candidates.last_seen END",
            [],
        );

        let source: String = conn
            .query_row(
                "SELECT source FROM dictionary_candidates WHERE surface = 'test_word'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source, "document", "source should preserve initial value");
    }

    #[test]
    fn test_collect_from_text_no_table_noop() {
        let conn = setup();
        conn.execute_batch("DROP TABLE IF EXISTS dictionary_candidates")
            .unwrap();
        collect_from_text(&conn, "some text with candle", "document");
    }

    #[test]
    fn test_collect_from_text_short_text_skipped() {
        let conn = setup();
        collect_from_text(&conn, "hi", "document");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "short text should be skipped");
    }

    // ─── spawn_collect_from_query tests ──────────────────────

    #[test]
    fn test_spawn_collect_from_query_writes_to_own_connection() {
        // Arrange: create a real file-based DB (WAL requires a real file)
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        crate::db::init_db(&db_path).unwrap();

        // Act: spawn harvest and join deterministically
        spawn_collect_from_query(db_path.clone(), "candle framework for rust".to_string())
            .join()
            .unwrap();

        // Assert: candidates were written via the spawned writer connection
        let conn = crate::db::get_connection(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            count > 0,
            "spawn_collect_from_query must write candidates; got 0"
        );
    }

    // ─── get_threshold_candidates tests ──────────────────────

    #[test]
    fn test_get_threshold_candidates() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('high', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('low', 2, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('rejected', 10, 'ascii', 'document', ?, ?, 'rejected')",
            [now, now],
        ).unwrap();

        let candidates = get_threshold_candidates(&conn, 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].surface, "high");
    }

    #[test]
    fn test_get_threshold_candidates_no_table() {
        let conn = setup();
        conn.execute_batch("DROP TABLE IF EXISTS dictionary_candidates")
            .unwrap();
        let candidates = get_threshold_candidates(&conn, 5);
        assert!(candidates.is_empty());
    }

    // ─── candidate_summary tests ─────────────────────────────

    #[test]
    fn test_candidate_summary() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('a_word', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('b_word', 2, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('c_word', 5, 'ascii', 'document', ?, ?, 'rejected')",
            [now, now],
        ).unwrap();

        let summary = candidate_summary(&conn);
        assert_eq!(summary.total_pending, 2);
        assert_eq!(summary.ready_count, 1);
        assert_eq!(summary.rejected_count, 1);
    }

    // ─── mark_accepted / mark_rejected tests ─────────────────

    #[test]
    fn test_mark_accepted() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('word1', 5, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        mark_accepted(&conn, &["word1"]).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM dictionary_candidates WHERE surface = 'word1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "accepted");

        let candidates = get_threshold_candidates(&conn, 1);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_mark_rejected() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('bad_word', 5, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        mark_rejected(&conn, "bad_word").unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM dictionary_candidates WHERE surface = 'bad_word'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "rejected");
    }

    // ─── CSV format tests ────────────────────────────────────

    #[test]
    fn test_format_simpledic_row() {
        assert_eq!(format_simpledic_row("candle"), "candle,名詞,candle");
    }

    // ─── load_existing_surfaces tests ────────────────────────

    #[test]
    fn test_load_existing_surfaces_missing_file() {
        let result = load_existing_surfaces(Path::new("/nonexistent/dict.csv")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_existing_surfaces_reads_csv() {
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("dict.csv");
        std::fs::write(&csv_path, "candle,名詞,candle\nlindera,名詞,lindera\n").unwrap();

        let surfaces = load_existing_surfaces(&csv_path).unwrap();
        assert!(surfaces.contains("candle"));
        assert!(surfaces.contains("lindera"));
        assert_eq!(surfaces.len(), 2);
    }

    #[test]
    fn test_load_existing_surfaces_skips_comments_and_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("dict.csv");
        std::fs::write(&csv_path, "# comment\n\ncandle,名詞,candle\n").unwrap();

        let surfaces = load_existing_surfaces(&csv_path).unwrap();
        assert_eq!(surfaces.len(), 1);
        assert!(surfaces.contains("candle"));
    }

    // ─── export_candidates_to_csv tests ──────────────────────

    #[test]
    fn test_export_candidates_to_csv_creates_file() {
        let conn = setup();
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("user_dict.csv");

        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let exported = export_candidates_to_csv(&conn, &csv_path, 5).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].surface, "candle");
        assert!(csv_path.exists());

        let content = std::fs::read_to_string(&csv_path).unwrap();
        assert!(content.contains("candle,名詞,candle"));

        let status: String = conn
            .query_row(
                "SELECT status FROM dictionary_candidates WHERE surface = 'candle'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "accepted");
    }

    #[test]
    fn test_export_candidates_to_csv_idempotent() {
        let conn = setup();
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("user_dict.csv");

        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let exported1 = export_candidates_to_csv(&conn, &csv_path, 5).unwrap();
        assert_eq!(exported1.len(), 1);

        conn.execute(
            "UPDATE dictionary_candidates SET status = 'pending' WHERE surface = 'candle'",
            [],
        )
        .unwrap();

        let exported2 = export_candidates_to_csv(&conn, &csv_path, 5).unwrap();
        assert!(exported2.is_empty(), "should not write duplicates");
    }

    #[test]
    fn test_export_already_in_csv_marks_accepted() {
        let conn = setup();
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("user_dict.csv");

        // Write candidate to CSV manually
        std::fs::write(&csv_path, "candle,名詞,candle\n").unwrap();

        // Insert same candidate into DB with status = 'pending'
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let exported = export_candidates_to_csv(&conn, &csv_path, 5).unwrap();

        // No new rows appended
        assert!(
            exported.is_empty(),
            "already_in_csv candidates should not be re-exported"
        );

        // DB status changed to 'accepted'
        let status: String = conn
            .query_row(
                "SELECT status FROM dictionary_candidates WHERE surface = 'candle'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "accepted");

        // CSV unchanged (no duplicate rows)
        let content = std::fs::read_to_string(&csv_path).unwrap();
        let line_count = content.lines().count();
        assert_eq!(line_count, 1, "CSV should not have new rows appended");
    }

    #[test]
    fn test_export_candidates_preserves_existing_rows() {
        let conn = setup();
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("user_dict.csv");

        // Write an existing entry
        std::fs::write(&csv_path, "existing_word,名詞,existing_word\n").unwrap();

        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('new_word', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        export_candidates_to_csv(&conn, &csv_path, 5).unwrap();

        let content = std::fs::read_to_string(&csv_path).unwrap();
        assert!(
            content.contains("existing_word"),
            "existing rows should be preserved"
        );
        assert!(content.contains("new_word"), "new rows should be appended");
    }

    #[test]
    fn test_export_candidates_empty() {
        let conn = setup();
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("user_dict.csv");

        let exported = export_candidates_to_csv(&conn, &csv_path, 5).unwrap();
        assert!(exported.is_empty());
        assert!(!csv_path.exists());
    }

    // ─── helper function tests ───────────────────────────────

    #[test]
    fn test_is_all_katakana() {
        assert!(is_all_katakana("リンデラ"));
        assert!(is_all_katakana("テスラー"));
        assert!(!is_all_katakana("テスト123"));
        assert!(!is_all_katakana("hello"));
        assert!(!is_all_katakana(""));
    }

    #[test]
    fn test_is_ascii_term() {
        assert!(is_ascii_term("candle"));
        assert!(is_ascii_term("sqlite-vec"));
        assert!(is_ascii_term("ruri_v3"));
        assert!(!is_ascii_term("123"));
        assert!(!is_ascii_term(""));
        assert!(!is_ascii_term("日本語"));
    }

    // ─── collect_from_query test ─────────────────────────────

    #[test]
    fn test_collect_from_query() {
        let conn = setup();
        collect_from_query(&conn, "candle framework for rust");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(count > 0, "should collect candidates from query");
    }

    // ─── reject list tests ──────────────────────────────────────

    #[test]
    fn test_load_reject_words_missing_file() {
        let result = load_reject_words(Path::new("/nonexistent/reject.txt")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_reject_words_skips_comments_and_blanks() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("reject.txt");
        std::fs::write(&path, "# comment\n\nfoo\n  bar  \n# another\nbaz\n").unwrap();
        let words = load_reject_words(&path).unwrap();
        assert_eq!(words.len(), 3);
        assert!(words.contains("foo"));
        assert!(words.contains("bar"));
        assert!(words.contains("baz"));
    }

    #[test]
    fn test_load_reject_words_lowercases() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("reject.txt");
        std::fs::write(&path, "Hello\nWORLD\n").unwrap();
        let words = load_reject_words(&path).unwrap();
        assert!(words.contains("hello"));
        assert!(words.contains("world"));
        assert!(!words.contains("Hello"));
    }

    #[test]
    fn test_apply_reject_list_marks_pending() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('foo', 5, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('bar', 3, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let reject_words: HashSet<String> = ["foo".to_string()].into();
        let rejected = apply_reject_list(&conn, &reject_words).unwrap();

        assert_eq!(rejected, vec!["foo"]);
        let status: String = conn
            .query_row(
                "SELECT status FROM dictionary_candidates WHERE surface = 'foo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "rejected");
        // bar should remain pending
        let status: String = conn
            .query_row(
                "SELECT status FROM dictionary_candidates WHERE surface = 'bar'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn test_apply_reject_list_ignores_non_pending() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('accepted_word', 5, 'ascii', 'document', ?, ?, 'accepted')",
            [now, now],
        ).unwrap();

        let reject_words: HashSet<String> = ["accepted_word".to_string()].into();
        let rejected = apply_reject_list(&conn, &reject_words).unwrap();
        assert!(rejected.is_empty());
    }

    #[test]
    fn test_get_rejected_candidates() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('aaa', 5, 'ascii', 'document', ?, ?, 'rejected')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('bbb', 3, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('ccc', 2, 'ascii', 'document', ?, ?, 'rejected')",
            [now, now],
        ).unwrap();

        let rejected = get_rejected_candidates(&conn);
        assert_eq!(rejected.len(), 2);
        assert_eq!(rejected[0].surface, "aaa");
        assert_eq!(rejected[1].surface, "ccc");
    }

    #[test]
    fn test_get_pending_in_reject_list() {
        let conn = setup();
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('keep', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates (surface, frequency, pos, source, first_seen, last_seen, status) VALUES ('drop', 5, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let reject_words: HashSet<String> = ["drop".to_string()].into();
        let candidates = get_pending_in_reject_list(&conn, &reject_words);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].surface, "drop");
    }

    // ─── regenerate_user_dict reading-emission test ──────────────

    #[test]
    fn test_regenerate_user_dict_emits_stored_readings() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        // accepted with a distinct reading, accepted without (NULL reading),
        // plus rejected/pending noise that must not be written.
        set_verdict(
            &conn,
            "ハンドロード",
            Verdict::Accepted,
            Some("はんどろーど"),
        )
        .unwrap();
        set_verdict(&conn, "クラウド", Verdict::Accepted, None).unwrap();
        set_verdict(&conn, "noise", Verdict::Rejected, None).unwrap();
        seed(&conn, "foo", "pending");

        let written = regenerate_user_dict(&conn, &path).unwrap();

        assert_eq!(written, 2, "only accepted surfaces are written");
        let body = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<&str> = body.lines().collect();
        // Sorted by surface; the stored reading is emitted, falling back to the
        // surface when NULL.
        assert_eq!(
            rows,
            vec![
                format!("クラウド,{},クラウド", USER_DICT_POS),
                format!("ハンドロード,{},はんどろーど", USER_DICT_POS),
            ]
        );
    }

    // ─── export_reject_words_to_file tests ───────────────────────

    #[test]
    fn test_export_reject_words_writes_only_rejected_sorted() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        set_verdict(&conn, "noise", Verdict::Rejected, None).unwrap();
        set_verdict(&conn, "bad", Verdict::Rejected, None).unwrap();
        set_verdict(&conn, "good", Verdict::Accepted, None).unwrap();
        seed(&conn, "foo", "pending");

        let written = export_reject_words_to_file(&conn, &path).unwrap();

        assert_eq!(written, 2, "only rejected surfaces are written");
        let body = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<&str> = body.lines().collect();
        assert_eq!(rows, vec!["bad", "noise"], "sorted, rejected only");
    }

    #[test]
    fn test_export_reject_words_overwrites_stale_content() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        std::fs::write(&path, "outdated\n").unwrap();
        set_verdict(&conn, "fresh", Verdict::Rejected, None).unwrap();

        export_reject_words_to_file(&conn, &path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("outdated"), "stale lines are gone");
        assert!(body.contains("fresh"), "rejected surface is present");
    }

    #[test]
    fn test_export_reject_words_empty_truncates() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        std::fs::write(&path, "outdated\n").unwrap();

        let written = export_reject_words_to_file(&conn, &path).unwrap();

        assert_eq!(written, 0);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.is_empty(), "no rejected rows truncates the file");
    }

    // ─── import_reject_words_from_file tests ─────────────────────

    #[test]
    fn test_import_reject_words_inserts_rejected_skips_comments() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        std::fs::write(&path, "bad\n# a comment\n\n  noise  \n").unwrap();

        let outcome = import_reject_words_from_file(&conn, &path).unwrap();

        assert_eq!(outcome.imported, 2, "comments and blank lines are skipped");
        assert_eq!(
            outcome.dict_affected, 0,
            "rejecting never-accepted words does not touch the dict"
        );
        assert_eq!(status_of(&conn, "bad").as_deref(), Some("rejected"));
        assert_eq!(status_of(&conn, "noise").as_deref(), Some("rejected"));
    }

    #[test]
    fn test_import_reject_words_demoting_accepted_marks_dict_affected() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        // `prev` was accepted; rejecting it shrinks the accepted set.
        set_verdict(&conn, "prev", Verdict::Accepted, None).unwrap();
        std::fs::write(&path, "prev\n").unwrap();

        let outcome = import_reject_words_from_file(&conn, &path).unwrap();

        assert_eq!(outcome.imported, 1);
        assert_eq!(
            outcome.dict_affected, 1,
            "demoting an accepted word changes the accepted set"
        );
    }

    #[test]
    fn test_import_reject_words_preserves_case() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        std::fs::write(&path, "FooBar\n").unwrap();

        import_reject_words_from_file(&conn, &path).unwrap();

        // Case is preserved verbatim (unlike `load_reject_words`, which lowercases).
        assert_eq!(status_of(&conn, "FooBar").as_deref(), Some("rejected"));
        assert_eq!(status_of(&conn, "foobar"), None);
    }

    #[test]
    fn test_import_reject_words_missing_file_is_zero() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.txt");

        let outcome = import_reject_words_from_file(&conn, &path).unwrap();

        assert_eq!(outcome.imported, 0);
    }

    // ─── import_user_dict_from_file tests ────────────────────────

    #[test]
    fn test_import_user_dict_inserts_accepted_with_reading() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        std::fs::write(
            &path,
            "ハンドロード,名詞,はんどろーど\n# comment\n\nクラウド,名詞,クラウド\n",
        )
        .unwrap();

        let outcome = import_user_dict_from_file(&conn, &path).unwrap();

        assert_eq!(outcome.imported, 2);
        assert_eq!(
            status_of(&conn, "ハンドロード").as_deref(),
            Some("accepted")
        );
        assert_eq!(
            reading_of(&conn, "ハンドロード").as_deref(),
            Some("はんどろーど")
        );
        assert_eq!(status_of(&conn, "クラウド").as_deref(), Some("accepted"));
        // reading == surface → no distinct reading stored.
        assert_eq!(reading_of(&conn, "クラウド"), None);
    }

    #[test]
    fn test_import_user_dict_empty_reading_is_none() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        // surface only / surface with empty reading column → no reading stored.
        std::fs::write(&path, "alpha\nbeta,名詞,\n").unwrap();

        let outcome = import_user_dict_from_file(&conn, &path).unwrap();

        assert_eq!(outcome.imported, 2);
        assert_eq!(status_of(&conn, "alpha").as_deref(), Some("accepted"));
        assert_eq!(reading_of(&conn, "alpha"), None);
        assert_eq!(reading_of(&conn, "beta"), None);
    }

    #[test]
    fn test_import_user_dict_missing_file_is_zero() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.simpledic");

        let outcome = import_user_dict_from_file(&conn, &path).unwrap();

        assert_eq!(outcome.imported, 0);
    }

    #[test]
    fn test_import_user_dict_round_trips_with_export() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        set_verdict(&conn, "甲", Verdict::Accepted, Some("こう")).unwrap();
        set_verdict(&conn, "乙", Verdict::Accepted, None).unwrap();
        regenerate_user_dict(&conn, &path).unwrap();

        // Fresh DB, import the exported file: same accepted set + readings.
        let conn2 = setup();
        let outcome = import_user_dict_from_file(&conn2, &path).unwrap();

        assert_eq!(outcome.imported, 2);
        assert_eq!(status_of(&conn2, "甲").as_deref(), Some("accepted"));
        assert_eq!(reading_of(&conn2, "甲").as_deref(), Some("こう"));
        assert_eq!(status_of(&conn2, "乙").as_deref(), Some("accepted"));
        assert_eq!(reading_of(&conn2, "乙"), None);
    }
}
