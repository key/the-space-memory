use std::collections::HashMap;

use rusqlite::Connection;

use crate::{db, entity, tokenizer::wakachi};

use super::plan::QueryPlan;

/// Candidate chunk IDs with their rank positions from each retriever.
pub(crate) struct CandidateSets {
    /// FTS5 result ranks: chunk_id → rank (0-based).
    pub(crate) fts: HashMap<i64, usize>,
    /// Vector search result ranks: chunk_id → rank (0-based).
    pub(crate) vec: HashMap<i64, usize>,
    /// Entity graph result ranks: chunk_id → rank (0-based).
    pub(crate) entity: HashMap<i64, usize>,
}

/// Retrieve stage: run FTS, vector, and entity retrievers and return candidate sets.
///
/// When `require_vector` is true, returns an error if the vec table exists but
/// the vector results are empty (embedder not running).
pub(crate) fn retrieve(
    conn: &Connection,
    plan: &QueryPlan,
    limit: usize,
    require_vector: bool,
) -> anyhow::Result<CandidateSets> {
    let query = plan.keywords_query.as_str();

    let fts = if plan.expansions.is_empty() {
        fts_results(conn, query, limit)?
    } else {
        let expanded = build_expanded_fts_query(query, &plan.expansions);
        fts_results_raw(conn, &expanded, limit)?
    };

    let vec = vec_results_from_embedding(conn, plan.query_vec.as_deref(), limit)?;

    if require_vector && vec.is_empty() && db::has_vec_table(conn) {
        anyhow::bail!(
            "Embedder is not running. Vector search unavailable.\n\
             Run `tsm restart` to restart, or use `--fallback fts_only` for FTS-only search."
        );
    }

    let entity =
        entity::entity_results_by_ids(conn, &plan.classification.matched_entity_ids, limit)
            .unwrap_or_default();

    Ok(CandidateSets { fts, vec, entity })
}

/// Run FTS5 search using the plain (non-expanded) keywords query.
fn fts_results(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> anyhow::Result<HashMap<i64, usize>> {
    if query.trim().is_empty() {
        return Ok(HashMap::new());
    }

    let wakachi_query = wakachi(query);
    let tokens: Vec<&str> = wakachi_query.split_whitespace().collect();
    if tokens.is_empty() {
        return Ok(HashMap::new());
    }

    let fts_query = tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" AND ");

    fts_results_raw(conn, &fts_query, limit)
}

/// Run FTS5 search using a pre-built query string (e.g. expanded with synonyms).
fn fts_results_raw(
    conn: &Connection,
    fts_query: &str,
    limit: usize,
) -> anyhow::Result<HashMap<i64, usize>> {
    if fts_query.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let fts_query = fts_query.to_string();

    let mut stmt = conn.prepare(
        "SELECT chunks_fts.rowid AS chunk_id
         FROM chunks_fts
         WHERE chunks_fts MATCH ?
         ORDER BY rank
         LIMIT ?",
    )?;

    let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
        row.get::<_, i64>(0)
    })?;

    let mut result = HashMap::new();
    for (i, row) in rows.enumerate() {
        result.insert(row?, i);
    }
    Ok(result)
}

/// Run vector MATCH search using an already-computed query embedding.
///
/// Returns an empty map when the embedding is `None` (embedder unavailable)
/// or the vec table does not exist.
fn vec_results_from_embedding(
    conn: &Connection,
    query_vec: Option<&[f32]>,
    limit: usize,
) -> anyhow::Result<HashMap<i64, usize>> {
    let vec = match query_vec {
        Some(v) if !v.is_empty() => v,
        _ => return Ok(HashMap::new()),
    };

    if !db::has_vec_table(conn) {
        return Ok(HashMap::new());
    }

    let query_vec_json = serde_json::to_string(vec)?;

    let mut stmt = conn.prepare(
        "SELECT rowid, distance FROM chunks_vec WHERE embedding MATCH ? ORDER BY distance LIMIT ?",
    )?;

    let rows = stmt.query_map(rusqlite::params![query_vec_json, limit as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;

    let mut result = HashMap::new();
    for (i, row) in rows.enumerate() {
        let (chunk_id, _distance) = row?;
        result.insert(chunk_id, i);
    }
    Ok(result)
}

/// Build an expanded FTS5 query: original terms (AND) OR expansion terms.
fn build_expanded_fts_query(query: &str, expansions: &[String]) -> String {
    let wakachi_query = wakachi(query);
    let tokens: Vec<&str> = wakachi_query.split_whitespace().collect();
    if tokens.is_empty() {
        return query.to_string();
    }

    let original = tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" AND ");

    if expansions.is_empty() {
        return original;
    }

    let expansion_terms: Vec<String> = expansions
        .iter()
        .map(|e| {
            let w = wakachi(e);
            let toks: Vec<&str> = w.split_whitespace().collect();
            toks.iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" AND ")
        })
        .collect();

    format!("({}) OR {}", original, expansion_terms.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn test_fts_insert_and_match() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at)
             VALUES ('test.md', 'note', 'Test', 'hash', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content)
             VALUES (1, 0, 'Test', '射撃場のルールについて説明します。')",
            [],
        )
        .unwrap();
        let chunk_id: i64 = conn
            .query_row("SELECT id FROM chunks WHERE document_id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let wakachi_text = wakachi("射撃場のルールについて説明します。");
        conn.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
            rusqlite::params![chunk_id, wakachi_text],
        )
        .unwrap();

        let ranks = fts_results(&conn, "射撃", 10).unwrap();
        assert!(!ranks.is_empty());
        assert!(ranks.contains_key(&chunk_id));
    }

    #[test]
    fn test_fts_no_match() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at)
             VALUES ('test.md', 'note', 'Test', 'hash', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content)
             VALUES (1, 0, 'Test', '射撃場のルール')",
            [],
        )
        .unwrap();
        let wakachi_text = wakachi("射撃場のルール");
        conn.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (1, ?)",
            rusqlite::params![wakachi_text],
        )
        .unwrap();

        let ranks = fts_results(&conn, "ロケット", 10).unwrap();
        assert!(ranks.is_empty());
    }

    #[test]
    fn test_fts_empty_query() {
        let conn = db::get_memory_connection().unwrap();
        let ranks = fts_results(&conn, "", 10).unwrap();
        assert!(ranks.is_empty());
        let ranks = fts_results(&conn, "   ", 10).unwrap();
        assert!(ranks.is_empty());
    }

    #[test]
    fn test_build_expanded_fts_query_no_expansions() {
        let result = build_expanded_fts_query("射撃 ルール", &[]);
        // Should just be the original wakachi'd AND query
        assert!(result.contains("AND"));
        assert!(!result.contains("OR"));
    }

    #[test]
    fn test_build_expanded_fts_query_with_expansions() {
        let result =
            build_expanded_fts_query("rust", &["sqlite".to_string(), "lindera".to_string()]);
        assert!(result.contains("OR"));
        assert!(result.contains("rust"));
    }

    #[test]
    fn test_build_expanded_fts_query_empty_query() {
        let result = build_expanded_fts_query("", &["sqlite".to_string()]);
        assert_eq!(result, "");
    }
}
