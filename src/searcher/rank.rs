use rusqlite::Connection;
use serde_json::json;

use crate::temporal::TimeFilter;
use crate::{config, doc_links, lua_hooks};

use super::plan::QueryPlan;
use super::retrieve::CandidateSets;
use super::{SearchOutput, SearchResult};

/// Exponential time decay `0.5^(days / half_life)`. A `half_life` of 0 is the
/// "timeless" sentinel: decay is disabled and full score (1.0) is returned
/// regardless of age, avoiding the `days / 0` blow-up to 0 or NaN.
pub(crate) fn decay_factor(days: f64, half_life: f64) -> f64 {
    if half_life == 0.0 {
        return 1.0;
    }
    0.5_f64.powf(days / half_life)
}

fn snippet(content: &str) -> String {
    let text = match content.split_once('\n') {
        Some((_, rest)) => rest,
        None => content,
    };
    let chars: String = text.chars().take(config::SNIPPET_MAX_CHARS).collect();
    chars.trim().to_string()
}

/// Build the optional time/path `WHERE` fragments and their bound params for
/// the metadata-fetch query. Returns `(time_sql, path_sql, extra_params)`;
/// each SQL fragment is empty when its filter is absent. `extra_params` are
/// ordered time-first, then path, to match the `?` placeholders in the
/// fragments.
fn build_filter_clauses(
    time_filter: Option<&TimeFilter>,
    path_prefixes: Option<&[String]>,
) -> (String, String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    let mut time_clauses = Vec::new();
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

    // Directory-boundary path scope. Built by the shared, pure,
    // unit-tested `paths::scope_clause` so the final JOIN, retrieval, and entity
    // queries all use one source of truth (no divergence between filters).
    let (path_sql, path_params) = crate::paths::scope_clause(path_prefixes);
    for p in path_params {
        extra_params.push(Box::new(p));
    }

    (time_sql, path_sql, extra_params)
}

/// Rank stage: metadata-fetch SQL join, RRF fusion, score hooks, threshold
/// filtering, top-k truncation, and related-doc attachment.
///
/// Returns a `SearchOutput` with results sorted descending by score.
pub(crate) fn rank(
    conn: &Connection,
    plan: &QueryPlan,
    candidates: CandidateSets,
    top_k: usize,
    time_filter: Option<&TimeFilter>,
    path_prefixes: Option<&[String]>,
) -> anyhow::Result<SearchOutput> {
    let cls = &plan.classification;

    let all_chunk_ids: Vec<i64> = candidates
        .fts
        .keys()
        .chain(candidates.vec.keys())
        .chain(candidates.entity.keys())
        .copied()
        .collect::<std::collections::HashSet<i64>>()
        .into_iter()
        .collect();

    if all_chunk_ids.is_empty() {
        return Ok(SearchOutput {
            results: Vec::new(),
            total_hits: 0,
        });
    }

    let placeholders = all_chunk_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");

    let (time_sql, path_sql, extra_params) = build_filter_clauses(time_filter, path_prefixes);

    let sql = format!(
        "SELECT c.id AS chunk_id, c.section_path, c.content,
                d.file_path, d.source_type, d.status, d.updated, d.metadata
         FROM chunks c
         JOIN documents d ON c.document_id = d.id
         WHERE c.id IN ({placeholders}){time_sql}{path_sql}"
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
            row.get::<_, Option<String>>(7)?,
        ))
    })?;

    let hooks = lua_hooks::hooks();
    let mut results = Vec::new();
    for row in rows {
        let (chunk_id, section_path, content, file_path, source_type, status, updated, metadata) =
            row?;

        let mut rrf = 0.0;
        if let Some(&rank) = candidates.fts.get(&chunk_id) {
            rrf += cls.fts_weight / (config::RRF_K + rank as f64);
        }
        if let Some(&rank) = candidates.vec.get(&chunk_id) {
            rrf += cls.vec_weight / (config::RRF_K + rank as f64);
        }
        if let Some(&rank) = candidates.entity.get(&chunk_id) {
            rrf += 1.0 / (config::RRF_K + rank as f64);
        }

        // Build effective metadata JSON: use stored metadata if present;
        // otherwise synthesize from status/updated columns so NULL-metadata
        // rows score identically to pre-feature rows.
        let effective_metadata = metadata.clone().or_else(|| {
            let obj = json!({
                "status": status.as_deref(),
                "effective_date": updated.as_deref(),
            });
            Some(obj.to_string())
        });
        let weight = config::directory_weight(&file_path);
        let half_life = config::half_life_days(&file_path, &source_type);
        let multiplier = lua_hooks::run_score(
            &hooks,
            effective_metadata.as_deref(),
            rrf,
            &source_type,
            &file_path,
            half_life,
        );
        let score = rrf * weight * multiplier;

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
    let total_hits = results.len();
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

    Ok(SearchOutput {
        results,
        total_hits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decay_factor_zero_half_life_disables_decay() {
        // half_life == 0 is the "timeless" sentinel: full score regardless of age.
        assert_eq!(decay_factor(0.0, 0.0), 1.0);
        assert_eq!(decay_factor(10_000.0, 0.0), 1.0);
    }

    #[test]
    fn test_decay_factor_halves_at_one_half_life() {
        assert_eq!(decay_factor(90.0, 90.0), 0.5);
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
}
