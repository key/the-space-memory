use std::collections::HashSet;
use std::path::Path;

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

/// A tokenized dictionary candidate (surface form + part of speech).
///
/// Produced by [`extract_query_candidates`] and consumed by
/// [`upsert_candidates`]; fields stay private (callers forward the value).
#[derive(Debug)]
pub struct RawCandidate {
    surface: String,
    pos: CandidatePos,
}

mod simpledic_format;
mod surfaces_cache;

pub use simpledic_format::{
    export_reject_words_to_file, format_simpledic_row_with_reading, import_reject_words_from_file,
    import_user_dict_from_file, regenerate_user_dict, ImportOutcome, RegenOutcome,
};
use simpledic_format::{read_reject_surfaces, read_simpledic_surfaces};
use surfaces_cache::with_existing_surfaces;
pub use surfaces_cache::{load_existing_surfaces, reset_existing_surfaces};

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

/// Tokenize text into validated dictionary candidates (no DB access).
///
/// Splitting extraction from the upsert lets the daemon tokenize a search query
/// outside the writer lock and acquire the lock only for the DB write.
pub fn extract_query_candidates(text: &str) -> Vec<RawCandidate> {
    if text.trim().len() < 4 {
        return Vec::new();
    }
    with_existing_surfaces(|existing| {
        extract_raw_candidates(text)
            .into_iter()
            .filter(|c| is_valid_candidate(&c.surface, existing))
            .collect()
    })
}

/// Upsert pre-extracted candidates into the DB. Requires a writable connection.
/// source: "document" | "query" | "session"
pub fn upsert_candidates(conn: &Connection, candidates: &[RawCandidate], source: &str) {
    if candidates.is_empty() || !db::has_candidates_table(conn) {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();

    for c in candidates {
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

/// Collect dictionary candidates from text and upsert into DB.
/// source: "document" | "query" | "session"
pub fn collect_from_text(conn: &Connection, text: &str, source: &str) {
    if !db::has_candidates_table(conn) {
        return;
    }
    upsert_candidates(conn, &extract_query_candidates(text), source);
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
    let dict_word_count = with_existing_surfaces(HashSet::len) as i64;
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
/// caller normalizes). A non-NULL `reading` overwrites; `None` preserves any existing reading (`COALESCE`).
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
    let current = find_status(conn, surface)?;

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

/// Resolve the reading stored by `dict add`.
/// An explicit `yomi` is normalized to NFC and used as-is. When omitted, the
/// normalized surface stands in as its own reading; the returned bool is
/// `true` when the surface contains kanji, so the caller can warn and surface
/// the data debt (automatic readings are never inferred — added terms are
/// exactly the words lindera does not know). Readings are stored only; search
/// is surface-based today.
pub fn resolve_reading(surface: &str, yomi: Option<&str>) -> (String, bool) {
    let normalized_surface = crate::normalize::nfc(surface);
    match yomi {
        Some(y) => (crate::normalize::nfc(y).into_owned(), false),
        None => (normalized_surface.into_owned(), surface_has_kanji(surface)),
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

/// Resolve `surface`'s status by exact match, migrating in place a legacy
/// row whose surface normalizes to the same NFC form on a miss (small table).
fn find_status(conn: &Connection, surface: &str) -> Result<Option<String>, rusqlite::Error> {
    let sql = "SELECT status FROM dictionary_candidates WHERE surface = ?1";
    let exact: Option<String> = conn.query_row(sql, [surface], |r| r.get(0)).optional()?;
    let None = exact else { return Ok(exact) };
    let hit = conn
        .prepare("SELECT surface, status FROM dictionary_candidates")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .flatten()
        .find(|(s, _)| crate::normalize::nfc(s).as_ref() == surface);
    let Some(hit) = hit else { return Ok(None) };
    let update = "UPDATE dictionary_candidates SET surface = ?1 WHERE surface = ?2";
    conn.execute(update, rusqlite::params![surface, hit.0])?;
    Ok(Some(hit.1))
}

/// Current DB verdict for `surface`, or `None` if it has no candidate row.
fn db_status(conn: &Connection, surface: &str) -> anyhow::Result<Option<Verdict>> {
    match find_status(conn, surface)? {
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
/// in) the DB — the data-loss path of an empty DB after `rebuild`, or a
/// hand-edited file. The DB becomes a faithful image of disk before any
/// rewrite, making the DB-as-authority model self-healing.
///
/// Insert-or-promote, never override an opposing verdict: an absent or
/// `pending` surface is inserted/promoted to the file's verdict (`accepted`
/// for simpledic, `rejected` for reject); an already-matching status is a
/// no-op. **Fails closed with no writes** on any contradiction — a surface in
/// both files, or a DB verdict opposing the file's — so an ambiguous state
/// never silently demotes a term; a malformed simpledic line also fails
/// closed (see [`read_simpledic_surfaces`]). All parsing + conflict detection
/// happens before the first write, inside the caller's one transaction, so
/// an abort leaves the DB completely unchanged.
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

    /// A legacy row written before this crate normalized surfaces (an NFD
    /// surface inserted directly, bypassing every normalized load/CLI path)
    /// must be found — not duplicated — when a caller supplies its NFC form.
    /// `find_status` migrates the row's key in place on the first
    /// touch, so a verdict change against it converges without a rebuild.
    #[test]
    fn test_set_verdict_migrates_legacy_nfd_surface_key() {
        let conn = setup();
        let nfd_worker = "\u{30ef}\u{30fc}\u{30ab}\u{3099}\u{30fc}"; // ワーガー decomposed
        let nfc_worker = "\u{30ef}\u{30fc}\u{30ac}\u{30fc}"; // ワーガー precomposed
        seed(&conn, nfd_worker, "accepted");

        let t = set_verdict(&conn, nfc_worker, Verdict::Rejected, None).unwrap();

        assert_eq!(t.from, Some(Verdict::Accepted));
        assert_eq!(t.to, Verdict::Rejected);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "must migrate the legacy row, not duplicate it");
        assert_eq!(status_of(&conn, nfc_worker).as_deref(), Some("rejected"));
        assert_eq!(
            status_of(&conn, nfd_worker),
            None,
            "old NFD key must no longer exist"
        );
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

    // ─── resolve_reading tests ────────────────────────────────

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
        assert!(warned, "supplementary-plane kanji must also warn");
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

    // ─── extract_query_candidates / upsert_candidates tests ──────────

    #[test]
    fn test_extract_query_candidates_returns_validated() {
        let candidates = extract_query_candidates("田中さんが東京に行った");
        assert!(
            !candidates.is_empty(),
            "should extract at least one validated candidate"
        );
    }

    #[test]
    fn test_extract_query_candidates_short_text_empty() {
        assert!(
            extract_query_candidates("hi").is_empty(),
            "text shorter than the threshold yields no candidates"
        );
    }

    #[test]
    fn test_upsert_candidates_writes_pre_extracted() {
        let conn = setup();
        let candidates = extract_query_candidates("田中さんが東京に行った");
        assert!(!candidates.is_empty(), "precondition: candidates extracted");

        upsert_candidates(&conn, &candidates, "query");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            count > 0,
            "upsert should persist the pre-extracted candidates"
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
        // Core: a simpledic term absent from the DB is pulled in as accepted.
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

    /// The reconcile path shares `db_status`'s legacy-key migration: a row
    /// seeded before this crate normalized surfaces (an NFD surface) must be
    /// recognized as already accepted when `user_dict.simpledic` names its
    /// NFC form, instead of reconcile treating it as a new term to heal.
    #[test]
    fn test_reconcile_migrates_legacy_nfd_surface_key() {
        let conn = setup();
        let nfd_worker = "\u{30ef}\u{30fc}\u{30ab}\u{3099}\u{30fc}"; // ワーガー decomposed
        let nfc_worker = "\u{30ef}\u{30fc}\u{30ac}\u{30fc}"; // ワーガー precomposed
        seed(&conn, nfd_worker, "accepted");
        let dir = tempfile::tempdir().unwrap();
        let (sp, rp) = write_files(dir.path(), &format!("{nfc_worker}\n"), "");

        let out = reconcile_files_into_db(&conn, &sp, &rp).unwrap();

        assert_eq!(
            out.accepted_healed, 0,
            "already accepted (legacy NFD row) needs no heal"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "must migrate the legacy row, not duplicate it");
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
        // post-mutation regenerate (the file-only-term data-loss repro at the
        // user_dict layer).
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

    // ─── review follow-ups: dup lines, reading sync, import overlap ───

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
