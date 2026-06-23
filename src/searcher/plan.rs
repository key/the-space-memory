use rusqlite::Connection;

use crate::{
    classifier, config, embedder, entity, synonyms, tokenizer::extract_search_keywords, user_dict,
};

/// Output of the Plan stage: extracted keywords, classification weights,
/// merged entity + synonym expansions, and the embedded query vector.
pub(crate) struct QueryPlan {
    /// Keyword-only form of the original query (space-joined after extraction).
    pub(crate) keywords_query: String,
    /// RRF weights and matched entity IDs from query classification.
    pub(crate) classification: classifier::QueryClassification,
    /// Merged entity-graph + synonym expansions (deduped).
    pub(crate) expansions: Vec<String>,
    /// Query embedding from the embedder daemon; `None` when unavailable.
    pub(crate) query_vec: Option<Vec<f32>>,
}

/// Plan stage: extract keywords, classify the query, run side-effect calls,
/// expand via entity graph and synonyms, and embed the query for vector search.
///
/// Returns `Ok(None)` if the query contains too few meaningful keywords to
/// search (the caller should return an empty result set). Returns
/// `Ok(Some(QueryPlan))` otherwise.
pub(crate) fn plan(conn: &Connection, query: &str) -> anyhow::Result<Option<QueryPlan>> {
    let keywords = extract_search_keywords(query);
    if keywords.len() < config::MIN_QUERY_KEYWORDS {
        return Ok(None);
    }
    let keywords_query = keywords.join(" ");

    let classification = classifier::classify(conn, &keywords_query);

    // Lazy spawn stale synonym cleanup (once per process)
    synonyms::maybe_spawn_cleanup(config::db_path());

    // Collect query terms as dictionary candidates
    user_dict::collect_from_query(conn, &keywords_query);

    // Expand query: entity graph + synonym dictionary
    let entity_exp = entity::expand_entities_by_ids(
        conn,
        &classification.matched_entity_ids,
        config::MAX_QUERY_EXPANSIONS,
    );
    let synonym_exp = synonyms::expand_query_synonyms(conn, &keywords_query, 3, 0.3);
    let mut expansions = entity_exp;
    for s in synonym_exp {
        if !expansions.contains(&s) {
            expansions.push(s);
        }
    }

    // Embed the query for vector search; None when embedder is unavailable.
    let texts = vec![keywords_query.clone()];
    let query_vec = embedder::embed_via_socket(&texts).and_then(|mut e| {
        if e.is_empty() {
            None
        } else {
            Some(e.remove(0))
        }
    });

    Ok(Some(QueryPlan {
        keywords_query,
        classification,
        expansions,
        query_vec,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// A noise/stopword-only query must yield `None` (too few keywords).
    #[test]
    fn test_plan_noise_query_returns_none() {
        let conn = db::get_memory_connection().unwrap();
        let result = plan(&conn, "なるほど").unwrap();
        assert!(
            result.is_none(),
            "Noise/stopword query should return None from plan()"
        );
    }

    /// An empty query must also yield `None`.
    #[test]
    fn test_plan_empty_query_returns_none() {
        let conn = db::get_memory_connection().unwrap();
        let result = plan(&conn, "").unwrap();
        assert!(
            result.is_none(),
            "Empty query should return None from plan()"
        );
    }

    /// A meaningful query must yield `Some(QueryPlan)` with a non-empty
    /// `keywords_query` and populated classification weights.
    #[test]
    fn test_plan_meaningful_query_returns_some() {
        let conn = db::get_memory_connection().unwrap();
        let result = plan(&conn, "LoRaモジュール 開発").unwrap();
        let qp = result.expect("Meaningful query should return Some(QueryPlan)");
        assert!(
            !qp.keywords_query.is_empty(),
            "keywords_query must be non-empty"
        );
        assert!(
            qp.classification.fts_weight > 0.0,
            "fts_weight must be positive, got {}",
            qp.classification.fts_weight
        );
        assert!(
            qp.classification.vec_weight > 0.0,
            "vec_weight must be positive, got {}",
            qp.classification.vec_weight
        );
    }

    /// A noise-only query like `"よかったーーー"` must also return `None`.
    #[test]
    fn test_plan_interjection_returns_none() {
        let conn = db::get_memory_connection().unwrap();
        let result = plan(&conn, "よかったーーー").unwrap();
        assert!(
            result.is_none(),
            "Interjection query should return None from plan()"
        );
    }
}
