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

pub(crate) struct PreparedFile {
    pub source_type: String,
    pub title: String,
    pub frontmatter: frontmatter::Frontmatter,
    pub tags_str: Option<String>,
    pub metadata_json: String,
    pub chunk_inputs: Vec<ChunkInput>,
    pub text: String,
}

pub(crate) fn prepare(
    file_path: &Path,
    rel_path: &str,
    directory: &str,
    filename: &str,
) -> anyhow::Result<PreparedFile> {
    let text = std::fs::read_to_string(file_path)?;
    let (fm, body) = frontmatter::parse(&text);
    let fm_map = frontmatter::parse_map(&text);
    let metadata_json =
        lua_hooks::run_extract(&lua_hooks::hooks(), rel_path, body, &fm_map).to_string();
    let source_type = config::source_type_from_dir(directory);
    let tags_str = if fm.tags.is_empty() {
        None
    } else {
        Some(format!("{:?}", fm.tags))
    };
    let chunk_inputs = chunk_markdown_default(body, directory, filename)
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
    Ok(PreparedFile {
        source_type,
        title: filename.to_string(),
        frontmatter: fm,
        tags_str,
        metadata_json,
        chunk_inputs,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
