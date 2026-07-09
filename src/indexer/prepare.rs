//! Prepare stage: file -> PreparedFile. Pure (file IO only, no DB).

use std::path::Path;

use crate::chunker::chunk_markdown_default;
use crate::{config, frontmatter, lua_hooks};

use super::chunk_hash;

pub(crate) struct ChunkInput {
    pub chunk_index: usize,
    pub section_path: String,
    pub content: String,
    pub content_hash: String,
}

/// Per-source pipeline participation policy. Markdown documents join every
/// side index; sessions are searchable text only (no entity co-occurrence,
/// no link graph, no per-chunk dictionary candidate collection — their
/// dictionary learning happens on user messages via `learn_from_session_jsonl`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourcePolicy {
    pub entity_graph: bool,
    pub doc_links: bool,
    pub dict_candidates: bool,
}

impl SourcePolicy {
    pub(crate) fn markdown() -> Self {
        Self {
            entity_graph: true,
            doc_links: true,
            dict_candidates: true,
        }
    }

    pub(crate) fn session() -> Self {
        Self {
            entity_graph: false,
            doc_links: false,
            dict_candidates: false,
        }
    }
}

/// Identity and policy for one Prepare invocation. Source transforms
/// (e.g. session JSONL → Markdown) set `source_type` to override the
/// directory-derived classification, since their `rel_path` is a synthetic
/// key (`session:<stem>`) outside the content directories.
pub(crate) struct PrepareContext<'a> {
    pub rel_path: &'a str,
    pub directory: &'a str,
    pub filename: &'a str,
    pub source_type: Option<&'a str>,
    pub policy: SourcePolicy,
}

pub(crate) struct PreparedFile {
    pub source_type: String,
    pub title: String,
    pub frontmatter: frontmatter::Frontmatter,
    pub tags_str: Option<String>,
    pub metadata_json: String,
    pub chunk_inputs: Vec<ChunkInput>,
    pub text: String,
    pub policy: SourcePolicy,
}

/// Prepare stage body: Markdown text -> PreparedFile. Pure (no I/O), so every
/// source that can serialize itself to Markdown funnels through here — this is
/// the single Prepare implementation (ADR-0016 `source` transform doctrine).
pub(crate) fn prepare_text(text: &str, ctx: &PrepareContext) -> PreparedFile {
    let (fm, body) = frontmatter::parse(text);
    let fm_map = frontmatter::parse_map(text);
    let metadata_json =
        lua_hooks::run_extract(&lua_hooks::hooks(), ctx.rel_path, body, &fm_map).to_string();
    let source_type = match ctx.source_type {
        Some(st) => st.to_string(),
        None => config::source_type_from_dir(ctx.directory),
    };
    let tags_str = if fm.tags.is_empty() {
        None
    } else {
        Some(format!("{:?}", fm.tags))
    };
    let chunk_inputs = chunk_markdown_default(body, ctx.directory, ctx.filename)
        .into_iter()
        .map(|c| {
            let content_hash = chunk_hash(&c.content);
            ChunkInput {
                chunk_index: c.chunk_index,
                section_path: c.section_path,
                content: c.content,
                content_hash,
            }
        })
        .collect();
    PreparedFile {
        source_type,
        title: ctx.filename.to_string(),
        frontmatter: fm,
        tags_str,
        metadata_json,
        chunk_inputs,
        text: text.to_string(),
        policy: ctx.policy,
    }
}

/// Load a Markdown file and run [`prepare_text`] with the default
/// (directory-derived, full-participation) context.
pub(crate) fn prepare(
    file_path: &Path,
    rel_path: &str,
    directory: &str,
    filename: &str,
) -> anyhow::Result<PreparedFile> {
    let text = std::fs::read_to_string(file_path)?;
    Ok(prepare_text(
        &text,
        &PrepareContext {
            rel_path,
            directory,
            filename,
            source_type: None,
            policy: SourcePolicy::markdown(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_text_is_pure_and_mirrors_prepare() {
        let text = "---\nstatus: current\n---\n# H\n\nbody text\n";
        let ctx = PrepareContext {
            rel_path: "daily/notes/x.md",
            directory: "daily/notes",
            filename: "x",
            source_type: None,
            policy: SourcePolicy::markdown(),
        };

        let p = prepare_text(text, &ctx);

        assert_eq!(p.title, "x");
        assert_eq!(p.source_type, "note"); // derived from directory
        assert_eq!(p.frontmatter.status.as_deref(), Some("current"));
        assert!(!p.chunk_inputs.is_empty());
        assert_eq!(p.chunk_inputs[0].content_hash.len(), 64);
        assert!(p.metadata_json.contains("status"));
        assert!(p.text.contains("body text"));
    }

    #[test]
    fn test_prepare_text_source_type_override() {
        // A source transform (e.g. session) must be able to override the
        // directory-derived source_type.
        let ctx = PrepareContext {
            rel_path: "session:abc",
            directory: "session",
            filename: "abc",
            source_type: Some("session"),
            policy: SourcePolicy::session(),
        };

        let p = prepare_text("## Q: hello\n\nQ: hello\n\nA: world\n", &ctx);

        assert_eq!(p.source_type, "session");
    }

    #[test]
    fn test_source_policy_constructors() {
        let md = SourcePolicy::markdown();
        assert!(md.entity_graph && md.doc_links && md.dict_candidates);

        let s = SourcePolicy::session();
        assert!(!s.entity_graph && !s.doc_links && !s.dict_candidates);
    }

    #[test]
    fn test_prepared_file_carries_policy() {
        let ctx = PrepareContext {
            rel_path: "session:abc",
            directory: "session",
            filename: "abc",
            source_type: Some("session"),
            policy: SourcePolicy::session(),
        };

        let p = prepare_text("## Q: hi\n\nQ: hi there\n", &ctx);

        assert!(!p.policy.entity_graph);
        assert!(!p.policy.doc_links);
        assert!(!p.policy.dict_candidates);
    }

    #[test]
    fn test_prepare_parses_and_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily/notes/x.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nstatus: current\n---\n# H\n\nbody text\n").unwrap();

        let p = prepare(&path, "daily/notes/x.md", "daily/notes", "x").unwrap();

        assert_eq!(p.title, "x");
        assert_eq!(p.frontmatter.status.as_deref(), Some("current"));
        assert!(!p.chunk_inputs.is_empty());
        assert_eq!(p.chunk_inputs[0].content_hash.len(), 64); // sha-256 hex
        assert!(p.metadata_json.contains("status"));
        assert!(p.text.contains("body text"));
    }
}
