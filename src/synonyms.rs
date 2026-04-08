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
        "wordnet" => None, // No decay
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

    let a = word_a.trim().to_lowercase();
    let b = word_b.trim().to_lowercase();
    if a == b || a.is_empty() || b.is_empty() {
        return Ok(());
    }

    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
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

/// Import synonym pairs from a Japanese WordNet SQLite database.
/// Extracts pairs of Japanese words that share a synset.
pub fn import_wordnet(conn: &Connection, wordnet_path: &std::path::Path) -> anyhow::Result<usize> {
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
        if imported % 10000 == 0 {
            eprint!("\r  {imported}/{total}");
        }
    }

    eprint!("\r                              \r"); // clear progress line
    log::info!("{imported}/{total} synonym pairs imported.");
    Ok(imported)
}

/// Record a hit on a synonym pair (increments hits, updates last_hit).
pub fn record_hit(conn: &Connection, word_a: &str, word_b: &str) {
    let a = word_a.trim().to_lowercase();
    let b = word_b.trim().to_lowercase();
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
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
            "DELETE FROM synonyms WHERE hits = 0 AND source != 'wordnet' AND created < ?",
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
}
