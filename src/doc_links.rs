use std::collections::HashSet;

use regex::Regex;
use rusqlite::Connection;

use crate::db;

/// A related document found via document links.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelatedDoc {
    pub file_path: String,
    pub link_type: String,
    pub strength: f64,
}

/// Build document links for a newly indexed document.
/// Discovers: explicit Markdown links, shared tags, entity co-occurrence.
pub fn build_links(conn: &Connection, doc_id: i64, content: &str, tags: &[String]) {
    if !db::has_doc_links_table(conn) {
        return;
    }

    build_explicit_links(conn, doc_id, content);
    build_tag_links(conn, doc_id, tags);
    build_entity_links(conn, doc_id);
}

/// Delete all links involving a document (before re-index).
pub fn delete_links(conn: &Connection, doc_id: i64) {
    let _ = conn.execute(
        "DELETE FROM document_links WHERE doc_a_id = ? OR doc_b_id = ?",
        rusqlite::params![doc_id, doc_id],
    );
}

/// Find related documents for a set of document IDs.
pub fn find_related(conn: &Connection, doc_ids: &[i64], limit: usize) -> Vec<RelatedDoc> {
    if doc_ids.is_empty() || !db::has_doc_links_table(conn) {
        return Vec::new();
    }

    let placeholders = doc_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT d.file_path, dl.link_type, MAX(dl.strength) as max_strength
         FROM document_links dl
         JOIN documents d ON d.id = CASE
             WHEN dl.doc_a_id IN ({ph}) THEN dl.doc_b_id
             ELSE dl.doc_a_id
         END
         WHERE (dl.doc_a_id IN ({ph}) OR dl.doc_b_id IN ({ph}))
           AND d.id NOT IN ({ph})
         GROUP BY d.file_path, dl.link_type
         ORDER BY max_strength DESC
         LIMIT ?",
        ph = placeholders,
    );

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    // 4 times for the 4 placeholders in the query
    for _ in 0..4 {
        for id in doc_ids {
            params.push(Box::new(*id));
        }
    }
    params.push(Box::new(limit as i64));
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    conn.prepare(&sql)
        .and_then(|mut stmt| {
            let rows = stmt.query_map(refs.as_slice(), |row| {
                Ok(RelatedDoc {
                    file_path: row.get(0)?,
                    link_type: row.get(1)?,
                    strength: row.get(2)?,
                })
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}

// ─── Link builders ──────────────────────────────────────────

fn upsert_link(conn: &Connection, id_a: i64, id_b: i64, link_type: &str, strength: f64) {
    if id_a == id_b {
        return;
    }
    let (lo, hi) = if id_a < id_b {
        (id_a, id_b)
    } else {
        (id_b, id_a)
    };
    let _ = conn.execute(
        "INSERT INTO document_links (doc_a_id, doc_b_id, link_type, strength)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(doc_a_id, doc_b_id, link_type) DO UPDATE SET strength = MAX(strength, excluded.strength)",
        rusqlite::params![lo, hi, link_type, strength],
    );
}

/// Extract Markdown links [text](path) and find matching documents.
fn build_explicit_links(conn: &Connection, doc_id: i64, content: &str) {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[([^\]]*)\]\(([^)]+)\)").unwrap());

    for caps in re.captures_iter(content) {
        let path = &caps[2];
        // Skip external URLs
        if path.starts_with("http://") || path.starts_with("https://") {
            continue;
        }
        // Try to find the target document by file_path suffix
        let target_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM documents WHERE file_path LIKE ?",
                rusqlite::params![format!("%{path}")],
                |row| row.get(0),
            )
            .ok();
        if let Some(tid) = target_id {
            upsert_link(conn, doc_id, tid, "explicit", 1.0);
        }
    }
}

/// Find documents sharing frontmatter tags.
fn build_tag_links(conn: &Connection, doc_id: i64, tags: &[String]) {
    if tags.is_empty() {
        return;
    }

    let normalized_tags: Vec<String> = tags.iter().map(|t| t.trim().to_lowercase()).collect();

    // Find other documents with overlapping tags
    let all_docs: Vec<(i64, String)> = conn
        .prepare("SELECT id, tags FROM documents WHERE id != ? AND tags IS NOT NULL")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([doc_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    for (other_id, other_tags_str) in &all_docs {
        let other_tags = parse_tags_str(other_tags_str);
        let shared: usize = normalized_tags
            .iter()
            .filter(|t| other_tags.contains(&t.as_str()))
            .count();
        if shared > 0 {
            let max_tags = normalized_tags.len().max(other_tags.len()).max(1);
            let strength = 0.7 * (shared as f64 / max_tags as f64);
            upsert_link(conn, doc_id, *other_id, "tag", strength);
        }
    }
}

/// Parse tags from the Rust Debug format stored in documents.tags.
/// Format: ["tag1", "tag2"] or similar.
fn parse_tags_str(s: &str) -> HashSet<&str> {
    let s = s.trim();
    let s = s.strip_prefix('[').unwrap_or(s);
    let s = s.strip_suffix(']').unwrap_or(s);
    s.split(',')
        .map(|t| t.trim().trim_matches('"').trim())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Build entity co-occurrence links between documents sharing entities.
fn build_entity_links(conn: &Connection, doc_id: i64) {
    if !db::has_entity_tables(conn) {
        return;
    }

    // Find other documents that share entities via chunk_entities
    let related: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT c2.document_id, COUNT(DISTINCT ce1.entity_id) as shared
             FROM chunk_entities ce1
             JOIN chunks c1 ON c1.id = ce1.chunk_id AND c1.document_id = ?
             JOIN chunk_entities ce2 ON ce2.entity_id = ce1.entity_id
             JOIN chunks c2 ON c2.id = ce2.chunk_id AND c2.document_id != ?
             GROUP BY c2.document_id
             HAVING shared >= 2
             ORDER BY shared DESC
             LIMIT 20",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map(rusqlite::params![doc_id, doc_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    for (other_id, shared) in &related {
        let strength = ((*shared as f64).ln() / 10.0).min(0.6);
        if strength > 0.05 {
            upsert_link(conn, doc_id, *other_id, "entity", strength);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn setup() -> Connection {
        db::get_memory_connection().unwrap()
    }

    fn insert_doc(conn: &Connection, path: &str, tags: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at, tags)
             VALUES (?, 'note', ?, 'hash', '2026-01-01', ?)",
            rusqlite::params![path, path, tags],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn test_upsert_link() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", None);
        let b = insert_doc(&conn, "b.md", None);

        upsert_link(&conn, a, b, "tag", 0.5);
        upsert_link(&conn, a, b, "tag", 0.7); // Should update to higher strength

        let strength: f64 = conn
            .query_row(
                "SELECT strength FROM document_links WHERE doc_a_id = ? AND doc_b_id = ?",
                rusqlite::params![a, b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(strength, 0.7);
    }

    #[test]
    fn test_upsert_link_ordering() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", None);
        let b = insert_doc(&conn, "b.md", None);

        // Insert with b,a order — should still store (a,b)
        upsert_link(&conn, b, a, "tag", 0.5);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let (lo, hi): (i64, i64) = conn
            .query_row("SELECT doc_a_id, doc_b_id FROM document_links", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(lo < hi);
    }

    #[test]
    fn test_upsert_self_link_ignored() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", None);
        upsert_link(&conn, a, a, "tag", 1.0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_build_tag_links() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", Some(r#"["rust", "sqlite"]"#));
        let _b = insert_doc(&conn, "b.md", Some(r#"["rust", "search"]"#));
        let _c = insert_doc(&conn, "c.md", Some(r#"["python"]"#));

        build_tag_links(&conn, a, &["rust".to_string(), "sqlite".to_string()]);

        // a and b share "rust"
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_links WHERE link_type = 'tag'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(count >= 1);
    }

    #[test]
    fn test_build_explicit_links() {
        let conn = setup();
        let a = insert_doc(&conn, "daily/notes/a.md", None);
        let _b = insert_doc(&conn, "company/research/report.md", None);

        let content = "See [report](company/research/report.md) for details.";
        build_explicit_links(&conn, a, content);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_links WHERE link_type = 'explicit'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_build_explicit_links_skip_urls() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", None);

        let content = "See [Google](https://google.com) and [docs](http://example.com).";
        build_explicit_links(&conn, a, content);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_links WHERE link_type = 'explicit'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_delete_links() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", None);
        let b = insert_doc(&conn, "b.md", None);

        upsert_link(&conn, a, b, "tag", 0.7);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM document_links", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );

        delete_links(&conn, a);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM document_links", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_find_related() {
        let conn = setup();
        let a = insert_doc(&conn, "a.md", None);
        let b = insert_doc(&conn, "b.md", None);
        let c = insert_doc(&conn, "c.md", None);

        upsert_link(&conn, a, b, "tag", 0.7);
        upsert_link(&conn, a, c, "explicit", 1.0);

        let related = find_related(&conn, &[a], 10);
        assert!(!related.is_empty());
        assert!(related
            .iter()
            .any(|r| r.file_path == "b.md" || r.file_path == "c.md"));
    }

    #[test]
    fn test_find_related_empty() {
        let conn = setup();
        let related = find_related(&conn, &[], 10);
        assert!(related.is_empty());
    }

    #[test]
    fn test_parse_tags_str() {
        let tags = parse_tags_str(r#"["rust", "sqlite", "search"]"#);
        assert_eq!(tags.len(), 3);
        assert!(tags.contains("rust"));
        assert!(tags.contains("sqlite"));
    }

    #[test]
    fn test_parse_tags_str_empty() {
        assert!(parse_tags_str("").is_empty());
        assert!(parse_tags_str("[]").is_empty());
    }

    #[test]
    fn test_no_table() {
        let conn = setup();
        conn.execute_batch("DROP TABLE IF EXISTS document_links;")
            .unwrap();

        // Should not panic
        build_links(&conn, 1, "content", &[]);
        let related = find_related(&conn, &[1], 10);
        assert!(related.is_empty());
    }
}
