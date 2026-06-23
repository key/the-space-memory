use std::path::Path;

use super::SearchResult;

/// Render search results as human-readable text.
///
/// Returns a formatted multi-line string suitable for terminal output.
/// When `results` is empty, returns "No results found.".
pub fn format_text(results: &[SearchResult], total_hits: usize) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }
    let mut out = format!("Showing {} of {} results\n\n", results.len(), total_hits);
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {} — {} (score: {:.4})\n",
            i + 1,
            r.source_type,
            r.source_file,
            r.section_path,
            r.score
        ));
        out.push_str(&format!("   {}\n", r.snippet));
        if let Some(ref status) = r.status {
            out.push_str(&format!("   status: {status}\n"));
        }
        if !r.related_docs.is_empty() {
            out.push_str("   related:\n");
            for rd in &r.related_docs {
                out.push_str(&format!(
                    "     - [{}] {} (strength: {:.2})\n",
                    rd.link_type, rd.file_path, rd.strength
                ));
            }
        }
        out.push('\n');
    }
    out
}

/// Render search results as a pretty-printed JSON string.
///
/// Optionally reads the full file content for the first `include_content`
/// results when `include_content` is `Some(n)`. Missing files are silently
/// skipped (no `content` key in output).
pub fn format_json(
    results: &[SearchResult],
    total_hits: usize,
    include_content: Option<usize>,
    project_root: &Path,
) -> anyhow::Result<String> {
    let mut json_results: Vec<serde_json::Value> = Vec::new();

    for (i, r) in results.iter().enumerate() {
        let related: Vec<serde_json::Value> = r
            .related_docs
            .iter()
            .map(|rd| {
                serde_json::json!({
                    "file_path": rd.file_path,
                    "link_type": rd.link_type,
                    "strength": rd.strength,
                })
            })
            .collect();

        let mut obj = serde_json::json!({
            "source_file": r.source_file,
            "source_type": r.source_type,
            "section_path": r.section_path,
            "snippet": r.snippet,
            "score": r.score,
            "status": r.status,
            "related_docs": related,
        });

        if let Some(n) = include_content {
            if i < n {
                let full_path = project_root.join(&r.source_file);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    obj["content"] = serde_json::Value::String(content);
                }
            }
        }

        json_results.push(obj);
    }

    let envelope = serde_json::json!({
        "total_hits": total_hits,
        "results": json_results,
    });

    Ok(serde_json::to_string_pretty(&envelope)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_text_empty() {
        let result = format_text(&[], 0);
        assert_eq!(result, "No results found.");
    }

    #[test]
    fn test_format_text_with_results() {
        let results = vec![SearchResult {
            source_file: "daily/notes/test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Test > Section".to_string(),
            snippet: "Some content".to_string(),
            score: 0.5,
            status: Some("current".to_string()),
            related_docs: vec![],
        }];
        let text = format_text(&results, results.len());
        assert!(text.contains("1. [note]"));
        assert!(text.contains("daily/notes/test.md"));
        assert!(text.contains("0.5000"));
        assert!(text.contains("status: current"));
    }

    #[test]
    fn test_format_text_no_status() {
        let results = vec![SearchResult {
            source_file: "test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.3,
            status: None,
            related_docs: vec![],
        }];
        let text = format_text(&results, results.len());
        assert!(!text.contains("status:"));
    }

    #[test]
    fn test_format_text_multiple_results() {
        let results = vec![
            SearchResult {
                source_file: "a.md".to_string(),
                source_type: "note".to_string(),
                section_path: "A".to_string(),
                snippet: "aaa".to_string(),
                score: 0.5,
                status: None,
                related_docs: vec![],
            },
            SearchResult {
                source_file: "b.md".to_string(),
                source_type: "research".to_string(),
                section_path: "B".to_string(),
                snippet: "bbb".to_string(),
                score: 0.3,
                status: Some("outdated".to_string()),
                related_docs: vec![],
            },
        ];
        let text = format_text(&results, results.len());
        assert!(text.contains("1. [note]"));
        assert!(text.contains("2. [research]"));
        assert!(text.contains("status: outdated"));
        assert!(!text.contains("No results found"));
    }

    #[test]
    fn test_format_json_empty() {
        let result = format_json(&[], 0, None, Path::new("/tmp")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["total_hits"], 0);
        assert_eq!(parsed["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_format_json_with_results() {
        let results = vec![SearchResult {
            source_file: "test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.5,
            status: None,
            related_docs: vec![],
        }];
        let json = format_json(&results, 10, None, Path::new("/tmp")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total_hits"], 10);
        let arr = parsed["results"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["source_file"], "test.md");
        assert_eq!(arr[0]["score"], 0.5);
        // No content field when include_content is None
        assert!(arr[0].get("content").is_none());
    }

    #[test]
    fn test_format_json_with_include_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.md");
        std::fs::write(&file_path, "# Hello\n\nWorld.").unwrap();

        let results = vec![SearchResult {
            source_file: "test.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.5,
            status: None,
            related_docs: vec![],
        }];
        let json = format_json(&results, 1, Some(1), dir.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["results"][0]["content"], "# Hello\n\nWorld.");
    }

    #[test]
    fn test_format_json_include_content_file_missing() {
        let results = vec![SearchResult {
            source_file: "nonexistent.md".to_string(),
            source_type: "note".to_string(),
            section_path: "Section".to_string(),
            snippet: "Content".to_string(),
            score: 0.5,
            status: None,
            related_docs: vec![],
        }];
        // File doesn't exist, so content field should not be present
        let json = format_json(&results, 1, Some(1), Path::new("/tmp/empty")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["results"][0].get("content").is_none());
    }

    #[test]
    fn test_format_json_include_content_beyond_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.md"), "aaa").unwrap();
        std::fs::write(dir.path().join("b.md"), "bbb").unwrap();

        let results = vec![
            SearchResult {
                source_file: "a.md".to_string(),
                source_type: "note".to_string(),
                section_path: "A".to_string(),
                snippet: "aaa".to_string(),
                score: 0.5,
                status: None,
                related_docs: vec![],
            },
            SearchResult {
                source_file: "b.md".to_string(),
                source_type: "note".to_string(),
                section_path: "B".to_string(),
                snippet: "bbb".to_string(),
                score: 0.4,
                status: None,
                related_docs: vec![],
            },
        ];
        // include_content=1, so only first result gets content
        let json = format_json(&results, 2, Some(1), dir.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed["results"].as_array().unwrap();
        assert_eq!(arr[0]["content"], "aaa");
        assert!(arr[1].get("content").is_none());
    }
}
