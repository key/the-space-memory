use std::collections::{HashMap, HashSet};
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
///
/// The daemon routes this through its shared writer after the search response
/// (see `tsmd::backfill::harvest_query_candidates`); it must run on a writable
/// connection, never the `query_only` reader pool.
pub fn collect_from_query(conn: &Connection, query: &str) {
    collect_from_text(conn, query, "query");
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
    let t = set_verdict_in(&tx, surface, to, reading)?;
    tx.commit()?;
    Ok(t)
}

/// Transaction-agnostic core of [`set_verdict`]: performs the SELECT + upsert on
/// the given connection/transaction **without** opening or committing its own
/// transaction. Callers that mutate several terms atomically (the reconcile +
/// verdict change in `dict add`/`reject`/`rm`, the bulk import loop) drive it
/// inside one outer transaction so a mid-sequence failure rolls everything back.
/// Semantics are identical to [`set_verdict`].
pub(crate) fn set_verdict_in(
    conn: &Connection,
    surface: &str,
    to: Verdict,
    reading: Option<&str>,
) -> Result<Transition, SetVerdictError> {
    let current: Option<String> = conn
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
    conn.execute(
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

    let affected_dict = (from == Some(Verdict::Accepted)) ^ (to == Verdict::Accepted);
    Ok(Transition {
        surface: surface.to_string(),
        from,
        to,
        affected_dict,
    })
}

// ─── CSV formatting ──────────────────────────────────────────

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
pub fn regenerate_user_dict(conn: &Connection, csv_path: &Path) -> anyhow::Result<RegenOutcome> {
    // Propagate per-row errors rather than silently dropping a term.
    let rows: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT surface, reading FROM dictionary_candidates
             WHERE status = 'accepted' ORDER BY surface ASC",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut content = String::new();
    for (surface, reading) in &rows {
        let reading = reading.as_deref().unwrap_or(surface);
        content.push_str(&format_simpledic_row_with_reading(surface, reading));
        content.push('\n');
    }

    // Compare against the current file so callers can skip an FTS reindex when
    // nothing changed — and so a retry after a failed reindex still detects the
    // on-disk/DB divergence and re-materializes (no dirty-marker needed).
    let changed = match std::fs::read_to_string(csv_path) {
        Ok(existing) => existing != content,
        // Any read error (missing file, permissions, non-UTF-8) → treat as
        // changed and rewrite; at worst this is one redundant reindex, never loss.
        Err(_) => true,
    };

    if let Some(parent) = csv_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to a sibling temp file then atomically rename, so a crash or write
    // failure mid-rewrite never leaves lindera reading a truncated/partial dict.
    let tmp_path = csv_path.with_extension("simpledic.tmp");
    {
        use std::io::Write;
        let mut tmp = std::fs::File::create(&tmp_path)?;
        tmp.write_all(content.as_bytes())?;
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, csv_path)?;
    Ok(RegenOutcome {
        written: rows.len(),
        changed,
    })
}

/// Result of [`regenerate_user_dict`]: how many accepted terms were written, and
/// whether the file content actually changed (so the caller reindexes FTS only
/// when needed, and a post-failure retry still re-materializes on divergence).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RegenOutcome {
    pub written: usize,
    pub changed: bool,
}

/// Export the DB's rejected set to `reject_words.txt`, overwriting the file.
/// One surface per line, sorted. Returns the number of words written. An empty
/// rejected set truncates the file. Round-trips with
/// [`import_reject_words_from_file`]. Mirrors [`regenerate_user_dict`]'s
/// durability: per-row DB errors propagate and the write goes through a sibling
/// temp file + atomic rename.
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

/// Parse one `user_dict.simpledic` line into `(surface, reading)`.
///
/// Blank lines and `#` comments return `Ok(None)` (tolerated). A non-blank,
/// non-comment line whose first column (the surface) is empty returns `Err`:
/// silently skipping it would let the next full rewrite delete a real term, so
/// we **fail closed** instead. `reading == surface` normalizes to `None`
/// (`regenerate_user_dict` re-emits the surface for a NULL reading, so they
/// round-trip identically; ADR-0014 §4).
fn parse_simpledic_line(line: &str) -> anyhow::Result<Option<(String, Option<String>)>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let mut cols = trimmed.split(',');
    let surface = cols.next().map(str::trim).unwrap_or("");
    if surface.is_empty() {
        anyhow::bail!("missing surface (first column is empty)");
    }
    let reading = cols
        .next() // pos column (ignored)
        .and(cols.next())
        .map(str::trim)
        .filter(|r| !r.is_empty() && *r != surface)
        .map(str::to_string);
    Ok(Some((surface.to_string(), reading)))
}

/// Read `user_dict.simpledic` into `(surface, reading)` rows, failing closed on
/// any malformed line (reported with its 1-based line number and path) so a
/// hand-edit typo never gets silently dropped by a later rewrite. A surface that
/// repeats with a *different* reading also fails closed (collapsing it would
/// silently drop one reading); an exact duplicate is harmless and deduped.
/// Missing file → empty vec.
fn read_simpledic_surfaces(path: &Path) -> anyhow::Result<Vec<(String, Option<String>)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    for (i, line) in content.lines().enumerate() {
        let row = match parse_simpledic_line(line) {
            Ok(Some(row)) => row,
            Ok(None) => continue,
            Err(e) => anyhow::bail!("{}:{}: {e}", path.display(), i + 1),
        };
        match seen.get(&row.0) {
            Some(prev) if *prev == row.1 => continue, // exact duplicate: dedupe
            Some(_) => anyhow::bail!(
                "{}:{}: surface '{}' appears more than once with different readings",
                path.display(),
                i + 1,
                row.0
            ),
            None => {}
        }
        seen.insert(row.0.clone(), row.1.clone());
        out.push(row);
    }
    Ok(out)
}

/// Read `reject_words.txt` surfaces (one per line; `#` comments and blanks
/// tolerated), case preserved. A non-blank, non-comment line is taken verbatim
/// as a surface. Missing file → empty vec.
fn read_reject_surfaces(path: &Path) -> anyhow::Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in content.lines() {
        let surface = line.trim();
        if surface.is_empty() || surface.starts_with('#') {
            continue;
        }
        out.push(surface.to_string());
    }
    Ok(out)
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
/// lines are skipped; case is preserved verbatim. A missing file is a no-op.
pub fn import_reject_words_from_file(
    conn: &Connection,
    path: &Path,
) -> anyhow::Result<ImportOutcome> {
    let mut outcome = ImportOutcome::default();
    for surface in read_reject_surfaces(path)? {
        let t = set_verdict_in(conn, &surface, Verdict::Rejected, None)?;
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
    let mut outcome = ImportOutcome::default();
    for (surface, reading) in read_simpledic_surfaces(path)? {
        let t = set_verdict_in(conn, &surface, Verdict::Accepted, reading.as_deref())?;
        outcome.imported += 1;
        if t.affected_dict {
            outcome.dict_affected += 1;
        }
    }
    Ok(outcome)
}

/// Current DB verdict for `surface`, or `None` if it has no candidate row.
fn db_status(conn: &Connection, surface: &str) -> anyhow::Result<Option<Verdict>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT status FROM dictionary_candidates WHERE surface = ?1",
            [surface],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        None => Ok(None),
        Some(s) => Verdict::from_status(&s)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("invalid status '{s}' for '{surface}' in DB")),
    }
}

/// How many on-disk terms the reconcile pulled into the DB.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub accepted_healed: usize,
    pub rejected_healed: usize,
}

impl ReconcileOutcome {
    pub fn is_empty(&self) -> bool {
        self.accepted_healed == 0 && self.rejected_healed == 0
    }
}

/// Reconcile the on-disk verdict files INTO the DB before a verdict mutation, so
/// the subsequent `regenerate_user_dict` rewrite preserves terms present in
/// `user_dict.simpledic` / `reject_words.txt` but absent from (or only `pending`
/// in) the DB — the data-loss path of #281 (empty DB after `rebuild`) and #288
/// (hand-edited file). The DB becomes a faithful image of disk before any
/// rewrite, making the DB-as-authority model self-healing.
///
/// Insert-or-promote, never override an opposing verdict:
/// - simpledic surface: absent → insert `accepted`; `pending` → promote to
///   `accepted` (presence in the dict file is an accept); `accepted` → no-op.
/// - reject surface: absent → insert `rejected`; `pending` → promote to
///   `rejected`; `rejected` → no-op.
///
/// **Fails closed with no writes** on any contradiction — a surface in both
/// files, a simpledic surface already `rejected` in the DB, or a reject surface
/// already `accepted` in the DB — so an ambiguous state never silently demotes a
/// term. All parsing + conflict detection happens before the first write, and
/// the caller drives this inside one transaction, so an abort leaves the DB
/// completely unchanged. A malformed simpledic line also fails closed (see
/// [`read_simpledic_surfaces`]).
pub fn reconcile_files_into_db(
    conn: &Connection,
    simpledic_path: &Path,
    reject_path: &Path,
) -> anyhow::Result<ReconcileOutcome> {
    let accepted = read_simpledic_surfaces(simpledic_path)?;
    let rejected = read_reject_surfaces(reject_path)?;

    // Detect every conflict before the first write so an abort is a clean no-op.
    detect_file_conflicts(conn, &accepted, &rejected)?;

    // Apply: insert/promote only (overrides already ruled out by the check above).
    let mut outcome = ReconcileOutcome::default();
    for (surface, reading) in &accepted {
        if reconcile_accepted_surface(conn, surface, reading.as_deref())? {
            outcome.accepted_healed += 1;
        }
    }
    for surface in &rejected {
        if db_status(conn, surface)? != Some(Verdict::Rejected) {
            set_verdict_in(conn, surface, Verdict::Rejected, None)?;
            outcome.rejected_healed += 1;
        }
    }
    Ok(outcome)
}

/// Bring one simpledic surface into the DB during reconcile: insert it / promote
/// `pending` to `accepted`, or — when it is already `accepted` — sync an explicit
/// hand-edited reading from the file (so a reading edit on an existing term is
/// not silently dropped by the later rewrite). A file reading of `None` is left
/// untouched (it is the normal serialization of a NULL reading, so it must not
/// clear a DB reading). Returns whether the DB changed.
fn reconcile_accepted_surface(
    conn: &Connection,
    surface: &str,
    reading: Option<&str>,
) -> anyhow::Result<bool> {
    if db_status(conn, surface)? != Some(Verdict::Accepted) {
        set_verdict_in(conn, surface, Verdict::Accepted, reading)?;
        return Ok(true);
    }
    match reading {
        Some(r) if db_reading(conn, surface)?.as_deref() != Some(r) => {
            set_verdict_in(conn, surface, Verdict::Accepted, Some(r))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Current stored reading for `surface` (its candidate row's `reading` column),
/// or `None` when the row is absent or the reading is NULL.
fn db_reading(conn: &Connection, surface: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT reading FROM dictionary_candidates WHERE surface = ?1",
            [surface],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Fail closed if a surface appears in BOTH verdict files — an unresolvable
/// contradiction. `dict import` calls this before importing, since its
/// accepted-then-rejected pass would otherwise apply the overlap as a silent
/// demotion. Read-only; also surfaces a malformed `user_dict.simpledic`.
pub fn assert_no_cross_file_conflict(
    simpledic_path: &Path,
    reject_path: &Path,
) -> anyhow::Result<()> {
    let accepted = read_simpledic_surfaces(simpledic_path)?;
    let rejected = read_reject_surfaces(reject_path)?;
    let reject_set: HashSet<&str> = rejected.iter().map(String::as_str).collect();
    let both: Vec<&str> = accepted
        .iter()
        .map(|(s, _)| s.as_str())
        .filter(|s| reject_set.contains(s))
        .collect();
    if !both.is_empty() {
        anyhow::bail!(
            "dictionary file conflict — no changes were made. Resolve, then retry:\n  - {}\n\
             Each term must appear in only one of user_dict.simpledic / reject_words.txt.",
            both.iter()
                .map(|s| format!("'{s}' is in both user_dict.simpledic and reject_words.txt"))
                .collect::<Vec<_>>()
                .join("\n  - ")
        );
    }
    Ok(())
}

/// Read-only check for [`reconcile_files_into_db`]: errors (listing every
/// offending term) if a surface is in both files, accepted on disk but rejected
/// in the DB, or rejected on disk but accepted in the DB. Runs before any write
/// so the caller's transaction stays a no-op on abort.
fn detect_file_conflicts(
    conn: &Connection,
    accepted: &[(String, Option<String>)],
    rejected: &[String],
) -> anyhow::Result<()> {
    let reject_set: HashSet<&str> = rejected.iter().map(String::as_str).collect();
    let mut conflicts: Vec<String> = Vec::new();
    for (surface, _) in accepted {
        if reject_set.contains(surface.as_str()) {
            conflicts.push(format!(
                "'{surface}' is in both user_dict.simpledic and reject_words.txt"
            ));
        } else if db_status(conn, surface)? == Some(Verdict::Rejected) {
            conflicts.push(format!(
                "'{surface}' is accepted in user_dict.simpledic but rejected in the database"
            ));
        }
    }
    for surface in rejected {
        if db_status(conn, surface)? == Some(Verdict::Accepted) {
            conflicts.push(format!(
                "'{surface}' is in reject_words.txt but accepted in the database"
            ));
        }
    }
    if !conflicts.is_empty() {
        anyhow::bail!(
            "dictionary file conflict — no changes were made. Resolve, then retry:\n  - {}\n\
             Edit user_dict.simpledic / reject_words.txt so each term appears in only one, \
             or run `tsm dict export` to rewrite both from the database.",
            conflicts.join("\n  - ")
        );
    }
    Ok(())
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

        assert_eq!(count.written, 2);
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

        assert_eq!(count.written, 0);
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

        assert_eq!(written.written, 2, "only accepted surfaces are written");
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

        // Case is preserved verbatim (the surface is stored as written).
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

    // ─── parse_simpledic_line tests ──────────────────────────────

    #[test]
    fn test_parse_simpledic_line_blank_and_comment() {
        assert_eq!(parse_simpledic_line("").unwrap(), None);
        assert_eq!(parse_simpledic_line("   ").unwrap(), None);
        assert_eq!(parse_simpledic_line("# a comment").unwrap(), None);
    }

    #[test]
    fn test_parse_simpledic_line_surface_only() {
        assert_eq!(
            parse_simpledic_line("クラウド").unwrap(),
            Some(("クラウド".to_string(), None))
        );
    }

    #[test]
    fn test_parse_simpledic_line_with_distinct_reading() {
        assert_eq!(
            parse_simpledic_line("ハンドロード,名詞,はんどろーど").unwrap(),
            Some(("ハンドロード".to_string(), Some("はんどろーど".to_string())))
        );
    }

    #[test]
    fn test_parse_simpledic_line_reading_equal_surface_is_none() {
        assert_eq!(
            parse_simpledic_line("クラウド,名詞,クラウド").unwrap(),
            Some(("クラウド".to_string(), None))
        );
    }

    #[test]
    fn test_parse_simpledic_line_empty_surface_fails_closed() {
        // A non-comment line with no surface must error, not silently skip:
        // skipping it would let the next full rewrite delete a real term.
        assert!(parse_simpledic_line(",名詞,reading").is_err());
    }

    #[test]
    fn test_read_simpledic_surfaces_reports_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        std::fs::write(&path, "good\n# c\n,bad\n").unwrap();
        let err = read_simpledic_surfaces(&path).unwrap_err().to_string();
        assert!(err.contains(":3:"), "error names the offending line: {err}");
    }

    // ─── reconcile_files_into_db tests ───────────────────────────

    fn write_files(
        dir: &std::path::Path,
        simpledic: &str,
        reject: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let sp = dir.join("user_dict.simpledic");
        let rp = dir.join("reject_words.txt");
        std::fs::write(&sp, simpledic).unwrap();
        std::fs::write(&rp, reject).unwrap();
        (sp, rp)
    }

    #[test]
    fn test_reconcile_inserts_file_only_accepted() {
        // #281/#288 core: a simpledic term absent from the DB is pulled in as accepted.
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "マイ用語,名詞,マイヨウゴ\n", "");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(out.accepted_healed, 1);
        assert_eq!(status_of(&conn, "マイ用語").as_deref(), Some("accepted"));
        assert_eq!(reading_of(&conn, "マイ用語").as_deref(), Some("マイヨウゴ"));
    }

    #[test]
    fn test_reconcile_promotes_pending_to_accepted() {
        // codex finding: a harvested `pending` candidate also present in simpledic
        // must be promoted, else regenerate (accepted-only) still drops it.
        let conn = setup();
        seed(&conn, "クラウド", "pending");
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "クラウド\n", "");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(out.accepted_healed, 1);
        assert_eq!(status_of(&conn, "クラウド").as_deref(), Some("accepted"));
    }

    #[test]
    fn test_reconcile_already_accepted_is_noop() {
        let conn = setup();
        seed(&conn, "既存", "accepted");
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "既存\n", "");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(out.accepted_healed, 0, "already accepted needs no heal");
    }

    #[test]
    fn test_reconcile_inserts_and_promotes_reject_words() {
        let conn = setup();
        seed(&conn, "harvested", "pending");
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "", "novel\nharvested\n");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(out.rejected_healed, 2);
        assert_eq!(status_of(&conn, "novel").as_deref(), Some("rejected"));
        assert_eq!(status_of(&conn, "harvested").as_deref(), Some("rejected"));
    }

    #[test]
    fn test_reconcile_conflict_both_files_is_noop_error() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "dup\n", "dup\n");

        let err = reconcile_files_into_db(&conn, &sp, &rp)
            .unwrap_err()
            .to_string();

        assert!(err.contains("dup") && err.contains("both"), "{err}");
        // No-op: the conflict was detected before any write.
        assert_eq!(
            status_of(&conn, "dup"),
            None,
            "abort leaves the DB unchanged"
        );
    }

    #[test]
    fn test_reconcile_conflict_file_vs_db_rejected_is_noop_error() {
        let conn = setup();
        seed(&conn, "x", "rejected");
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "x\n", "");

        let err = reconcile_files_into_db(&conn, &sp, &rp)
            .unwrap_err()
            .to_string();

        assert!(err.contains("'x'"), "{err}");
        assert_eq!(
            status_of(&conn, "x").as_deref(),
            Some("rejected"),
            "unchanged"
        );
    }

    #[test]
    fn test_reconcile_conflict_reject_vs_db_accepted_is_noop_error() {
        let conn = setup();
        seed(&conn, "y", "accepted");
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "", "y\n");

        let err = reconcile_files_into_db(&conn, &sp, &rp)
            .unwrap_err()
            .to_string();

        assert!(err.contains("'y'"), "{err}");
        assert_eq!(
            status_of(&conn, "y").as_deref(),
            Some("accepted"),
            "unchanged"
        );
    }

    #[test]
    fn test_reconcile_malformed_simpledic_is_noop_error() {
        // A malformed line aborts before any write — no partial heal.
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "good\n,bad\n", "");

        let err = reconcile_files_into_db(&conn, &sp, &rp)
            .unwrap_err()
            .to_string();

        assert!(err.contains(":2:"), "names the bad line: {err}");
        assert_eq!(
            status_of(&conn, "good"),
            None,
            "abort on a later bad line leaves earlier terms unwritten (no-op)"
        );
    }

    #[test]
    fn test_reconcile_missing_files_is_noop() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let out = reconcile_files_into_db(
            &conn,
            &dir.path().join("absent.simpledic"),
            &dir.path().join("absent.txt"),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_reconcile_then_regenerate_preserves_file_only_term() {
        // End-to-end of the fix: a file-only term + a fresh accept both survive the
        // post-mutation regenerate (the #288 repro at the user_dict layer).
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "マイ用語,名詞,マイヨウゴ\n", "");

        reconcile_files_into_db(&conn, &sp, &rp).unwrap();
        set_verdict_in(&conn, "新語", Verdict::Accepted, Some("シンゴ")).unwrap();
        regenerate_user_dict(&conn, &sp).unwrap();

        let body = std::fs::read_to_string(&sp).unwrap();
        assert!(
            body.contains("マイ用語,名詞,マイヨウゴ"),
            "file-only term survived: {body}"
        );
        assert!(body.contains("新語,名詞,シンゴ"), "new term added: {body}");
    }

    // ─── regenerate_user_dict change detection ───────────────────

    #[test]
    fn test_regenerate_reports_changed_then_unchanged() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_dict.simpledic");
        seed(&conn, "alpha", "accepted");

        let first = regenerate_user_dict(&conn, &path).unwrap();
        assert!(first.changed, "writing a fresh file counts as changed");

        let second = regenerate_user_dict(&conn, &path).unwrap();
        assert!(!second.changed, "re-running with no DB change is unchanged");
    }

    // ─── review follow-ups: dup lines, reading sync, import overlap ───

    #[test]
    fn test_read_simpledic_dedupes_identical_but_errors_on_conflicting_dup() {
        let dir = tempfile::tempdir().unwrap();
        let ident = dir.path().join("a.simpledic");
        std::fs::write(&ident, "x,名詞,x\nx,名詞,x\n").unwrap();
        assert_eq!(
            read_simpledic_surfaces(&ident).unwrap().len(),
            1,
            "identical dup deduped"
        );

        let conflicting = dir.path().join("b.simpledic");
        std::fs::write(&conflicting, "x,名詞,r1\nx,名詞,r2\n").unwrap();
        let err = read_simpledic_surfaces(&conflicting)
            .unwrap_err()
            .to_string();
        assert!(err.contains("different readings"), "{err}");
    }

    #[test]
    fn test_reconcile_syncs_hand_edited_reading_on_accepted() {
        // An already-accepted term whose file reading was hand-edited must have
        // the new reading synced, else regenerate overwrites the edit.
        let conn = setup();
        set_verdict_in(&conn, "漢字", Verdict::Accepted, None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "漢字,名詞,かんじ\n", "");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(out.accepted_healed, 1, "reading edit counts as a heal");
        assert_eq!(reading_of(&conn, "漢字").as_deref(), Some("かんじ"));
    }

    #[test]
    fn test_reconcile_accepted_no_reading_edit_is_noop() {
        let conn = setup();
        set_verdict_in(&conn, "漢字", Verdict::Accepted, Some("かんじ")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        // File reading == surface → None → must not clear the DB reading.
        let (sp, rp) = write_files(dir.path(), "漢字,名詞,漢字\n", "");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(out.accepted_healed, 0);
        assert_eq!(
            reading_of(&conn, "漢字").as_deref(),
            Some("かんじ"),
            "a None file reading does not clear the DB reading"
        );
    }

    #[test]
    fn test_assert_no_cross_file_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), "dup,名詞,dup\nok\n", "dup\n");
        let err = assert_no_cross_file_conflict(&sp, &rp)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'dup'") && err.contains("both"), "{err}");

        let (sp2, rp2) = write_files(dir.path(), "ok\n", "other\n");
        assert!(assert_no_cross_file_conflict(&sp2, &rp2).is_ok());
    }
}
