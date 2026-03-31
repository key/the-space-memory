use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use rusqlite::Connection;
use serde::Deserialize;

use crate::db;
use crate::tokenizer::extract_proper_nouns;

#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub name: String,
    pub entity_type: String,
}

// ─── Custom terms dictionary ────────────────────────────────

#[derive(Debug, Deserialize)]
struct CustomTermsFile {
    #[serde(default)]
    terms: Vec<CustomTerm>,
}

#[derive(Debug, Deserialize, Clone)]
struct CustomTerm {
    name: String,
    #[serde(default = "default_type")]
    r#type: String,
}

fn default_type() -> String {
    "custom".to_string()
}

/// A custom term with a pre-compiled matching strategy.
struct CompiledTerm {
    entity: Entity,
    matcher: TermMatcher,
}

enum TermMatcher {
    /// Latin terms: regex with \b word boundary
    Regex(regex::Regex),
    /// CJK terms: simple substring match (word boundaries don't apply)
    Contains(String),
}

impl CompiledTerm {
    fn matches(&self, text: &str) -> bool {
        match &self.matcher {
            TermMatcher::Regex(re) => re.is_match(text),
            TermMatcher::Contains(s) => text.contains(s.as_str()),
        }
    }
}

fn is_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{3000}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' |
            '\u{FF00}'..='\u{FFEF}'
        )
    })
}

/// Loaded once per process. Restart the binary to pick up changes to custom_terms.toml.
static CUSTOM_TERMS: OnceLock<Vec<CompiledTerm>> = OnceLock::new();

/// Load custom terms from TOML file (cached, loaded once per process).
fn get_custom_terms() -> &'static [CompiledTerm] {
    CUSTOM_TERMS
        .get_or_init(load_custom_terms_from_file)
        .as_slice()
}

fn load_custom_terms_from_file() -> Vec<CompiledTerm> {
    let path = crate::config::custom_terms_path();
    if !path.exists() {
        log::info!("custom terms file not found at {}", path.display());
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => compile_custom_terms(&parse_custom_terms(&content)),
        Err(e) => {
            log::warn!("failed to read custom terms: {e}");
            Vec::new()
        }
    }
}

/// Parse custom terms TOML content into entities.
pub fn parse_custom_terms(content: &str) -> Vec<Entity> {
    let file: CustomTermsFile = match toml::from_str(content) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("failed to parse custom terms TOML: {e}");
            return Vec::new();
        }
    };
    file.terms
        .into_iter()
        .filter(|t| t.name.chars().count() >= 2)
        .map(|t| Entity {
            name: normalize(&t.name),
            entity_type: t.r#type,
        })
        .collect()
}

fn compile_custom_terms(entities: &[Entity]) -> Vec<CompiledTerm> {
    entities
        .iter()
        .map(|e| {
            let matcher = if is_cjk(&e.name) {
                TermMatcher::Contains(e.name.clone())
            } else {
                let pattern = format!(r"(?i)\b{}\b", regex::escape(&e.name));
                match regex::Regex::new(&pattern) {
                    Ok(re) => TermMatcher::Regex(re),
                    Err(_) => TermMatcher::Contains(e.name.clone()),
                }
            };
            CompiledTerm {
                entity: e.clone(),
                matcher,
            }
        })
        .collect()
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Extract proper noun entities from chunk text via lindera POS analysis.
pub fn extract_entities(text: &str) -> Vec<Entity> {
    let mut seen = HashSet::new();
    let mut entities: Vec<Entity> = Vec::new();

    // 1. Custom dictionary matches (highest priority, pre-compiled patterns)
    let normalized_text = normalize(text);
    for ct in get_custom_terms() {
        if ct.matches(&normalized_text) && seen.insert(ct.entity.name.clone()) {
            entities.push(ct.entity.clone());
        }
    }

    // 2. Proper nouns from lindera POS analysis
    for surface in extract_proper_nouns(text) {
        let name = normalize(&surface);
        if name.chars().count() >= 2 && seen.insert(name.clone()) {
            entities.push(Entity {
                name,
                entity_type: "proper_noun".to_string(),
            });
        }
    }

    entities
}

/// Convert frontmatter tags to tag entities.
pub fn extract_tags_as_entities(tags: &[String]) -> Vec<Entity> {
    let mut seen = HashSet::new();
    tags.iter()
        .filter_map(|tag| {
            let name = normalize(tag);
            if name.chars().count() >= 2 && seen.insert(name.clone()) {
                Some(Entity {
                    name,
                    entity_type: "tag".to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Upsert entity into DB, returning its id.
fn upsert_entity(conn: &Connection, entity: &Entity) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO entities (name, entity_type) VALUES (?, ?)",
        rusqlite::params![entity.name, entity.entity_type],
    )?;
    let id = conn.query_row(
        "SELECT id FROM entities WHERE name = ? AND entity_type = ?",
        rusqlite::params![entity.name, entity.entity_type],
        |row| row.get(0),
    )?;
    Ok(id)
}

fn insert_chunk_entity(conn: &Connection, chunk_id: i64, entity_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO chunk_entities (chunk_id, entity_id) VALUES (?, ?)",
        rusqlite::params![chunk_id, entity_id],
    )?;
    Ok(())
}

/// Upsert co-occurrence edge (entity_a < entity_b enforced).
fn upsert_edge(conn: &Connection, id_a: i64, id_b: i64, doc_id: i64) -> anyhow::Result<()> {
    let (lo, hi) = if id_a < id_b {
        (id_a, id_b)
    } else {
        (id_b, id_a)
    };
    conn.execute(
        "INSERT INTO entity_edges (entity_a, entity_b, doc_id, occurrences)
         VALUES (?, ?, ?, 1)
         ON CONFLICT(entity_a, entity_b, doc_id) DO UPDATE SET occurrences = occurrences + 1",
        rusqlite::params![lo, hi, doc_id],
    )?;
    Ok(())
}

/// Extract entities from chunks and tags, write to DB.
/// Silent no-op if entity tables don't exist.
pub fn insert_entities(
    conn: &Connection,
    doc_id: i64,
    chunk_entries: &[(i64, String)],
    tags: &[String],
) -> anyhow::Result<()> {
    if !db::has_entity_tables(conn) {
        return Ok(());
    }

    let tag_entities = extract_tags_as_entities(tags);
    let mut tag_ids: Vec<i64> = Vec::new();
    for entity in &tag_entities {
        tag_ids.push(upsert_entity(conn, entity)?);
    }

    // Tag-to-tag edges: document-level, generated once (not per-chunk)
    for i in 0..tag_ids.len() {
        for j in (i + 1)..tag_ids.len() {
            if tag_ids[i] != tag_ids[j] {
                upsert_edge(conn, tag_ids[i], tag_ids[j], doc_id)?;
            }
        }
    }

    for (chunk_id, content) in chunk_entries {
        let content_entities = extract_entities(content);
        let mut content_ids: Vec<i64> = Vec::new();

        for entity in &content_entities {
            let eid = upsert_entity(conn, entity)?;
            insert_chunk_entity(conn, *chunk_id, eid)?;
            content_ids.push(eid);
        }

        // Associate tags with each chunk
        for &eid in &tag_ids {
            insert_chunk_entity(conn, *chunk_id, eid)?;
        }

        // Content-to-content edges within this chunk
        for i in 0..content_ids.len() {
            for j in (i + 1)..content_ids.len() {
                if content_ids[i] != content_ids[j] {
                    upsert_edge(conn, content_ids[i], content_ids[j], doc_id)?;
                }
            }
        }

        // Content-to-tag edges within this chunk
        for &cid in &content_ids {
            for &tid in &tag_ids {
                if cid != tid {
                    upsert_edge(conn, cid, tid, doc_id)?;
                }
            }
        }
    }

    Ok(())
}

// ─── Search integration ─────────────────────────────────────────

/// Find entity IDs matching normalized query terms.
fn find_entity_ids(conn: &Connection, query: &str) -> Vec<i64> {
    let entities = extract_entities(query);
    let tag_entities = extract_tags_as_entities(
        &query
            .split_whitespace()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    );

    let mut names: HashSet<String> = HashSet::new();
    for e in entities.iter().chain(tag_entities.iter()) {
        names.insert(e.name.clone());
    }
    // Also try the full query as a single entity name
    let normalized_query = normalize(query);
    if normalized_query.chars().count() >= 2 {
        names.insert(normalized_query);
    }

    let mut ids = Vec::new();
    for name in &names {
        if let Ok(id) = conn.query_row(
            "SELECT id FROM entities WHERE name = ?",
            rusqlite::params![name],
            |row| row.get::<_, i64>(0),
        ) {
            ids.push(id);
        }
    }
    ids
}

/// Search for chunks related to entities found in the query.
/// Returns chunk_id → rank (lower rank = more relevant).
pub fn entity_results(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> anyhow::Result<HashMap<i64, usize>> {
    let entity_ids = find_entity_ids(conn, query);
    entity_results_by_ids(conn, &entity_ids, limit)
}

/// Search for chunks by pre-computed entity IDs (avoids redundant lookup).
pub fn entity_results_by_ids(
    conn: &Connection,
    entity_ids: &[i64],
    limit: usize,
) -> anyhow::Result<HashMap<i64, usize>> {
    if entity_ids.is_empty() || !db::has_entity_tables(conn) {
        return Ok(HashMap::new());
    }

    // Direct matches: chunks containing these entities
    let placeholders = entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT ce.chunk_id, COUNT(*) as match_count
         FROM chunk_entities ce
         WHERE ce.entity_id IN ({placeholders})
         GROUP BY ce.chunk_id
         ORDER BY match_count DESC
         LIMIT ?",
    );

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = entity_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))?;

    let mut result = HashMap::new();
    for (rank, row) in rows.enumerate() {
        if let Ok(chunk_id) = row {
            result.insert(chunk_id, rank);
        }
    }

    // 2nd hop: chunks related via co-occurring entities (lower priority)
    let related_ids = find_related_entity_ids(conn, entity_ids, 5);
    if !related_ids.is_empty() {
        let ph2 = related_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql2 = format!(
            "SELECT ce.chunk_id, COUNT(*) as match_count
             FROM chunk_entities ce
             WHERE ce.entity_id IN ({ph2})
               AND ce.chunk_id NOT IN (SELECT chunk_id FROM chunk_entities WHERE entity_id IN ({placeholders}))
             GROUP BY ce.chunk_id
             ORDER BY match_count DESC
             LIMIT ?",
        );

        let mut params2: Vec<Box<dyn rusqlite::types::ToSql>> = related_ids
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        for id in entity_ids {
            params2.push(Box::new(*id));
        }
        params2.push(Box::new(limit as i64));
        let refs2: Vec<&dyn rusqlite::types::ToSql> = params2.iter().map(|p| p.as_ref()).collect();

        let mut stmt2 = conn.prepare(&sql2)?;
        let rows2 = stmt2.query_map(refs2.as_slice(), |row| row.get::<_, i64>(0))?;

        let base_rank = result.len();
        for (i, row) in rows2.enumerate() {
            if let Ok(chunk_id) = row {
                result.entry(chunk_id).or_insert(base_rank + i);
            }
        }
    }

    Ok(result)
}

/// Find entity IDs that co-occur with the given entities (1 hop via entity_edges).
fn find_related_entity_ids(conn: &Connection, entity_ids: &[i64], limit: usize) -> Vec<i64> {
    if entity_ids.is_empty() {
        return Vec::new();
    }

    let placeholders = entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT CASE WHEN entity_a IN ({placeholders}) THEN entity_b ELSE entity_a END AS related_id,
                SUM(occurrences) AS total_occ
         FROM entity_edges
         WHERE entity_a IN ({placeholders}) OR entity_b IN ({placeholders})
         GROUP BY related_id
         HAVING related_id NOT IN ({placeholders})
         ORDER BY total_occ DESC
         LIMIT ?",
    );

    // entity_ids repeated 4 times in the query
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    for _ in 0..4 {
        for id in entity_ids {
            params.push(Box::new(*id));
        }
    }
    params.push(Box::new(limit as i64));
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    conn.prepare(&sql)
        .and_then(|mut stmt| {
            let rows = stmt.query_map(refs.as_slice(), |row| row.get::<_, i64>(0))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}

/// Expand query with related entity names from the entity graph.
/// Returns up to `max_expansions` related entity names.
pub fn expand_query_entities(conn: &Connection, query: &str, max_expansions: usize) -> Vec<String> {
    let entity_ids = find_entity_ids(conn, query);
    expand_entities_by_ids(conn, &entity_ids, max_expansions)
}

/// Expand entities by pre-computed IDs (avoids redundant entity lookup).
pub fn expand_entities_by_ids(
    conn: &Connection,
    entity_ids: &[i64],
    max_expansions: usize,
) -> Vec<String> {
    if entity_ids.is_empty() || !db::has_entity_tables(conn) {
        return Vec::new();
    }

    let related_ids = find_related_entity_ids(conn, entity_ids, max_expansions);
    if related_ids.is_empty() {
        return Vec::new();
    }

    let placeholders = related_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT name FROM entities WHERE id IN ({placeholders})");
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = related_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    conn.prepare(&sql)
        .and_then(|mut stmt| {
            let rows = stmt.query_map(refs.as_slice(), |row| row.get::<_, String>(0))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_entities_empty() {
        assert!(extract_entities("").is_empty());
    }

    #[test]
    fn test_extract_entities_dedup() {
        let entities = extract_entities("東京は東京タワーがある東京の街です");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "should be deduplicated");
    }

    #[test]
    fn test_extract_entities_normalized_lowercase() {
        let entities = extract_tags_as_entities(&["Rust".to_string(), "SQLite".to_string()]);
        assert!(entities.iter().any(|e| e.name == "rust"));
        assert!(entities.iter().any(|e| e.name == "sqlite"));
    }

    #[test]
    fn test_extract_entities_min_length() {
        let entities = extract_tags_as_entities(&["A".to_string(), "ab".to_string()]);
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "ab");
    }

    #[test]
    fn test_extract_tags_as_entities() {
        let tags = vec!["Rust".to_string(), "検索".to_string()];
        let entities = extract_tags_as_entities(&tags);
        assert_eq!(entities.len(), 2);
        assert!(entities.iter().all(|e| e.entity_type == "tag"));
        assert!(entities.iter().any(|e| e.name == "rust"));
        assert!(entities.iter().any(|e| e.name == "検索"));
    }

    #[test]
    fn test_extract_tags_dedup() {
        let tags = vec!["Rust".to_string(), "rust".to_string()];
        let entities = extract_tags_as_entities(&tags);
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn test_insert_entities_no_table() {
        let conn = db::get_memory_connection().unwrap();
        // Drop entity tables to simulate missing tables
        conn.execute_batch("DROP TABLE IF EXISTS entity_edges; DROP TABLE IF EXISTS chunk_entities; DROP TABLE IF EXISTS entities;").unwrap();

        let result = insert_entities(&conn, 1, &[], &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_insert_entities_writes_db() {
        let conn = db::get_memory_connection().unwrap();

        // Insert a document and chunk first
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at) VALUES ('test.md', 'note', 'Test', 'hash', '2026-01-01')",
            [],
        ).unwrap();
        let doc_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content) VALUES (?, 0, 'Test', 'テスト')",
            [doc_id],
        ).unwrap();
        let chunk_id = conn.last_insert_rowid();

        let tags = vec!["Rust".to_string(), "検索".to_string()];
        let chunk_entries = vec![(chunk_id, "テスト内容です".to_string())];

        insert_entities(&conn, doc_id, &chunk_entries, &tags).unwrap();

        let entity_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
            .unwrap();
        assert!(entity_count >= 2, "At least 2 tag entities should exist");

        let ce_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_entities", [], |r| r.get(0))
            .unwrap();
        assert!(ce_count >= 2, "At least 2 chunk-entity links should exist");
    }

    #[test]
    fn test_upsert_deduplicates_entity() {
        let conn = db::get_memory_connection().unwrap();

        let entity = Entity {
            name: "rust".to_string(),
            entity_type: "tag".to_string(),
        };
        let id1 = upsert_entity(&conn, &entity).unwrap();
        let id2 = upsert_entity(&conn, &entity).unwrap();
        assert_eq!(id1, id2);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE name = 'rust'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_edge_ordering() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at) VALUES ('t.md', 'note', 'T', 'h', '2026-01-01')",
            [],
        ).unwrap();
        let doc_id = conn.last_insert_rowid();

        let e1 = Entity {
            name: "aaa".to_string(),
            entity_type: "tag".to_string(),
        };
        let e2 = Entity {
            name: "zzz".to_string(),
            entity_type: "tag".to_string(),
        };
        let id1 = upsert_entity(&conn, &e1).unwrap();
        let id2 = upsert_entity(&conn, &e2).unwrap();

        // Insert with id2 first — should still store (lo, hi) = (id1, id2)
        upsert_edge(&conn, id2, id1, doc_id).unwrap();

        let (a, b): (i64, i64) = conn
            .query_row("SELECT entity_a, entity_b FROM entity_edges", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(a < b);
    }

    #[test]
    fn test_edge_occurrences_increments() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at) VALUES ('t.md', 'note', 'T', 'h', '2026-01-01')",
            [],
        ).unwrap();
        let doc_id = conn.last_insert_rowid();

        let e1 = Entity {
            name: "aaa".to_string(),
            entity_type: "tag".to_string(),
        };
        let e2 = Entity {
            name: "bbb".to_string(),
            entity_type: "tag".to_string(),
        };
        let id1 = upsert_entity(&conn, &e1).unwrap();
        let id2 = upsert_entity(&conn, &e2).unwrap();

        upsert_edge(&conn, id1, id2, doc_id).unwrap();
        upsert_edge(&conn, id1, id2, doc_id).unwrap();

        let occ: i64 = conn
            .query_row(
                "SELECT occurrences FROM entity_edges WHERE entity_a = ? AND entity_b = ?",
                rusqlite::params![id1, id2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(occ, 2);
    }

    #[test]
    fn test_tags_linked_to_all_chunks() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at) VALUES ('t.md', 'note', 'T', 'h', '2026-01-01')",
            [],
        ).unwrap();
        let doc_id = conn.last_insert_rowid();

        // Two chunks
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content) VALUES (?, 0, 'S1', 'content1')",
            [doc_id],
        ).unwrap();
        let cid1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content) VALUES (?, 1, 'S2', 'content2')",
            [doc_id],
        ).unwrap();
        let cid2 = conn.last_insert_rowid();

        let tags = vec!["Rust".to_string()];
        let chunk_entries = vec![
            (cid1, "content1".to_string()),
            (cid2, "content2".to_string()),
        ];

        insert_entities(&conn, doc_id, &chunk_entries, &tags).unwrap();

        // Tag entity should be linked to both chunks
        let links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunk_entities ce JOIN entities e ON ce.entity_id = e.id WHERE e.name = 'rust'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(links, 2);
    }

    #[test]
    fn test_normalize() {
        assert_eq!(normalize("  Rust  "), "rust");
        assert_eq!(normalize("SQLite"), "sqlite");
        assert_eq!(normalize("東京"), "東京");
    }

    // ─── search integration tests ────────────────────────────

    fn setup_graph(conn: &Connection) -> (i64, i64, i64) {
        conn.execute(
            "INSERT INTO documents (file_path, source_type, title, file_hash, indexed_at) VALUES ('t.md', 'note', 'T', 'h', '2026-01-01')",
            [],
        ).unwrap();
        let doc_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content) VALUES (?, 0, 'S1', 'chunk1')",
            [doc_id],
        ).unwrap();
        let cid1 = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, section_path, content) VALUES (?, 1, 'S2', 'chunk2')",
            [doc_id],
        ).unwrap();
        let cid2 = conn.last_insert_rowid();

        // Insert entities and link to chunks
        let tags = vec!["Rust".to_string(), "SQLite".to_string()];
        let chunk_entries = vec![(cid1, "chunk1".to_string()), (cid2, "chunk2".to_string())];
        insert_entities(conn, doc_id, &chunk_entries, &tags).unwrap();

        (doc_id, cid1, cid2)
    }

    #[test]
    fn test_entity_results_finds_chunks() {
        let conn = db::get_memory_connection().unwrap();
        let (_doc_id, cid1, cid2) = setup_graph(&conn);

        let results = entity_results(&conn, "rust", 10).unwrap();
        assert!(!results.is_empty());
        assert!(results.contains_key(&cid1) || results.contains_key(&cid2));
    }

    #[test]
    fn test_entity_results_empty_query() {
        let conn = db::get_memory_connection().unwrap();
        let results = entity_results(&conn, "", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_entity_results_no_match() {
        let conn = db::get_memory_connection().unwrap();
        setup_graph(&conn);

        let results = entity_results(&conn, "nonexistent", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_entity_results_no_tables() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute_batch(
            "DROP TABLE IF EXISTS entity_edges; DROP TABLE IF EXISTS chunk_entities; DROP TABLE IF EXISTS entities;",
        ).unwrap();

        let results = entity_results(&conn, "rust", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_expand_query_entities() {
        let conn = db::get_memory_connection().unwrap();
        setup_graph(&conn);

        // "rust" should expand to related entities (e.g., "sqlite" via co-occurrence)
        let expansions = expand_query_entities(&conn, "rust", 5);
        // sqlite should be in expansions since it co-occurs with rust
        assert!(
            expansions.iter().any(|e| e == "sqlite"),
            "expected 'sqlite' in expansions: {expansions:?}"
        );
    }

    #[test]
    fn test_expand_query_no_match() {
        let conn = db::get_memory_connection().unwrap();
        setup_graph(&conn);

        let expansions = expand_query_entities(&conn, "nonexistent", 5);
        assert!(expansions.is_empty());
    }

    #[test]
    fn test_expand_query_no_tables() {
        let conn = db::get_memory_connection().unwrap();
        conn.execute_batch(
            "DROP TABLE IF EXISTS entity_edges; DROP TABLE IF EXISTS chunk_entities; DROP TABLE IF EXISTS entities;",
        ).unwrap();

        let expansions = expand_query_entities(&conn, "rust", 5);
        assert!(expansions.is_empty());
    }

    #[test]
    fn test_find_related_entity_ids() {
        let conn = db::get_memory_connection().unwrap();
        setup_graph(&conn);

        let rust_id: i64 = conn
            .query_row("SELECT id FROM entities WHERE name = 'rust'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let related = find_related_entity_ids(&conn, &[rust_id], 5);
        assert!(
            !related.is_empty(),
            "should find related entities via edges"
        );
    }

    #[test]
    fn test_find_related_entity_ids_empty() {
        let conn = db::get_memory_connection().unwrap();
        let related = find_related_entity_ids(&conn, &[], 5);
        assert!(related.is_empty());
    }

    // ─── custom terms tests ──────────────────────────────────

    #[test]
    fn test_parse_custom_terms() {
        let toml = r#"
[[terms]]
name = "candle"
type = "tech"

[[terms]]
name = "lindera"
type = "tech"

[[terms]]
name = "x"
type = "tech"
"#;
        let terms = parse_custom_terms(toml);
        assert_eq!(terms.len(), 2); // "x" filtered (< 2 chars)
        assert!(terms.iter().any(|t| t.name == "candle"));
        assert!(terms.iter().any(|t| t.name == "lindera"));
    }

    #[test]
    fn test_parse_custom_terms_default_type() {
        let toml = r#"
[[terms]]
name = "candle"
"#;
        let terms = parse_custom_terms(toml);
        assert_eq!(terms[0].entity_type, "custom");
    }

    #[test]
    fn test_parse_custom_terms_empty() {
        let terms = parse_custom_terms("");
        assert!(terms.is_empty());
    }

    #[test]
    fn test_parse_custom_terms_invalid() {
        let terms = parse_custom_terms("not valid toml {{{");
        assert!(terms.is_empty());
    }

    #[test]
    fn test_extract_entities_with_custom() {
        // This test verifies custom terms are matched in text
        let toml = r#"
[[terms]]
name = "candle"
type = "tech"
"#;
        let terms = parse_custom_terms(toml);
        assert!(!terms.is_empty());

        // Custom terms match via substring in extract_entities
        // (depends on OnceLock state, so we test parse_custom_terms directly)
    }
}
