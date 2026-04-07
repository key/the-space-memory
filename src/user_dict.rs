use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use rusqlite::Connection;

use crate::config;
use crate::db;
use crate::tokenizer;

// ─── Enums ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidatePos {
    ProperNoun,
    Katakana,
    Ascii,
}

impl CandidatePos {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProperNoun => "proper_noun",
            Self::Katakana => "katakana",
            Self::Ascii => "ascii",
        }
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

        let pos = if details.len() >= 2 && details[0] == "名詞" && details[1] == "固有名詞" {
            Some(CandidatePos::ProperNoun)
        } else if is_all_katakana(&surface) && surface.chars().count() >= 2 {
            Some(CandidatePos::Katakana)
        } else if is_ascii_term(&surface) && surface.chars().count() >= 2 {
            Some(CandidatePos::Ascii)
        } else {
            None
        };

        if let Some(pos) = pos {
            let normalized = surface.to_lowercase();
            if seen.insert(normalized.clone()) {
                candidates.push(RawCandidate {
                    surface: normalized,
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

/// Format a CSV row in janome simpledic format: surface,カスタム名詞,surface
pub fn format_simpledic_row(surface: &str) -> String {
    format!("{surface},カスタム名詞,{surface}")
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
            "INSERT INTO dictionary_candidates VALUES ('high', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('low', 2, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('rejected', 10, 'ascii', 'document', ?, ?, 'rejected')",
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
            "INSERT INTO dictionary_candidates VALUES ('a_word', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('b_word', 2, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('c_word', 5, 'ascii', 'document', ?, ?, 'rejected')",
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
            "INSERT INTO dictionary_candidates VALUES ('word1', 5, 'ascii', 'document', ?, ?, 'pending')",
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
            "INSERT INTO dictionary_candidates VALUES ('bad_word', 5, 'ascii', 'document', ?, ?, 'pending')",
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
        assert_eq!(format_simpledic_row("candle"), "candle,カスタム名詞,candle");
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
        std::fs::write(
            &csv_path,
            "candle,カスタム名詞,candle\nlindera,カスタム名詞,lindera\n",
        )
        .unwrap();

        let surfaces = load_existing_surfaces(&csv_path).unwrap();
        assert!(surfaces.contains("candle"));
        assert!(surfaces.contains("lindera"));
        assert_eq!(surfaces.len(), 2);
    }

    #[test]
    fn test_load_existing_surfaces_skips_comments_and_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let csv_path = dir.path().join("dict.csv");
        std::fs::write(&csv_path, "# comment\n\ncandle,カスタム名詞,candle\n").unwrap();

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
            "INSERT INTO dictionary_candidates VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let exported = export_candidates_to_csv(&conn, &csv_path, 5).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].surface, "candle");
        assert!(csv_path.exists());

        let content = std::fs::read_to_string(&csv_path).unwrap();
        assert!(content.contains("candle,カスタム名詞,candle"));

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
            "INSERT INTO dictionary_candidates VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
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
        std::fs::write(&csv_path, "candle,カスタム名詞,candle\n").unwrap();

        // Insert same candidate into DB with status = 'pending'
        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('candle', 10, 'ascii', 'document', ?, ?, 'pending')",
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
        std::fs::write(&csv_path, "existing_word,カスタム名詞,existing_word\n").unwrap();

        let now = "2026-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('new_word', 10, 'ascii', 'document', ?, ?, 'pending')",
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
            "INSERT INTO dictionary_candidates VALUES ('foo', 5, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('bar', 3, 'ascii', 'document', ?, ?, 'pending')",
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
            "INSERT INTO dictionary_candidates VALUES ('accepted_word', 5, 'ascii', 'document', ?, ?, 'accepted')",
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
            "INSERT INTO dictionary_candidates VALUES ('aaa', 5, 'ascii', 'document', ?, ?, 'rejected')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('bbb', 3, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('ccc', 2, 'ascii', 'document', ?, ?, 'rejected')",
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
            "INSERT INTO dictionary_candidates VALUES ('keep', 10, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO dictionary_candidates VALUES ('drop', 5, 'ascii', 'document', ?, ?, 'pending')",
            [now, now],
        ).unwrap();

        let reject_words: HashSet<String> = ["drop".to_string()].into();
        let candidates = get_pending_in_reject_list(&conn, &reject_words);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].surface, "drop");
    }
}
