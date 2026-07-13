//! `user_dict.simpledic` / `reject_words.txt` file format: parsing, writing
//! (regenerate/export), and importing verdict files into the DB.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;

#[cfg(test)]
use super::set_verdict;
use super::{set_verdict_in, Verdict, USER_DICT_POS};

/// Format a simpledic row with an explicit reading: surface,{USER_DICT_POS},reading
pub fn format_simpledic_row_with_reading(surface: &str, reading: &str) -> String {
    format!("{surface},{},{reading}", USER_DICT_POS)
}

/// Regenerate `user_dict.simpledic` from the DB's accepted terms (full rewrite).
///
/// The shared primitive every verdict change relies on: a verdict change
/// regenerates simpledic and reloads the tokenizer. `export`
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
/// round-trip identically).
fn parse_simpledic_line(line: &str) -> anyhow::Result<Option<(String, Option<String>)>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let mut cols = trimmed.split(',');
    let surface = crate::normalize::nfc(cols.next().map(str::trim).unwrap_or("")).into_owned();
    if surface.is_empty() {
        anyhow::bail!("missing surface (first column is empty)");
    }
    let reading = cols
        .next() // pos column (ignored)
        .and(cols.next())
        .map(str::trim)
        .map(|r| crate::normalize::nfc(r).into_owned())
        .filter(|r| !r.is_empty() && *r != surface);
    Ok(Some((surface, reading)))
}

/// Read `user_dict.simpledic` into `(surface, reading)` rows, failing closed on
/// any malformed line (reported with its 1-based line number and path) so a
/// hand-edit typo never gets silently dropped by a later rewrite. A surface that
/// repeats with a *different* reading also fails closed (collapsing it would
/// silently drop one reading); an exact duplicate is harmless and deduped.
/// Missing file → empty vec.
pub(super) fn read_simpledic_surfaces(
    path: &Path,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
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
pub(super) fn read_reject_surfaces(path: &Path) -> anyhow::Result<Vec<String>> {
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
        out.push(crate::normalize::nfc(surface).into_owned());
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
/// via [`set_verdict`](super::set_verdict). Insert-only: words absent from the
/// file keep their verdict (use `dict rm`/`reject` to remove). Lines starting
/// with `#` and blank lines are skipped; case is preserved verbatim. A missing
/// file is a no-op.
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
/// with its reading via [`set_verdict`](super::set_verdict). Insert-only (see
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::setup_db as setup;

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

    /// Regression for the 2026-07-06 dict maintenance failure: a `reject_words.txt`
    /// entry saved in NFD must be found by an NFC-typed `dict reject` lookup —
    /// both go through the same `nfc()` normalization now.
    #[test]
    fn test_import_reject_words_nfd_matches_nfc_lookup() {
        let conn = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reject_words.txt");
        let nfd_worker = "\u{30ef}\u{30fc}\u{30ab}\u{3099}\u{30fc}"; // ワーガー decomposed
        std::fs::write(&path, format!("{nfd_worker}\n")).unwrap();

        import_reject_words_from_file(&conn, &path).unwrap();

        // The hand-typed NFC form must resolve to the same stored row.
        assert_eq!(
            status_of(&conn, "\u{30ef}\u{30fc}\u{30ac}\u{30fc}").as_deref(), // ワーガー precomposed
            Some("rejected")
        );
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

    /// An NFD-encoded surface in `user_dict.simpledic` must parse to its NFC
    /// form, matching the normalization applied everywhere else.
    #[test]
    fn test_parse_simpledic_line_normalizes_nfd_surface() {
        let nfd_worker = "\u{30ef}\u{30fc}\u{30ab}\u{3099}\u{30fc}"; // ワーガー decomposed
        let nfc_worker = "\u{30ef}\u{30fc}\u{30ac}\u{30fc}"; // ワーガー precomposed

        let parsed = parse_simpledic_line(nfd_worker).unwrap();

        assert_eq!(parsed, Some((nfc_worker.to_string(), None)));
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
}
