use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::db;
use crate::tokenizer::{self, wakachi};

const SCORE_CAP: f64 = 0.9;
const LEARN_SCORE: f64 = 0.05;
const STALE_DAYS: i64 = 180;

/// Half-life in days for different sources.
fn half_life(source: &str) -> Option<f64> {
    match source {
        "wordnet" | "user" => None, // No decay
        "feedback" => Some(90.0),
        "chat" => Some(60.0),
        _ => Some(90.0),
    }
}

/// Compute effective score with decay.
fn effective_score(
    base_score: f64,
    source: &str,
    last_hit: Option<&str>,
    created: Option<&str>,
) -> f64 {
    let hl = match half_life(source) {
        Some(h) => h,
        None => return base_score, // wordnet: no decay
    };

    // Decay from last_hit if available, otherwise from created
    let reference = last_hit
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .or_else(|| created.and_then(|s| s.parse::<DateTime<Utc>>().ok()))
        .unwrap_or_else(Utc::now);

    let days = (Utc::now() - reference).num_days().max(0) as f64;
    base_score * 0.5_f64.powf(days / hl)
}

/// Look up synonyms for a single word with decay applied.
/// Returns (synonym_word, effective_score) pairs sorted by score descending.
fn lookup_word(conn: &Connection, word: &str, max: usize, threshold: f64) -> Vec<(String, f64)> {
    let word = word.trim().to_lowercase();

    let sql = "SELECT word_b AS synonym, score, source, last_hit, created FROM synonyms WHERE word_a = ?
               UNION ALL
               SELECT word_a AS synonym, score, source, last_hit, created FROM synonyms WHERE word_b = ?
               ORDER BY score DESC";

    let all: Vec<(String, f64)> = conn
        .prepare(sql)
        .and_then(|mut stmt| {
            let rows = stmt.query_map(rusqlite::params![word, word], |row| {
                let synonym: String = row.get(0)?;
                let base: f64 = row.get(1)?;
                let source: String = row.get(2)?;
                let last_hit: Option<String> = row.get(3)?;
                let created: Option<String> = row.get(4)?;
                Ok((synonym, base, source, last_hit, created))
            })?;
            Ok(rows
                .filter_map(|r| r.ok())
                .map(|(syn, base, src, lh, cr)| {
                    let eff = effective_score(base, &src, lh.as_deref(), cr.as_deref());
                    (syn, eff)
                })
                .filter(|(_, s)| *s >= threshold)
                .collect())
        })
        .unwrap_or_default();

    let mut result: Vec<(String, f64)> = all;
    result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    result.truncate(max);
    result
}

/// Expand a query by looking up synonyms for each token.
/// Returns a flat list of expansion words (deduplicated, excluding original tokens).
pub fn expand_query_synonyms(
    conn: &Connection,
    query: &str,
    max_per_token: usize,
    threshold: f64,
) -> Vec<String> {
    if !db::has_synonyms_table(conn) {
        return Vec::new();
    }

    let wakachi_query = wakachi(query);
    let tokens: Vec<&str> = wakachi_query.split_whitespace().collect();
    if tokens.is_empty() {
        return Vec::new();
    }

    let token_set: std::collections::HashSet<String> =
        tokens.iter().map(|t| t.to_lowercase()).collect();

    let mut expansions = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for token in &tokens {
        let synonyms = lookup_word(conn, token, max_per_token, threshold);
        for (word, _score) in synonyms {
            if !token_set.contains(&word) && seen.insert(word.clone()) {
                expansions.push(word);
            }
        }
    }

    expansions
}

/// Normalize and order a word pair: trim, lowercase, then sort so the result is
/// `(lo, hi)` with `lo <= hi`. Shared by every site that stores or looks up a
/// pair so the normalization rule lives in one place.
fn normalize_pair(a: &str, b: &str) -> (String, String) {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Upsert a synonym pair into the table.
/// Words are normalized (lowercase, trimmed) and ordered (word_a < word_b).
pub fn upsert_synonym(
    conn: &Connection,
    word_a: &str,
    word_b: &str,
    score: f64,
    source: &str,
) -> anyhow::Result<()> {
    if !db::has_synonyms_table(conn) {
        return Ok(());
    }

    // `lo` is the smaller of the two, so an empty input always lands in `lo`.
    let (lo, hi) = normalize_pair(word_a, word_b);
    if lo == hi || lo.is_empty() {
        return Ok(());
    }

    let score = score.min(SCORE_CAP);
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
         VALUES (?, ?, ?, ?, 0, ?)
         ON CONFLICT(word_a, word_b) DO UPDATE SET
             score = MAX(synonyms.score, excluded.score),
             source = CASE WHEN excluded.score > synonyms.score THEN excluded.source ELSE synonyms.source END",
        rusqlite::params![lo, hi, score, source, now],
    )?;
    Ok(())
}

/// Progress callback type for import_wordnet: (imported_so_far, total).
pub type WordnetProgressCb<'a> = &'a dyn Fn(usize, usize);

/// Import synonym pairs from a Japanese WordNet SQLite database.
/// Extracts pairs of Japanese words that share a synset.
pub fn import_wordnet(
    conn: &Connection,
    wordnet_path: &std::path::Path,
    progress_cb: Option<WordnetProgressCb<'_>>,
) -> anyhow::Result<usize> {
    if !db::has_synonyms_table(conn) {
        anyhow::bail!("synonyms table not found");
    }
    if !wordnet_path.exists() {
        anyhow::bail!("WordNet DB not found: {}", wordnet_path.display());
    }

    let wn = rusqlite::Connection::open(wordnet_path)?;
    let now = chrono::Utc::now().to_rfc3339();

    let mut stmt = wn.prepare(
        "SELECT DISTINCT
            CASE WHEN w1.lemma < w2.lemma THEN w1.lemma ELSE w2.lemma END,
            CASE WHEN w1.lemma < w2.lemma THEN w2.lemma ELSE w1.lemma END
         FROM sense s1
         JOIN sense s2 ON s1.synset = s2.synset AND s1.wordid < s2.wordid
         JOIN word w1 ON s1.wordid = w1.wordid AND w1.lang = 'jpn'
         JOIN word w2 ON s2.wordid = w2.wordid AND w2.lang = 'jpn'
         WHERE w1.lemma != w2.lemma
           AND length(w1.lemma) >= 2
           AND length(w2.lemma) >= 2",
    )?;

    let pairs: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let total = pairs.len();
    log::info!("importing {total} synonym pairs from WordNet...");

    let batch_size = 1000;
    let mut imported = 0;

    for chunk in pairs.chunks(batch_size) {
        let tx = conn.unchecked_transaction()?;
        for (a, b) in chunk {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO synonyms (word_a, word_b, score, source, hits, created)
                 VALUES (?, ?, 0.5, 'wordnet', 0, ?)",
                rusqlite::params![a, b, now],
            );
        }
        tx.commit()?;
        imported += chunk.len();
        if let Some(cb) = progress_cb {
            cb(imported, total);
        }
    }
    log::info!("{imported}/{total} synonym pairs imported.");
    Ok(imported)
}

const USER_SCORE: f64 = 0.7;

/// Result of a user synonym import operation.
#[derive(Debug)]
pub struct ImportResult {
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub total: usize,
}

/// Parse a synonyms CSV file into normalized pairs. Returns (pairs, skipped_count).
fn parse_synonym_csv(content: &str) -> (HashSet<(String, String)>, usize) {
    let mut pairs = HashSet::new();
    let mut skipped = 0;
    for (line_no, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, ',').collect();
        if parts.len() != 2 {
            log::warn!(
                "synonyms.csv line {}: skipping malformed line: {:?}",
                line_no + 1,
                raw_line
            );
            skipped += 1;
            continue;
        }
        let a = parts[0].trim().to_lowercase();
        let b = parts[1].trim().to_lowercase();
        if a.is_empty() || b.is_empty() || a == b {
            log::warn!(
                "synonyms.csv line {}: skipping invalid pair: {:?}",
                line_no + 1,
                raw_line
            );
            skipped += 1;
            continue;
        }
        let pair = if a < b { (a, b) } else { (b, a) };
        pairs.insert(pair);
    }
    (pairs, skipped)
}

/// Import user-defined synonym pairs from CSV text (source -> DB).
/// Always inserts pairs present in the input (`INSERT OR IGNORE`, so pairs from
/// other sources like wordnet are left untouched). When `mirror` is true, also
/// deletes any `source = 'user'` pairs absent from the input — making the user
/// subset exactly match the input, the inverse of [`export_user_synonyms`].
///
/// `mirror` callers (`tsm synonym import`) get the exact round-trip but risk a
/// mass delete: if the input parses to zero pairs (empty or all-malformed) while
/// user pairs exist, this bails instead of wiping them. `mirror = false`
/// callers (`tsm init`) are insert-only and never delete, so re-running them is
/// non-destructive. The caller owns reading from stdin or a file.
pub fn import_user_synonyms(
    conn: &Connection,
    content: &str,
    mirror: bool,
) -> anyhow::Result<ImportResult> {
    if !db::has_synonyms_table(conn) {
        anyhow::bail!("synonyms table not found");
    }

    let (file_pairs, skipped) = parse_synonym_csv(content);

    // Guard the destructive case: an empty/all-malformed input under mirror would
    // delete every user pair. Refuse rather than silently wipe.
    if mirror && file_pairs.is_empty() {
        let existing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM synonyms WHERE source = 'user'",
            [],
            |r| r.get(0),
        )?;
        if existing > 0 {
            anyhow::bail!(
                "input has no synonym pairs; refusing to delete all {existing} user \
                 synonym(s). Provide pairs, or remove them explicitly with `tsm synonym rm`."
            );
        }
    }

    let tx = conn.unchecked_transaction()?;

    // Insert pairs from the input. Use INSERT OR IGNORE to avoid overwriting
    // existing pairs from other sources (e.g. wordnet).
    let now = chrono::Utc::now().to_rfc3339();
    let mut upserted = 0;
    for (a, b) in &file_pairs {
        upserted += conn.execute(
            "INSERT OR IGNORE INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES (?, ?, ?, 'user', 0, ?)",
            rusqlite::params![a, b, USER_SCORE, now],
        )?;
    }

    let deleted = if mirror {
        delete_stale_user_pairs(conn, &file_pairs)?
    } else {
        0
    };

    tx.commit()?;

    Ok(ImportResult {
        upserted,
        deleted,
        skipped,
        total: file_pairs.len(),
    })
}

/// Delete source='user' pairs from DB that are not in the given set.
fn delete_stale_user_pairs(
    conn: &Connection,
    keep: &HashSet<(String, String)>,
) -> anyhow::Result<usize> {
    let mut stmt = conn.prepare("SELECT word_a, word_b FROM synonyms WHERE source = 'user'")?;
    let db_pairs: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut deleted = 0;
    for (a, b) in &db_pairs {
        if !keep.contains(&(a.clone(), b.clone())) {
            conn.execute(
                "DELETE FROM synonyms WHERE word_a = ? AND word_b = ? AND source = 'user'",
                rusqlite::params![a, b],
            )?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Add a single user-defined synonym pair (source = 'user', [`USER_SCORE`]).
/// Normalizes and orders the words like [`upsert_synonym`]. Bails on empty or
/// identical words so the CLI can surface the error. Returns the normalized
/// `(lo, hi)` pair as actually stored, so the caller can echo what it persisted.
///
/// `add` asserts user ownership: if the pair already exists under another source
/// (e.g. wordnet), it is claimed as `source = 'user'` while keeping the higher
/// score. Without this, a non-user pair scored at or above `USER_SCORE` would
/// stay non-user and become invisible to `export` / `rm`.
pub fn add_user_synonym(conn: &Connection, a: &str, b: &str) -> anyhow::Result<(String, String)> {
    if !db::has_synonyms_table(conn) {
        anyhow::bail!("synonyms table not found");
    }
    let (lo, hi) = normalize_pair(a, b);
    if lo.is_empty() {
        anyhow::bail!("synonym words must not be empty");
    }
    if lo == hi {
        anyhow::bail!("cannot add a word as its own synonym: {lo}");
    }
    upsert_synonym(conn, &lo, &hi, USER_SCORE, "user")?;
    conn.execute(
        "UPDATE synonyms SET source = 'user' WHERE word_a = ? AND word_b = ?",
        rusqlite::params![lo, hi],
    )?;
    Ok((lo, hi))
}

/// Outcome of [`remove_user_synonym`].
pub struct RemoveResult {
    /// Number of `source = 'user'` rows deleted.
    pub removed: usize,
    /// Matching rows left intact because they are not `source = 'user'`
    /// (e.g. wordnet/learned). Only meaningful when `removed == 0`.
    pub skipped_non_user: usize,
}

/// Remove user-defined synonym pair(s) from the DB.
/// With `b = Some`, deletes the single normalized pair `(a, b)`; with `b = None`,
/// deletes every user pair involving `a`. Only `source = 'user'` rows are
/// deleted; wordnet/learned rows are reported via `skipped_non_user` and left
/// intact. Bails on empty input.
pub fn remove_user_synonym(
    conn: &Connection,
    a: &str,
    b: Option<&str>,
) -> anyhow::Result<RemoveResult> {
    if !db::has_synonyms_table(conn) {
        anyhow::bail!("synonyms table not found");
    }
    let na = a.trim().to_lowercase();
    if na.is_empty() {
        anyhow::bail!("synonym word must not be empty");
    }

    // Each arm picks the DELETE/COUNT pair that matches its scope; both bind two
    // string params, so the delete-then-count-on-miss tail is shared below.
    let (del_sql, count_sql, p1, p2) = match b {
        Some(b) => {
            let nb = b.trim().to_lowercase();
            if nb.is_empty() {
                anyhow::bail!("synonym word must not be empty");
            }
            let (lo, hi) = normalize_pair(&na, &nb);
            (
                "DELETE FROM synonyms WHERE word_a = ? AND word_b = ? AND source = 'user'",
                "SELECT COUNT(*) FROM synonyms WHERE word_a = ? AND word_b = ? AND source != 'user'",
                lo,
                hi,
            )
        }
        None => (
            "DELETE FROM synonyms WHERE (word_a = ? OR word_b = ?) AND source = 'user'",
            "SELECT COUNT(*) FROM synonyms WHERE (word_a = ? OR word_b = ?) AND source != 'user'",
            na.clone(),
            na,
        ),
    };

    let removed = conn.execute(del_sql, rusqlite::params![p1, p2])?;
    let skipped_non_user = if removed == 0 {
        count_non_user(conn, count_sql, &p1, &p2)?
    } else {
        0
    };

    Ok(RemoveResult {
        removed,
        skipped_non_user,
    })
}

/// Count rows matching a two-param predicate whose `source` is not `'user'`.
/// `sql` must be a `SELECT COUNT(*)` with two `?` placeholders. Propagates query
/// errors so a failed count is not misreported as "no match".
fn count_non_user(conn: &Connection, sql: &str, p1: &str, p2: &str) -> anyhow::Result<usize> {
    let n: i64 = conn.query_row(sql, rusqlite::params![p1, p2], |r| r.get(0))?;
    Ok(n as usize)
}

/// Export user-defined synonym pairs as CSV to a writer (DB -> sink).
/// Writes only `source = 'user'` pairs as `a,b` lines (already normalized and
/// ordered), sorted for stable, diffable output, under a header comment.
/// Inverse of [`import_user_synonyms`]. The caller owns the sink (stdout or a
/// file). Returns the number of pairs written.
pub fn export_user_synonyms(
    conn: &Connection,
    out: &mut impl std::io::Write,
) -> anyhow::Result<usize> {
    if !db::has_synonyms_table(conn) {
        anyhow::bail!("synonyms table not found");
    }

    let mut stmt = conn.prepare(
        "SELECT word_a, word_b FROM synonyms WHERE source = 'user' ORDER BY word_a, word_b",
    )?;
    let pairs: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    writeln!(
        out,
        "# User-defined synonym pairs (a,b). Managed by `tsm synonym`."
    )?;
    for (a, b) in &pairs {
        writeln!(out, "{a},{b}")?;
    }
    Ok(pairs.len())
}

/// Record a hit on a synonym pair (increments hits, updates last_hit).
pub fn record_hit(conn: &Connection, word_a: &str, word_b: &str) {
    let (lo, hi) = normalize_pair(word_a, word_b);
    let now = chrono::Utc::now().to_rfc3339();

    let _ = conn.execute(
        "UPDATE synonyms SET hits = hits + 1, last_hit = ? WHERE word_a = ? AND word_b = ?",
        rusqlite::params![now, lo, hi],
    );
}

/// Learn synonym pairs from a human message.
/// Extracts nouns via morphological analysis and creates pairs within the message.
pub fn learn_from_message(conn: &Connection, message: &str, source: &str) {
    if !db::has_synonyms_table(conn) {
        return;
    }
    if message.trim().len() < 4 {
        return;
    }

    // Filter to nouns (2+ chars) — use lindera POS info
    let mut nouns: Vec<String> = {
        use std::borrow::Cow;
        let segmenter = tokenizer::get_segmenter();
        let mut segmenter_tokens = segmenter
            .segment(Cow::Borrowed(message))
            .unwrap_or_default();
        let mut result = Vec::new();
        for t in &mut segmenter_tokens {
            let surface = t.surface.as_ref().to_string();
            let details = t.details();
            if details.len() >= 2
                && details[0] == crate::tokenizer::POS_NOUN
                && surface.chars().count() >= 2
                && !surface.chars().all(|c| c.is_ascii_digit())
            {
                result.push(surface.to_lowercase());
            }
        }
        result
    };

    if nouns.len() < 2 {
        return;
    }

    // Cap to prevent O(N²) explosion and reduce noise from distant nouns
    const MAX_NOUNS: usize = 30;
    nouns.truncate(MAX_NOUNS);

    // Generate all noun pairs within the message
    let mut seen = HashSet::new();
    for i in 0..nouns.len() {
        for j in (i + 1)..nouns.len() {
            if nouns[i] != nouns[j] {
                let pair = if nouns[i] < nouns[j] {
                    (nouns[i].clone(), nouns[j].clone())
                } else {
                    (nouns[j].clone(), nouns[i].clone())
                };
                if seen.insert(pair.clone()) {
                    let _ = upsert_synonym(conn, &pair.0, &pair.1, LEARN_SCORE, source);
                }
            }
        }
    }
}

/// Delete stale synonym pairs (hits=0, older than STALE_DAYS).
/// Designed to be called from a background thread.
pub fn cleanup_stale(conn: &Connection) {
    if !db::has_synonyms_table(conn) {
        return;
    }

    let threshold = (Utc::now() - chrono::Duration::days(STALE_DAYS)).to_rfc3339();

    let deleted = conn
        .execute(
            "DELETE FROM synonyms WHERE hits = 0 AND source NOT IN ('wordnet', 'user') AND created < ?",
            rusqlite::params![threshold],
        )
        .unwrap_or(0);

    if deleted > 0 {
        log::info!("cleaned up {deleted} stale synonym pairs");
    }
}

/// Global flag to ensure cleanup runs at most once per process.
static CLEANUP_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Spawn a background cleanup thread (runs at most once per process).
pub fn maybe_spawn_cleanup(db_path: std::path::PathBuf) {
    if CLEANUP_SPAWNED.swap(true, Ordering::SeqCst) {
        return; // Already spawned
    }

    std::thread::spawn(move || {
        if let Ok(conn) = db::get_connection(&db_path) {
            cleanup_stale(&conn);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::setup_db as setup;

    #[test]
    fn test_upsert_synonym() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Check ordering (word_a < word_b)
        let (a, b): (String, String) = conn
            .query_row("SELECT word_a, word_b FROM synonyms", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(a < b);
    }

    #[test]
    fn test_upsert_synonym_idempotent() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "feedback").unwrap();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();

        let (score, source): (f64, String) = conn
            .query_row("SELECT score, source FROM synonyms", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(score, 0.7); // MAX(0.5, 0.7)
        assert_eq!(source, "wordnet");
    }

    #[test]
    fn test_upsert_synonym_self_pair_ignored() {
        let conn = setup();
        upsert_synonym(&conn, "rust", "rust", 1.0, "wordnet").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_upsert_synonym_normalized() {
        let conn = setup();
        upsert_synonym(&conn, "  Rust  ", "SQLITE", 0.5, "feedback").unwrap();

        let (a, b): (String, String) = conn
            .query_row("SELECT word_a, word_b FROM synonyms", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(a, "rust");
        assert_eq!(b, "sqlite");
    }

    #[test]
    fn test_upsert_synonym_empty_arg_ignored() {
        let conn = setup();
        // An empty arg always sorts into `lo`, so the `lo.is_empty()` guard
        // rejects it. Verifies the normalize_pair refactor preserved the guard.
        upsert_synonym(&conn, "", "word", 0.5, "feedback").unwrap();
        upsert_synonym(&conn, "word", "  ", 0.5, "feedback").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "empty-arg pairs must not be inserted");
    }

    #[test]
    fn test_lookup_word() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();
        upsert_synonym(&conn, "猟", "銃猟", 0.5, "wordnet").unwrap();
        upsert_synonym(&conn, "猟", "低スコア", 0.1, "feedback").unwrap();

        // threshold 0.3 should exclude "低スコア"
        let results = lookup_word(&conn, "猟", 10, 0.3);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "狩猟"); // highest score first
        assert_eq!(results[1].0, "銃猟");
    }

    #[test]
    fn test_lookup_word_bidirectional() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();

        // Lookup from either direction
        let from_a = lookup_word(&conn, "猟", 10, 0.0);
        let from_b = lookup_word(&conn, "狩猟", 10, 0.0);
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_a[0].0, "狩猟");
        assert_eq!(from_b[0].0, "猟");
    }

    #[test]
    fn test_expand_query_synonyms() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();
        upsert_synonym(&conn, "射撃", "銃砲", 0.6, "wordnet").unwrap();

        let expansions = expand_query_synonyms(&conn, "猟 射撃", 3, 0.3);
        assert!(expansions.contains(&"狩猟".to_string()));
        assert!(expansions.contains(&"銃砲".to_string()));
    }

    #[test]
    fn test_expand_query_synonyms_no_self() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();

        let expansions = expand_query_synonyms(&conn, "猟", 3, 0.3);
        assert!(expansions.contains(&"狩猟".to_string()));
        assert!(!expansions.contains(&"猟".to_string()));
    }

    #[test]
    fn test_expand_query_synonyms_empty() {
        let conn = setup();
        let expansions = expand_query_synonyms(&conn, "", 3, 0.3);
        assert!(expansions.is_empty());
    }

    #[test]
    fn test_expand_query_synonyms_no_table() {
        let conn = setup();
        conn.execute_batch("DROP TABLE IF EXISTS synonyms;")
            .unwrap();

        let expansions = expand_query_synonyms(&conn, "猟", 3, 0.3);
        assert!(expansions.is_empty());
    }

    #[test]
    fn test_record_hit() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();

        record_hit(&conn, "猟", "狩猟");
        record_hit(&conn, "狩猟", "猟"); // reverse order should work too

        let hits: i64 = conn
            .query_row("SELECT hits FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 2);
    }

    #[test]
    fn test_expand_query_dedup() {
        let conn = setup();
        // Both tokens map to the same synonym
        upsert_synonym(&conn, "猟", "狩猟", 0.7, "wordnet").unwrap();
        upsert_synonym(&conn, "銃猟", "狩猟", 0.6, "wordnet").unwrap();

        let expansions = expand_query_synonyms(&conn, "猟 銃猟", 3, 0.3);
        let count = expansions.iter().filter(|e| *e == "狩猟").count();
        assert_eq!(count, 1, "should be deduplicated");
    }

    // ─── feedback learning tests ─────────────────────────────

    #[test]
    fn test_learn_from_message() {
        let conn = setup();
        learn_from_message(&conn, "鉄砲屋で事業承継の相談をした", "chat");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "should have learned some pairs");

        // All learned pairs should have source='chat' and low score
        let max_score: f64 = conn
            .query_row("SELECT MAX(score) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert!(max_score <= SCORE_CAP);
    }

    #[test]
    fn test_learn_from_message_short() {
        let conn = setup();
        learn_from_message(&conn, "hi", "chat");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM synonyms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "short messages should be ignored");
    }

    #[test]
    fn test_learn_from_message_no_table() {
        let conn = setup();
        conn.execute_batch("DROP TABLE IF EXISTS synonyms;")
            .unwrap();
        // Should not panic
        learn_from_message(&conn, "鉄砲屋で事業承継の相談をした", "chat");
    }

    #[test]
    fn test_cleanup_stale() {
        let conn = setup();
        // Insert a stale pair (old date, hits=0)
        conn.execute(
            "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES ('old_a', 'old_b', 0.1, 'feedback', 0, '2025-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        // Insert a fresh pair
        upsert_synonym(&conn, "fresh_a", "fresh_b", 0.5, "feedback").unwrap();

        cleanup_stale(&conn);

        // Old pair should be deleted
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE word_a = 'old_a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0, "stale pair should be deleted");

        // Fresh pair should remain
        let fresh: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE word_a = 'fresh_a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fresh, 1, "fresh pair should remain");
    }

    #[test]
    fn test_cleanup_preserves_wordnet() {
        let conn = setup();
        // WordNet pairs should not be deleted even if old + no hits
        conn.execute(
            "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES ('wn_a', 'wn_b', 0.5, 'wordnet', 0, '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        cleanup_stale(&conn);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE word_a = 'wn_a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "wordnet pairs should not be cleaned");
    }

    #[test]
    fn test_effective_score_wordnet_no_decay() {
        let score = effective_score(0.5, "wordnet", Some("2020-01-01T00:00:00Z"), None);
        assert_eq!(score, 0.5, "wordnet should not decay");
    }

    #[test]
    fn test_effective_score_feedback_decays() {
        let old_date = "2020-01-01T00:00:00Z";
        let score = effective_score(0.5, "feedback", Some(old_date), None);
        assert!(score < 0.5, "old feedback should decay");
        assert!(score > 0.0, "should not decay to zero");
    }

    #[test]
    fn test_effective_score_recent_minimal_decay() {
        let recent = chrono::Utc::now().to_rfc3339();
        let score = effective_score(0.5, "feedback", Some(&recent), None);
        assert!(score > 0.49, "recent feedback should barely decay");
    }

    #[test]
    fn test_effective_score_no_hit_decays_from_created() {
        // No last_hit but old created → should decay
        let score = effective_score(0.5, "chat", None, Some("2020-01-01T00:00:00Z"));
        assert!(
            score < 0.1,
            "never-hit entry should decay from creation date"
        );
    }

    /// Create a minimal WordNet-schema SQLite DB with the given pairs.
    fn create_mock_wordnet(pairs: &[(&str, &str)]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let wn = rusqlite::Connection::open(file.path()).unwrap();
        wn.execute_batch(
            "CREATE TABLE word (wordid INTEGER PRIMARY KEY, lemma TEXT, lang TEXT);
             CREATE TABLE synset (synset TEXT PRIMARY KEY);
             CREATE TABLE sense (synset TEXT, wordid INTEGER);",
        )
        .unwrap();
        let mut word_id = 1i64;
        for (idx, (a, b)) in pairs.iter().enumerate() {
            let sid = format!("syn{:04}", idx + 1);
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
        file
    }

    #[test]
    fn test_import_wordnet_with_callback() {
        let conn = setup();
        let wn_file = create_mock_wordnet(&[("狩猟", "ハンティング"), ("射撃", "シューティング")]);

        let calls = std::cell::RefCell::new(Vec::new());
        let cb = |imported: usize, total: usize| {
            calls.borrow_mut().push((imported, total));
        };

        let count = import_wordnet(&conn, wn_file.path(), Some(&cb)).unwrap();
        assert_eq!(count, 2);

        let calls = calls.into_inner();
        assert!(!calls.is_empty(), "callback should be called at least once");
        let last = calls.last().unwrap();
        assert_eq!(last.0, 2, "last call should report all imported");
        assert_eq!(last.1, 2, "total should match");
    }

    #[test]
    fn test_import_wordnet_without_callback() {
        let conn = setup();
        let wn_file = create_mock_wordnet(&[("狩猟", "ハンティング")]);

        let count = import_wordnet(&conn, wn_file.path(), None).unwrap();
        assert_eq!(count, 1);

        // Verify pair was inserted
        let stored: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE source = 'wordnet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, 1);
    }

    #[test]
    fn test_import_user_synonyms_basic() {
        let conn = setup();

        let result = import_user_synonyms(&conn, "猟銃,散弾銃\nLoRa,LPWAN\n", true).unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.deleted, 0);

        assert_eq!(user_count(&conn), 2);
    }

    #[test]
    fn test_import_user_synonyms_idempotent() {
        let conn = setup();

        import_user_synonyms(&conn, "猟銃,散弾銃\n", true).unwrap();
        let result = import_user_synonyms(&conn, "猟銃,散弾銃\n", true).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.deleted, 0);

        assert_eq!(user_count(&conn), 1);
    }

    #[test]
    fn test_import_user_synonyms_deletes_removed_pairs() {
        let conn = setup();

        // First import with two pairs
        import_user_synonyms(&conn, "猟銃,散弾銃\nLoRa,LPWAN\n", true).unwrap();

        // Second import with only one pair — the other should be deleted
        let result = import_user_synonyms(&conn, "猟銃,散弾銃\n", true).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.deleted, 1);

        assert_eq!(user_count(&conn), 1);
    }

    #[test]
    fn test_import_mirror_empty_input_is_guarded() {
        let conn = setup();
        import_user_synonyms(&conn, "猟銃,散弾銃\n", true).unwrap();

        // Empty mirror input would wipe all user pairs — must bail, not delete.
        let err = import_user_synonyms(&conn, "", true).unwrap_err();
        assert!(err.to_string().contains("refusing to delete"));
        assert_eq!(user_count(&conn), 1, "the existing pair must survive");
    }

    #[test]
    fn test_import_mirror_all_malformed_is_guarded() {
        let conn = setup();
        import_user_synonyms(&conn, "猟銃,散弾銃\n", true).unwrap();

        // An all-garbage file parses to zero pairs — same wipe risk, same guard.
        let err = import_user_synonyms(&conn, "# just a comment\nbadline\n", true).unwrap_err();
        assert!(err.to_string().contains("refusing to delete"));
        assert_eq!(user_count(&conn), 1);
    }

    #[test]
    fn test_import_empty_input_ok_when_no_user_pairs() {
        let conn = setup();
        // No user pairs to lose → empty mirror input is a harmless no-op.
        let result = import_user_synonyms(&conn, "", true).unwrap();
        assert_eq!(result.total, 0);
        assert_eq!(result.deleted, 0);
    }

    #[test]
    fn test_import_non_mirror_is_insert_only() {
        let conn = setup();
        // A user pair that only lives in the DB (e.g. added via `synonym add`).
        add_user_synonym(&conn, "猟銃", "散弾銃").unwrap();

        // init-style import (mirror = false) of a different pair must ADD it
        // without deleting the DB-only pair absent from the input.
        let result = import_user_synonyms(&conn, "lora,lpwan\n", false).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.deleted, 0);
        assert_eq!(user_count(&conn), 2, "the add-ed pair must not be deleted");
    }

    #[test]
    fn test_import_user_synonyms_skips_comments_and_bad_lines() {
        let conn = setup();

        let result = import_user_synonyms(
            &conn,
            "# comment\n猟銃,散弾銃\nbadline\n,empty\nself,self\n",
            true,
        )
        .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.skipped, 3);
    }

    #[test]
    fn test_import_user_synonyms_does_not_affect_wordnet() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "wordnet").unwrap();

        import_user_synonyms(&conn, "猟銃,散弾銃\n", true).unwrap();

        // Wordnet pair should still exist
        let wn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE source = 'wordnet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wn_count, 1);
    }

    #[test]
    fn test_import_user_synonyms_overlapping_wordnet_not_destroyed() {
        let conn = setup();
        // Pre-populate wordnet with a pair
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "wordnet").unwrap();

        // User CSV includes the same pair — should NOT overwrite wordnet
        import_user_synonyms(&conn, "猟,狩猟\n", true).unwrap();

        // Remove from CSV — wordnet pair must survive
        import_user_synonyms(&conn, "", true).unwrap();

        let (source, score): (String, f64) = conn
            .query_row(
                "SELECT source, score FROM synonyms WHERE word_a = '狩猟' AND word_b = '猟'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(source, "wordnet", "wordnet source must be preserved");
        assert!(
            (score - 0.5).abs() < f64::EPSILON,
            "wordnet score must be preserved"
        );
    }

    #[test]
    fn test_import_user_synonyms_reversed_order() {
        let conn = setup();
        // CSV with reversed order — should normalize
        let result = import_user_synonyms(&conn, "散弾銃,猟銃\n", true).unwrap();
        assert_eq!(result.total, 1);

        assert_eq!(user_count(&conn), 1);
    }

    #[test]
    fn test_import_user_synonyms_duplicate_lines() {
        let conn = setup();
        // Same pair in both orders — should deduplicate
        let result = import_user_synonyms(&conn, "猟銃,散弾銃\n散弾銃,猟銃\n", true).unwrap();
        assert_eq!(result.total, 1);
    }

    fn user_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM synonyms WHERE source = 'user'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn test_add_user_synonym_basic() {
        let conn = setup();
        add_user_synonym(&conn, "猟銃", "散弾銃").unwrap();

        let (a, b, score, source): (String, String, f64, String) = conn
            .query_row(
                "SELECT word_a, word_b, score, source FROM synonyms",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert!(a < b, "words must be ordered");
        assert_eq!(source, "user");
        assert!((score - USER_SCORE).abs() < f64::EPSILON);
    }

    #[test]
    fn test_add_user_synonym_normalizes_and_orders() {
        let conn = setup();
        add_user_synonym(&conn, "  LoRa ", "LPWAN").unwrap();

        let (a, b): (String, String) = conn
            .query_row("SELECT word_a, word_b FROM synonyms", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(a, "lora");
        assert_eq!(b, "lpwan");
    }

    #[test]
    fn test_add_user_synonym_rejects_self_and_empty() {
        let conn = setup();
        assert!(add_user_synonym(&conn, "猟", "猟").is_err());
        assert!(add_user_synonym(&conn, "", "猟").is_err());
        assert!(add_user_synonym(&conn, "猟", "  ").is_err());
        assert_eq!(user_count(&conn), 0);
    }

    #[test]
    fn test_add_user_synonym_upgrades_wordnet() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "wordnet").unwrap();
        add_user_synonym(&conn, "猟", "狩猟").unwrap();

        let (score, source): (f64, String) = conn
            .query_row("SELECT score, source FROM synonyms", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!((score - USER_SCORE).abs() < f64::EPSILON);
        assert_eq!(source, "user");
    }

    #[test]
    fn test_add_user_synonym_claims_higher_scored_non_user() {
        let conn = setup();
        // A non-user pair scored at/above USER_SCORE: the generic upsert's
        // score-tied source rule would leave it non-user. `add` must still claim it.
        upsert_synonym(&conn, "猟", "狩猟", 0.9, "feedback").unwrap();
        add_user_synonym(&conn, "猟", "狩猟").unwrap();

        let (score, source): (f64, String) = conn
            .query_row("SELECT score, source FROM synonyms", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(source, "user", "add must claim ownership");
        assert!(
            (score - 0.9).abs() < f64::EPSILON,
            "the higher existing score is kept"
        );
    }

    #[test]
    fn test_add_user_synonym_returns_normalized_pair() {
        let conn = setup();
        let (lo, hi) = add_user_synonym(&conn, "  LoRa ", "LPWAN").unwrap();
        assert_eq!(lo, "lora");
        assert_eq!(hi, "lpwan");
    }

    #[test]
    fn test_remove_user_synonym_exact_pair() {
        let conn = setup();
        add_user_synonym(&conn, "猟銃", "散弾銃").unwrap();
        add_user_synonym(&conn, "lora", "lpwan").unwrap();

        // Reversed order still matches via normalization.
        let res = remove_user_synonym(&conn, "散弾銃", Some("猟銃")).unwrap();
        assert_eq!(res.removed, 1);
        assert_eq!(res.skipped_non_user, 0);
        assert_eq!(user_count(&conn), 1);
    }

    #[test]
    fn test_remove_user_synonym_all_involving() {
        let conn = setup();
        add_user_synonym(&conn, "猟", "狩猟").unwrap();
        add_user_synonym(&conn, "猟", "銃猟").unwrap();
        add_user_synonym(&conn, "lora", "lpwan").unwrap();

        let res = remove_user_synonym(&conn, "猟", None).unwrap();
        assert_eq!(res.removed, 2, "both pairs involving 猟 removed");
        assert_eq!(user_count(&conn), 1, "unrelated pair survives");
    }

    #[test]
    fn test_remove_user_synonym_leaves_wordnet() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "wordnet").unwrap();

        let res = remove_user_synonym(&conn, "猟", Some("狩猟")).unwrap();
        assert_eq!(res.removed, 0, "wordnet pair not deleted");
        assert_eq!(res.skipped_non_user, 1, "reported as skipped");

        let wn: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE source = 'wordnet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wn, 1);
    }

    #[test]
    fn test_remove_user_synonym_all_involving_leaves_wordnet() {
        let conn = setup();
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "wordnet").unwrap();

        // Single-word remove with no user pairs: nothing deleted, the wordnet
        // pair involving 猟 is reported as skipped.
        let res = remove_user_synonym(&conn, "猟", None).unwrap();
        assert_eq!(res.removed, 0);
        assert_eq!(res.skipped_non_user, 1);

        let wn: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE source = 'wordnet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wn, 1);
    }

    #[test]
    fn test_remove_user_synonym_absent_pair() {
        let conn = setup();
        let res = remove_user_synonym(&conn, "猟", Some("狩猟")).unwrap();
        assert_eq!(res.removed, 0);
        assert_eq!(res.skipped_non_user, 0);
    }

    #[test]
    fn test_remove_user_synonym_rejects_empty() {
        let conn = setup();
        assert!(remove_user_synonym(&conn, "  ", None).is_err());
        assert!(remove_user_synonym(&conn, "猟", Some("")).is_err());
    }

    #[test]
    fn test_export_user_synonyms_only_user() {
        let conn = setup();
        add_user_synonym(&conn, "猟銃", "散弾銃").unwrap();
        upsert_synonym(&conn, "猟", "狩猟", 0.5, "wordnet").unwrap();

        let mut buf = Vec::new();
        let written = export_user_synonyms(&conn, &mut buf).unwrap();
        assert_eq!(written, 1, "only the user pair is exported");

        let content = String::from_utf8(buf).unwrap();
        assert!(content.contains("散弾銃,猟銃") || content.contains("猟銃,散弾銃"));
        assert!(
            !content.contains("狩猟"),
            "wordnet pair must not be exported"
        );
    }

    #[test]
    fn test_export_user_synonyms_sorted() {
        let conn = setup();
        // Insert out of order; export must emit ascending `word_a,word_b` lines.
        add_user_synonym(&conn, "ccc", "ddd").unwrap();
        add_user_synonym(&conn, "aaa", "bbb").unwrap();

        let mut buf = Vec::new();
        export_user_synonyms(&conn, &mut buf).unwrap();
        let content = String::from_utf8(buf).unwrap();

        // Skip the leading header comment; pairs must be lexicographically sorted.
        let lines: Vec<&str> = content.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(lines, vec!["aaa,bbb", "ccc,ddd"]);
    }

    #[test]
    fn test_export_import_round_trip() {
        let conn = setup();
        add_user_synonym(&conn, "猟銃", "散弾銃").unwrap();
        add_user_synonym(&conn, "lora", "lpwan").unwrap();

        let mut buf = Vec::new();
        export_user_synonyms(&conn, &mut buf).unwrap();
        let exported = String::from_utf8(buf).unwrap();

        // Wipe user pairs, then import the exported text back.
        conn.execute("DELETE FROM synonyms WHERE source = 'user'", [])
            .unwrap();
        assert_eq!(user_count(&conn), 0);

        let result = import_user_synonyms(&conn, &exported, true).unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(user_count(&conn), 2, "round-trip restores both pairs");
    }

    #[test]
    fn test_user_source_no_decay() {
        let score = effective_score(0.7, "user", Some("2020-01-01T00:00:00Z"), None);
        assert_eq!(score, 0.7, "user source should not decay");
    }

    #[test]
    fn test_cleanup_preserves_user() {
        let conn = setup();
        conn.execute(
            "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES ('ua', 'ub', 0.7, 'user', 0, '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        cleanup_stale(&conn);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synonyms WHERE word_a = 'ua'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "user pairs should not be cleaned");
    }

    // ─── parse_synonym_csv unit tests ────────────────────────────

    #[test]
    fn test_parse_synonym_csv_basic() {
        let (pairs, skipped) = parse_synonym_csv("猟銃,散弾銃\nLoRa,LPWAN\n");
        assert_eq!(pairs.len(), 2);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_parse_synonym_csv_normalizes_order() {
        // "b,a" should be normalized to (a, b) regardless of input order
        let (pairs1, _) = parse_synonym_csv("bbb,aaa\n");
        let (pairs2, _) = parse_synonym_csv("aaa,bbb\n");
        assert_eq!(pairs1, pairs2);
        assert!(pairs1.contains(&("aaa".into(), "bbb".into())));
    }

    #[test]
    fn test_parse_synonym_csv_deduplicates_reversed() {
        let (pairs, _) = parse_synonym_csv("a,b\nb,a\n");
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn test_parse_synonym_csv_skips_invalid() {
        let (pairs, skipped) = parse_synonym_csv("# comment\ngood,pair\nbad\n,empty\nself,self\n");
        assert_eq!(pairs.len(), 1);
        assert_eq!(skipped, 3);
    }

    #[test]
    fn test_parse_synonym_csv_lowercases() {
        let (pairs, _) = parse_synonym_csv("LoRa,LPWAN\n");
        assert!(pairs.contains(&("lora".into(), "lpwan".into())));
    }

    #[test]
    fn test_parse_synonym_csv_empty() {
        let (pairs, skipped) = parse_synonym_csv("");
        assert_eq!(pairs.len(), 0);
        assert_eq!(skipped, 0);
    }

    // ─── delete_stale_user_pairs unit tests ──────────────────────

    #[test]
    fn test_delete_stale_user_pairs_removes_missing() {
        let conn = setup();
        conn.execute(
            "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES ('aa', 'bb', 0.7, 'user', 0, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        let keep = HashSet::new(); // empty — should delete everything
        let deleted = delete_stale_user_pairs(&conn, &keep).unwrap();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn test_delete_stale_user_pairs_keeps_matching() {
        let conn = setup();
        conn.execute(
            "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES ('aa', 'bb', 0.7, 'user', 0, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        let mut keep = HashSet::new();
        keep.insert(("aa".into(), "bb".into()));
        let deleted = delete_stale_user_pairs(&conn, &keep).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_delete_stale_user_pairs_ignores_other_sources() {
        let conn = setup();
        conn.execute(
            "INSERT INTO synonyms (word_a, word_b, score, source, hits, created)
             VALUES ('aa', 'bb', 0.5, 'wordnet', 0, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        let keep = HashSet::new();
        let deleted = delete_stale_user_pairs(&conn, &keep).unwrap();
        assert_eq!(deleted, 0, "should not delete wordnet pairs");
    }
}
