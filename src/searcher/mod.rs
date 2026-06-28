pub(crate) mod format;
pub(crate) mod plan;
pub(crate) mod rank;
pub(crate) mod retrieve;

pub use format::{format_json, format_text};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::doc_links;
use crate::temporal::TimeFilter;

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

/// Search output with total hit count (before top_k truncation).
#[derive(Debug)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub total_hits: usize,
}

/// Search for documents matching the query, with optional time filtering.
///
/// When `require_vector` is true, returns an error if vector search is
/// unavailable (embedder not running or vec table missing).
pub fn search(
    conn: &Connection,
    query: &str,
    top_k: usize,
    time_filter: Option<&TimeFilter>,
    require_vector: bool,
    path_prefixes: Option<&[String]>,
) -> anyhow::Result<SearchOutput> {
    // Plan stage: keyword extraction, classification, side effects, expansions
    let qp = match plan::plan(conn, query)? {
        Some(p) => p,
        None => {
            return Ok(SearchOutput {
                results: Vec::new(),
                total_hits: 0,
            })
        }
    };
    let limit = top_k * 3;

    let candidates = retrieve::retrieve(conn, &qp, limit, require_vector, path_prefixes)?;

    // The all-empty candidate case is handled inside rank() (its union guard),
    // which returns an empty SearchOutput — no need to peek at the sets here.
    rank::rank(conn, &qp, candidates, top_k, time_filter, path_prefixes)
}

/// Decay multiplier 0.5^(age_days / half_life). Returns 0.5 if the date is
/// absent, empty, or unparseable (missing-date sentinel).
pub(crate) fn decay_with_half_life(updated: Option<&str>, half_life: f64) -> f64 {
    let s = match updated {
        Some(s) if !s.is_empty() => s,
        _ => return 0.5,
    };
    let updated_dt: DateTime<Utc> = match s.parse::<DateTime<Utc>>() {
        Ok(dt) => dt,
        Err(_) => match chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            Ok(nd) => nd.and_hms_opt(0, 0, 0).unwrap().and_utc(),
            Err(_) => return 0.5,
        },
    };
    let days = (Utc::now() - updated_dt).num_days().max(0) as f64;
    rank::decay_factor(days, half_life)
}

/// Today's date as a `%Y-%m-%d` string (UTC).
pub(crate) fn today_string() -> String {
    Utc::now().date_naive().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db;

    #[test]
    fn test_recent_date_high_decay() {
        let now = Utc::now().format("%Y-%m-%d").to_string();
        let decay = decay_with_half_life(
            Some(&now),
            config::half_life_days("daily/notes/test.md", "note"),
        );
        assert!(decay > 0.9);
        assert!(decay <= 1.0);
    }

    #[test]
    fn test_none_returns_half() {
        assert_eq!(
            decay_with_half_life(None, config::half_life_days("daily/notes/test.md", "note")),
            0.5
        );
    }

    #[test]
    fn test_invalid_date_returns_half() {
        assert_eq!(
            decay_with_half_life(
                Some("not-a-date"),
                config::half_life_days("daily/notes/test.md", "note")
            ),
            0.5
        );
    }

    #[test]
    fn test_old_date_low_decay() {
        let decay = decay_with_half_life(
            Some("2020-01-01"),
            config::half_life_days("daily/notes/test.md", "note"),
        );
        assert!(decay < 0.1);
    }

    #[test]
    fn test_source_type_half_life() {
        let date = "2025-01-01";
        let session_decay =
            decay_with_half_life(Some(date), config::half_life_days("session:abc", "session"));
        let note_decay = decay_with_half_life(
            Some(date),
            config::half_life_days("daily/notes/test.md", "note"),
        );
        assert!(session_decay < note_decay);
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
        let SearchOutput { results, .. } = search(&conn, "射撃", 5, None, false, None).unwrap();
        assert!(!results.is_empty());
        assert!(results[0].source_file.contains("shooting"));
        assert!(results[0].score > 0.0);
        assert_eq!(results[0].source_type, "note");
    }

    #[test]
    fn test_search_empty_query() {
        let conn = db::get_memory_connection().unwrap();
        let SearchOutput { results, .. } = search(&conn, "", 5, None, false, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_no_results() {
        let conn = db::get_memory_connection().unwrap();
        let SearchOutput { results, .. } =
            search(&conn, "存在しないキーワード", 5, None, false, None).unwrap();
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

        let SearchOutput { results, .. } = search(&conn, "テスト", 3, None, false, None).unwrap();
        assert!(results.len() <= 3);
    }

    #[test]
    fn test_search_with_time_filter_includes() {
        use crate::indexer;
        use crate::temporal::TimeFilter;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        // Use today's date so decay doesn't push score below threshold
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
        let SearchOutput { results, .. } =
            search(&conn, "射撃", 5, Some(&filter), false, None).unwrap();
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
        let SearchOutput { results, .. } =
            search(&conn, "射撃", 5, Some(&filter), false, None).unwrap();
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
        let SearchOutput { results, .. } =
            search(&conn, "射撃", 5, Some(&filter), false, None).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn test_search_noise_query_returns_empty() {
        let conn = db::get_memory_connection().unwrap();
        // Pure interjection/greeting should return empty results
        let SearchOutput { results, .. } =
            search(&conn, "よかったーーー", 5, None, false, None).unwrap();
        assert!(
            results.is_empty(),
            "Noise query should return empty results"
        );
    }

    #[test]
    fn test_search_stopword_query_returns_empty() {
        let conn = db::get_memory_connection().unwrap();
        let SearchOutput { results, .. } = search(&conn, "なるほど", 5, None, false, None).unwrap();
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
        let SearchOutput { results, .. } =
            search(&conn, "LoRaモジュール", 5, None, false, None).unwrap();
        assert!(!results.is_empty(), "Meaningful query should find results");
    }

    #[test]
    fn test_search_with_path_filter_includes() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        // Create files in different directories
        let daily_md = "---\nstatus: current\n---\n\n# MTG Notes\n\nMTG meeting notes content.\n";
        let daily_path = dir.path().join("daily/notes/mtg.md");
        std::fs::create_dir_all(daily_path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&daily_path).unwrap();
        f.write_all(daily_md.as_bytes()).unwrap();

        let project_md =
            "---\nstatus: current\n---\n\n# Project MTG\n\nMTG project documentation.\n";
        let project_path = dir.path().join("projects/tsm/mtg.md");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&project_path).unwrap();
        f.write_all(project_md.as_bytes()).unwrap();

        indexer::index_file(&conn, &daily_path, dir.path()).unwrap();
        indexer::index_file(&conn, &project_path, dir.path()).unwrap();

        // Filter to <dir>/daily only (absolute, ADR-0017)
        let daily = dir.path().join("daily").to_string_lossy().to_string();
        let paths = vec![daily.clone()];
        let SearchOutput { results, .. } =
            search(&conn, "MTG", 5, None, false, Some(&paths)).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert!(
                r.source_file.starts_with(&format!("{daily}/")),
                "Expected {daily}/ prefix, got: {}",
                r.source_file
            );
        }
    }

    #[test]
    fn test_search_with_path_filter_excludes() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        let md = "---\nstatus: current\n---\n\n# MTG Notes\n\nMTG meeting notes.\n";
        let path = dir.path().join("daily/notes/mtg.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(md.as_bytes()).unwrap();
        indexer::index_file(&conn, &path, dir.path()).unwrap();

        // Filter to <dir>/projects — should exclude daily/
        let projects = dir.path().join("projects").to_string_lossy().to_string();
        let paths = vec![projects];
        let SearchOutput { results, .. } =
            search(&conn, "MTG", 5, None, false, Some(&paths)).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn empty_scope_returns_no_results_not_error() {
        // A scope matching nothing must return Ok(empty), never an SQL error —
        // guards the vector retriever's `rowid IN (SELECT ...)` subquery against
        // the invalid `rowid in ()` that a materialized empty id list would emit.
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let md = "---\nstatus: current\n---\n\n# MTG\n\nMTG content here.\n";
        let path = dir.path().join("daily/notes/mtg.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(md.as_bytes())
            .unwrap();
        indexer::index_file(&conn, &path, dir.path()).unwrap();

        let nope = dir
            .path()
            .join("does-not-exist")
            .to_string_lossy()
            .to_string();
        // require_vector = true: an empty scope yields an empty vector set, but
        // that must NOT be misread as "embedder down" — FTS is also empty, so
        // the scope simply matched nothing. (Regression guard for the
        // require_vector + empty-scope interaction.)
        let out = search(&conn, "MTG", 5, None, true, Some(&[nope]));
        let SearchOutput { results, .. } = out.expect("empty scope must not error");
        assert!(results.is_empty());
    }

    #[test]
    fn entity_only_match_errors_when_embedder_down() {
        // FTS misses (body lacks the term) but the frontmatter tag matches an
        // entity. With require_vector=true and no embedder (query_vec None →
        // empty vec), this must error rather than silently return entity-only
        // results — otherwise the embedder outage is hidden under fallback=error.
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let md = "---\nstatus: current\ntags: [rust]\n---\n\n# Topic\n\nUnrelated body content.\n";
        let full = dir.path().join("notes/x.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::File::create(&full)
            .unwrap()
            .write_all(md.as_bytes())
            .unwrap();
        indexer::index_file(&conn, &full, dir.path()).unwrap();

        let out = search(&conn, "rust", 5, None, true, None);
        assert!(
            out.is_err(),
            "embedder-down with an entity-only match must surface the outage"
        );
    }

    #[test]
    fn test_search_with_multiple_path_filters() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        for (rel, title) in [
            ("daily/notes/mtg.md", "Daily MTG"),
            ("projects/tsm/mtg.md", "Project MTG"),
            ("docs/api.md", "API MTG"),
        ] {
            let md =
                format!("---\nstatus: current\n---\n\n# {title}\n\nMTG content for searching.\n");
            let full = dir.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(md.as_bytes()).unwrap();
            indexer::index_file(&conn, &full, dir.path()).unwrap();
        }

        // Filter to <dir>/daily and <dir>/docs (OR, absolute)
        let daily = dir.path().join("daily").to_string_lossy().to_string();
        let docs = dir.path().join("docs").to_string_lossy().to_string();
        let paths = vec![daily.clone(), docs.clone()];
        let SearchOutput { results, .. } =
            search(&conn, "MTG", 10, None, false, Some(&paths)).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert!(
                r.source_file.starts_with(&format!("{daily}/"))
                    || r.source_file.starts_with(&format!("{docs}/")),
                "Unexpected path: {}",
                r.source_file
            );
        }
    }

    #[test]
    fn test_search_with_no_path_filter_returns_all() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        for rel in ["daily/notes/mtg.md", "projects/tsm/mtg.md"] {
            let md = "---\nstatus: current\n---\n\n# MTG Notes\n\nMTG meeting content.\n";
            let full = dir.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(md.as_bytes()).unwrap();
            indexer::index_file(&conn, &full, dir.path()).unwrap();
        }

        // No path filter — should return results from both directories
        let SearchOutput { results, .. } = search(&conn, "MTG", 10, None, false, None).unwrap();
        assert!(results.len() >= 2);
    }

    #[test]
    fn test_search_with_file_path_filter() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        for rel in ["docs/api.md", "docs/guide.md"] {
            let md = format!(
                "---\nstatus: current\n---\n\n# Auth\n\nAuthentication details for {}.\n",
                rel
            );
            let full = dir.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(md.as_bytes()).unwrap();
            indexer::index_file(&conn, &full, dir.path()).unwrap();
        }

        // Filter to a specific file (absolute) — matched via the equality branch.
        let api = dir.path().join("docs/api.md").to_string_lossy().to_string();
        let paths = vec![api.clone()];
        let SearchOutput { results, .. } =
            search(&conn, "Authentication", 10, None, false, Some(&paths)).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert_eq!(r.source_file, api);
        }
    }

    #[test]
    fn test_search_path_filter_escapes_like_metacharacters() {
        use crate::indexer;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        // Create files: one with underscore, one without
        for (rel, title) in [
            ("daily_notes/mtg.md", "Daily Notes MTG"),
            ("dailyXnotes/mtg.md", "DailyX Notes MTG"),
        ] {
            let md =
                format!("---\nstatus: current\n---\n\n# {title}\n\nMTG content for testing.\n");
            let full = dir.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(md.as_bytes()).unwrap();
            indexer::index_file(&conn, &full, dir.path()).unwrap();
        }

        // _ in path must be literal, not a wildcard
        let dn = dir.path().join("daily_notes").to_string_lossy().to_string();
        let paths = vec![dn.clone()];
        let SearchOutput { results, .. } =
            search(&conn, "MTG", 10, None, false, Some(&paths)).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert!(
                r.source_file.starts_with(&format!("{dn}/")),
                "Expected {dn}/ prefix, got: {}",
                r.source_file
            );
        }
    }

    #[test]
    fn test_search_path_filter_with_time_filter() {
        use crate::indexer;
        use crate::temporal::TimeFilter;
        use std::io::Write;

        let conn = db::get_memory_connection().unwrap();
        let dir = tempfile::TempDir::new().unwrap();

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        for (rel, date) in [
            ("daily/recent.md", today.as_str()),
            ("daily/old.md", "2020-01-01"),
            ("projects/recent.md", today.as_str()),
        ] {
            let md = format!(
                "---\nstatus: current\nupdated: {date}\n---\n\n# MTG\n\nMTG content here.\n"
            );
            let full = dir.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(md.as_bytes()).unwrap();
            indexer::index_file(&conn, &full, dir.path()).unwrap();
        }

        // Combine path filter + time filter (absolute path, ADR-0017)
        let daily = dir.path().join("daily").to_string_lossy().to_string();
        let paths = vec![daily.clone()];
        let filter = TimeFilter {
            after: Some("2025-01-01".to_string()),
            before: None,
        };
        let SearchOutput { results, .. } =
            search(&conn, "MTG", 10, Some(&filter), false, Some(&paths)).unwrap();
        // daily/recent.md (today) is in scope + recent; daily/old.md is filtered
        // by time; projects/recent.md is out of path scope.
        assert!(!results.is_empty());
        for r in &results {
            assert!(
                r.source_file.starts_with(&format!("{daily}/")),
                "Expected {daily}/ prefix, got: {}",
                r.source_file
            );
        }
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
        assert_eq!(
            decoded.related_docs[0].file_path,
            "company/knowledge/related.md"
        );
        assert_eq!(decoded.related_docs[0].link_type, "tag");
    }

    // ── Score hook integration tests ──────────────────────────────────────────

    /// Helper: insert a document + chunk + chunks_fts into an in-memory DB.
    /// Returns the chunk_id. `metadata` may be None (NULL) or Some(json_str).
    fn insert_doc_with_chunk(
        conn: &Connection,
        path: &str,
        status: &str,
        updated: &str,
        metadata: Option<&str>,
    ) -> i64 {
        let content = "射撃場のルールについて説明します。";
        conn.execute(
            "INSERT INTO documents
             (file_path, source_type, title, file_hash, indexed_at, status, updated, metadata)
             VALUES (?, 'note', 'T', ?, ?, ?, ?, ?)",
            rusqlite::params![
                path,
                format!("hash-{path}"),
                updated,
                status,
                updated,
                metadata
            ],
        )
        .unwrap();
        let doc_id: i64 = conn
            .query_row(
                "SELECT id FROM documents WHERE file_path = ?",
                rusqlite::params![path],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content)
             VALUES (?, 0, 'T', ?)",
            rusqlite::params![doc_id, content],
        )
        .unwrap();
        let chunk_id: i64 = conn
            .query_row(
                "SELECT id FROM chunks WHERE document_id = ?",
                rusqlite::params![doc_id],
                |row| row.get(0),
            )
            .unwrap();
        let wakachi_text = crate::tokenizer::wakachi(content);
        conn.execute(
            "INSERT INTO chunks_fts(rowid, content) VALUES (?, ?)",
            rusqlite::params![chunk_id, wakachi_text],
        )
        .unwrap();
        chunk_id
    }

    /// No-regression: insert a `superseded` doc (rank 0) and a `current` doc
    /// (rank 1) into chunks_fts. Verify scores reflect the penalty: the
    /// superseded doc should score substantially lower than the current one.
    ///
    /// Insertion order: superseded first → FTS rank 0 (score×0.2 ≥ threshold);
    /// current second → FTS rank 1 (score×1.0 well above threshold).
    #[test]
    fn test_search_applies_score_hook_penalty() {
        let conn = db::get_memory_connection().unwrap();
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();

        // superseded inserted first → FTS rank 0 → score = rrf_0 × 0.2
        // current inserted second → FTS rank 1 → score = rrf_1 × 1.0
        // Since rrf_0 ≈ rrf_1 and penalty = 0.2, superseded << current.
        let meta_sup =
            serde_json::json!({"status":"superseded","effective_date":&today}).to_string();
        let meta_cur = serde_json::json!({"status":"current","effective_date":&today}).to_string();
        insert_doc_with_chunk(&conn, "sup.md", "superseded", &today, Some(&meta_sup));
        insert_doc_with_chunk(&conn, "cur.md", "current", &today, Some(&meta_cur));

        let out = search(&conn, "射撃場", 10, None, false, None).unwrap();
        // Both docs must appear; superseded at rank 0 still passes threshold
        // (rrf_0 × 0.2 = 1.5/60 × 0.2 = 0.005 which is exactly at threshold
        //  and passes the `< SCORE_THRESHOLD` strict-less-than check).
        let sup = out.results.iter().find(|r| r.source_file.contains("sup"));
        let cur = out
            .results
            .iter()
            .find(|r| r.source_file.contains("cur"))
            .unwrap();
        if let Some(sup) = sup {
            assert!(
                sup.score < cur.score * 0.5,
                "superseded should be penalized: cur={} sup={}",
                cur.score,
                sup.score
            );
        } else {
            // superseded score fell below threshold — that also proves penalty was applied,
            // since current doc IS present with a non-trivial score.
            assert!(
                cur.score > 0.005,
                "current doc must score above threshold; got {}",
                cur.score
            );
        }
    }

    /// Synthesis fallback: insert a documents row with metadata=NULL but
    /// status='superseded' and a recent updated date; verify the score is
    /// penalized (~0.2 × current), proving the NULL→synthesized fallback works.
    ///
    /// Same insertion order as above: superseded first (rank 0), current second.
    #[test]
    fn test_search_synthesis_fallback_penalizes_null_metadata() {
        let conn = db::get_memory_connection().unwrap();
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();

        // metadata=NULL for both: status/updated columns must be synthesized.
        insert_doc_with_chunk(&conn, "sup-null.md", "superseded", &today, None);
        insert_doc_with_chunk(&conn, "cur-null.md", "current", &today, None);

        let out = search(&conn, "射撃場", 10, None, false, None).unwrap();
        let cur = out
            .results
            .iter()
            .find(|r| r.source_file.contains("cur-null"))
            .unwrap();
        let sup = out
            .results
            .iter()
            .find(|r| r.source_file.contains("sup-null"));
        if let Some(sup) = sup {
            assert!(
                sup.score < cur.score * 0.5,
                "NULL-metadata superseded should be penalized via synthesis: cur={} sup={}",
                cur.score,
                sup.score
            );
        } else {
            // superseded dropped below threshold — penalty is definitely applied.
            assert!(
                cur.score > 0.005,
                "current doc must score above threshold; got {}",
                cur.score
            );
        }
    }

    /// ADR-0015 guard: search() must succeed on a query_only connection.
    /// A revert of searcher/plan.rs:46 that writes through the serving conn
    /// would cause SQLITE_READONLY here.
    #[test]
    fn test_search_on_read_connection_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        crate::db::init_db(&path).unwrap();
        let conn = crate::db::get_read_connection(&path).unwrap();
        let result = search(&conn, "candle framework", 5, None, false, None);
        assert!(
            result.is_ok(),
            "search on query_only connection must not return SQLITE_READONLY: {:?}",
            result.err()
        );
    }
}
