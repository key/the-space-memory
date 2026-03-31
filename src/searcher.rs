use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::classifier;
use crate::config;
use crate::db;
use crate::doc_links;
use crate::embedder;
use crate::entity;
use crate::synonyms;
use crate::temporal::TimeFilter;
use crate::tokenizer::{extract_search_keywords, wakachi};
use crate::user_dict;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub source_file: String,
    pub source_type: String,
    pub section_path: String,
    pub snippet: String,
    pub score: f64,
    pub status: Option<String>,
    pub related_docs: Vec<doc_links::RelatedDoc>,
}

/// Search for documents matching the query, with optional time filtering.
pub fn search(
    conn: &Connection,
    query: &str,
    top_k: usize,
    time_filter: Option<&TimeFilter>,
) -> anyhow::Result<Vec<SearchResult>> {
    // Query preprocessing: extract meaningful keywords, skip noise-only queries
    let keywords = extract_search_keywords(query);
    if keywords.len() < config::MIN_QUERY_KEYWORDS {
        return Ok(Vec::new());
    }
    let query = &keywords.join(" ");

    let limit = top_k * 3;
    let cls = classifier::classify(conn, query);

    // Lazy spawn stale synonym cleanup (once per process)
    synonyms::maybe_spawn_cleanup(config::db_path());

    // Collect query terms as dictionary candidates
    user_dict::collect_from_query(conn, query);

    // Expand query: entity graph + synonym dictionary
    let entity_exp =
        entity::expand_entities_by_ids(conn, &cls.matched_entity_ids, config::MAX_QUERY_EXPANSIONS);
    let synonym_exp = synonyms::expand_query_synonyms(conn, query, 3, 0.3);
    let mut all_expansions = entity_exp;
    for s in synonym_exp {
        if !all_expansions.contains(&s) {
            all_expansions.push(s);
        }
    }

    let fts_ranks = if all_expansions.is_empty() {
        fts_results(conn, query, limit)?
    } else {
        let expanded = build_expanded_fts_query(query, &all_expansions);
        fts_results_raw(conn, &expanded, limit)?
    };
    let vec_ranks = vec_results(conn, query, limit)?;
    let ent_ranks =
        entity::entity_results_by_ids(conn, &cls.matched_entity_ids, limit).unwrap_or_default();

    let all_chunk_ids: Vec<i64> = fts_ranks
        .keys()
        .chain(vec_ranks.keys())
        .chain(ent_ranks.keys())
        .copied()
        .collect::<std::collections::HashSet<i64>>()
        .into_iter()
        .collect();

    if all_chunk_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = all_chunk_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");

    let mut time_clauses = Vec::new();
    let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(tf) = time_filter {
        if let Some(ref after) = tf.after {
            time_clauses.push(
                "(COALESCE(d.updated, d.created) >= ? OR (d.updated IS NULL AND d.created IS NULL))"
                    .to_string(),
            );
            extra_params.push(Box::new(after.clone()));
        }
        if let Some(ref before) = tf.before {
            time_clauses.push(
                "(COALESCE(d.updated, d.created) < ? OR (d.updated IS NULL AND d.created IS NULL))"
                    .to_string(),
            );
            extra_params.push(Box::new(before.clone()));
        }
    }
    let time_sql = if time_clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", time_clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT c.id AS chunk_id, c.section_path, c.content,
                d.file_path, d.source_type, d.status, d.updated
         FROM chunks c
         JOIN documents d ON c.document_id = d.id
         WHERE c.id IN ({placeholders}){time_sql}"
    );

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = all_chunk_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    params.extend(extra_params);

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut results = Vec::new();
    for row in rows {
        let (chunk_id, section_path, content, file_path, source_type, status, updated) = row?;

        let mut rrf = 0.0;
        if let Some(&rank) = fts_ranks.get(&chunk_id) {
            rrf += cls.fts_weight / (config::RRF_K + rank as f64);
        }
        if let Some(&rank) = vec_ranks.get(&chunk_id) {
            rrf += cls.vec_weight / (config::RRF_K + rank as f64);
        }
        if let Some(&rank) = ent_ranks.get(&chunk_id) {
            rrf += 1.0 / (config::RRF_K + rank as f64);
        }

        let decay = time_decay(updated.as_deref(), &source_type);
        let penalty = config::status_penalty(status.as_deref());
        let weight = config::directory_weight(&file_path);
        let score = rrf * decay * penalty * weight;

        if score < config::SCORE_THRESHOLD {
            continue;
        }

        results.push(SearchResult {
            source_file: file_path,
            source_type,
            section_path: section_path.unwrap_or_default(),
            snippet: snippet(&content),
            score,
            status,
            related_docs: Vec::new(),
        });
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(top_k);

    // Enrich with related documents
    let result_doc_ids: Vec<i64> = results
        .iter()
        .filter_map(|r| {
            conn.query_row(
                "SELECT DISTINCT document_id FROM chunks c JOIN documents d ON c.document_id = d.id WHERE d.file_path = ? LIMIT 1",
                rusqlite::params![r.source_file],
                |row| row.get::<_, i64>(0),
            ).ok()
        })
        .collect();

    if !result_doc_ids.is_empty() {
        let all_related = doc_links::find_related(conn, &result_doc_ids, 5);
        // Attach related docs to each result (exclude docs already in results)
        let result_files: std::collections::HashSet<&str> =
            results.iter().map(|r| r.source_file.as_str()).collect();
        let filtered: Vec<_> = all_related
            .into_iter()
            .filter(|rd| !result_files.contains(rd.file_path.as_str()))
            .collect();
        if !filtered.is_empty() {
            // Attach to first result as a summary
            results[0].related_docs = filtered;
        }
    }

    Ok(results)
}

pub(crate) fn time_decay(updated: Option<&str>, source_type: &str) -> f64 {
    let updated = match updated {
        Some(s) if !s.is_empty() => s,
        _ => return 0.5,
    };

    let updated_dt: DateTime<Utc> = match updated.parse::<DateTime<Utc>>() {
        Ok(dt) => dt,
        Err(_) => {
            // Try parsing as naive date
            match chrono::NaiveDate::parse_from_str(updated, "%Y-%m-%d") {
                Ok(nd) => nd.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                Err(_) => return 0.5,
            }
        }
    };

    let now = Utc::now();
    let days = (now - updated_dt).num_days().max(0) as f64;
    let half_life = config::half_life_days(source_type);
    0.5_f64.powf(days / half_life)
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

pub(crate) fn snippet(content: &str) -> String {
    let text = match content.split_once('\n') {
        Some((_, rest)) => rest,
        None => content,
    };
    let chars: String = text.chars().take(config::SNIPPET_MAX_CHARS).collect();
    chars.trim().to_string()
}

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

fn vec_results(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> anyhow::Result<HashMap<i64, usize>> {
    if query.trim().is_empty() {
        return Ok(HashMap::new());
    }

    // Check if vec table exists
    if !db::has_vec_table(conn) {
        return Ok(HashMap::new());
    }

    // Get query embedding from embedder daemon
    let texts = vec![query.to_string()];
    let embeddings = match embedder::embed_via_socket(&texts) {
        Some(e) if !e.is_empty() => e,
        _ => return Ok(HashMap::new()), // Embedder not running, graceful fallback
    };

    let query_vec = serde_json::to_string(&embeddings[0])?;

    let mut stmt = conn.prepare(
        "SELECT rowid, distance FROM chunks_vec WHERE embedding MATCH ? ORDER BY distance LIMIT ?",
    )?;

    let rows = stmt.query_map(rusqlite::params![query_vec, limit as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;

    let mut result = HashMap::new();
    for (i, row) in rows.enumerate() {
        let (chunk_id, _distance) = row?;
        result.insert(chunk_id, i);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn test_recent_date_high_decay() {
        let now = Utc::now().format("%Y-%m-%d").to_string();
        let decay = time_decay(Some(&now), "note");
        assert!(decay > 0.9);
        assert!(decay <= 1.0);
    }

    #[test]
    fn test_none_returns_half() {
        assert_eq!(time_decay(None, "note"), 0.5);
    }

    #[test]
    fn test_invalid_date_returns_half() {
        assert_eq!(time_decay(Some("not-a-date"), "note"), 0.5);
    }

    #[test]
    fn test_old_date_low_decay() {
        let decay = time_decay(Some("2020-01-01"), "note");
        assert!(decay < 0.1);
    }

    #[test]
    fn test_source_type_half_life() {
        let date = "2025-01-01";
        let session_decay = time_decay(Some(date), "session");
        let note_decay = time_decay(Some(date), "note");
        assert!(session_decay < note_decay);
    }

    #[test]
    fn test_status_penalty_none() {
        assert_eq!(config::status_penalty(None), 1.0);
    }

    #[test]
    fn test_status_penalty_current() {
        assert_eq!(config::status_penalty(Some("current")), 1.0);
    }

    #[test]
    fn test_status_penalty_outdated() {
        assert!(config::status_penalty(Some("outdated")) < 1.0);
    }

    #[test]
    fn test_snippet_strips_prefix() {
        let content = "【daily/notes/test】セクション\nこれは本文です。";
        let result = snippet(content);
        assert_eq!(result, "これは本文です。");
    }

    #[test]
    fn test_snippet_truncates_at_200() {
        let content = format!("prefix\n{}", "あ".repeat(300));
        let result = snippet(&content);
        assert_eq!(result.chars().count(), 200);
    }

    #[test]
    fn test_snippet_single_line() {
        assert_eq!(snippet("単一行テスト"), "単一行テスト");
    }

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
    fn test_search_e2e_via_indexer() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        // Create and index a markdown file
        let md = "---\nstatus: current\n---\n\n# 射撃場のルール\n\n射撃場での安全管理について説明します。\n";
        let full = dir.path().join("daily/notes/shooting.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(md.as_bytes()).unwrap();

        indexer::index_file(&conn, &full, dir.path()).unwrap();

        // Search should find it
        let results = search(&conn, "射撃", 5, None).unwrap();
        assert!(!results.is_empty());
        assert!(results[0].source_file.contains("shooting"));
        assert!(results[0].score > 0.0);
        assert_eq!(results[0].source_type, "note");
    }

    #[test]
    fn test_search_empty_query() {
        let conn = db::get_memory_connection().unwrap();
        let results = search(&conn, "", 5, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_no_results() {
        let conn = db::get_memory_connection().unwrap();
        let results = search(&conn, "存在しないキーワード", 5, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_respects_top_k() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        // Create multiple files with the same keyword
        for i in 0..5 {
            let md = format!(
                "---\nstatus: current\n---\n\n# テスト文書{i}\n\nテスト検索キーワードの内容です。\n"
            );
            let rel = format!("daily/notes/test{i}.md");
            let full = dir.path().join(&rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(md.as_bytes()).unwrap();
            indexer::index_file(&conn, &full, dir.path()).unwrap();
        }

        let results = search(&conn, "テスト", 3, None).unwrap();
        assert!(results.len() <= 3);
    }

    #[test]
    fn test_search_with_time_filter_includes() {
        use crate::indexer;
        use crate::temporal::TimeFilter;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        // Use today's date so time_decay doesn't push score below threshold
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let md = format!(
            "---\nstatus: current\nupdated: {today}\n---\n\n# 射撃ルール\n\n射撃場での安全管理。\n"
        );
        let full = dir.path().join("daily/notes/shooting.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(md.as_bytes()).unwrap();
        indexer::index_file(&conn, &full, dir.path()).unwrap();

        let filter = TimeFilter {
            after: Some("2020-01-01".to_string()),
            before: Some("2099-01-01".to_string()),
        };
        let results = search(&conn, "射撃", 5, Some(&filter)).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn test_search_with_time_filter_excludes() {
        use crate::indexer;
        use crate::temporal::TimeFilter;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let md = format!(
            "---\nstatus: current\nupdated: {today}\n---\n\n# 射撃ルール\n\n射撃場での安全管理。\n"
        );
        let full = dir.path().join("daily/notes/shooting.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(md.as_bytes()).unwrap();
        indexer::index_file(&conn, &full, dir.path()).unwrap();

        // Filter for a far-future range — should exclude the document
        let filter = TimeFilter {
            after: Some("2099-01-01".to_string()),
            before: None,
        };
        let results = search(&conn, "射撃", 5, Some(&filter)).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_with_time_filter_null_dates_pass() {
        use crate::indexer;
        use crate::temporal::TimeFilter;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        let md = "---\nstatus: current\n---\n\n# 射撃ルール\n\n射撃場での安全管理。\n";
        let full = dir.path().join("daily/notes/shooting.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(md.as_bytes()).unwrap();
        indexer::index_file(&conn, &full, dir.path()).unwrap();

        let filter = TimeFilter {
            after: Some("2025-01-01".to_string()),
            before: None,
        };
        let results = search(&conn, "射撃", 5, Some(&filter)).unwrap();
        assert!(!results.is_empty());
    }

    // ─── query expansion tests ───────────────────────────────

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

    #[test]
    fn test_search_noise_query_returns_empty() {
        let conn = db::get_memory_connection().unwrap();
        // Pure interjection/greeting should return empty results
        let results = search(&conn, "よかったーーー", 5, None).unwrap();
        assert!(results.is_empty(), "Noise query should return empty results");
    }

    #[test]
    fn test_search_stopword_query_returns_empty() {
        let conn = db::get_memory_connection().unwrap();
        let results = search(&conn, "なるほど", 5, None).unwrap();
        assert!(
            results.is_empty(),
            "Stopword-only query should return empty results"
        );
    }

    #[test]
    fn test_search_meaningful_query_still_works() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        let md =
            "---\nstatus: current\n---\n\n# LoRaモジュール\n\nLoRaモジュールの開発進捗について。\n";
        let full = dir.path().join("daily/notes/lora.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(md.as_bytes()).unwrap();
        indexer::index_file(&conn, &full, dir.path()).unwrap();

        // A meaningful query should still find results
        let results = search(&conn, "LoRaモジュール", 5, None).unwrap();
        assert!(
            !results.is_empty(),
            "Meaningful query should find results"
        );
    }

    #[test]
    fn test_candidates_collected_on_search() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        let md = "---\nstatus: current\n---\n\n# Test\n\nSome content for candle search.\n";
        let full = dir.path().join("daily/notes/test.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(md.as_bytes()).unwrap();
        indexer::index_file(&conn, &full, dir.path()).unwrap();

        // Clear candidates from indexing
        let _ = conn.execute("DELETE FROM dictionary_candidates", []);

        // Search should collect query candidates
        let _ = search(&conn, "candle framework", 5, None);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            count > 0,
            "should collect dictionary candidates from search query"
        );
    }

    #[test]
    fn test_search_result_serde_roundtrip() {
        let result = SearchResult {
            source_file: "daily/notes/test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Test > Section".to_string(),
            snippet: "Some content".to_string(),
            score: 0.5,
            status: Some("current".to_string()),
            related_docs: vec![doc_links::RelatedDoc {
                file_path: "company/knowledge/related.md".to_string(),
                link_type: "tag".to_string(),
                strength: 0.8,
            }],
        };
        let json = serde_json::to_value(&result).unwrap();
        let decoded: SearchResult = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.source_file, "daily/notes/test.md");
        assert_eq!(decoded.score, 0.5);
        assert_eq!(decoded.related_docs.len(), 1);
        assert_eq!(decoded.related_docs[0].file_path, "company/knowledge/related.md");
        assert_eq!(decoded.related_docs[0].link_type, "tag");
    }
}
