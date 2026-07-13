//! Prepare stage: Markdown text -> PreparedFile (`prepare_text`, pure) with
//! a file-loading wrapper (`prepare`). No DB access.

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

/// Per-source pipeline participation policy, named by capability — this
/// generic layer knows nothing about concrete sources. Each source's index
/// flow picks the policy that matches what its documents should join.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourcePolicy {
    pub entity_graph: bool,
    pub doc_links: bool,
    pub dict_candidates: bool,
}

impl SourcePolicy {
    /// Participate in every side index (entity graph, doc links,
    /// per-chunk dictionary candidates). Filesystem Markdown uses this.
    pub(crate) fn full() -> Self {
        Self {
            entity_graph: true,
            doc_links: true,
            dict_candidates: true,
        }
    }

    /// Searchable text only — no side-index participation. For sources
    /// whose documents shouldn't shape the entity/link graphs or the
    /// dictionary (e.g. ingested conversation transcripts).
    pub(crate) fn text_only() -> Self {
        Self {
            entity_graph: false,
            doc_links: false,
            dict_candidates: false,
        }
    }
}

/// Identity and policy for one Prepare invocation. The caller states all of
/// it explicitly; this layer derives nothing:
///
/// - `uri` — document identity: the project-relative path for filesystem
///   sources, or a `<scheme>:<id>` key for external sources (e.g.
///   `session:<stem>`). Passed to extract hooks as `ctx.path`.
/// - `source_type` — classification stored on the documents row. Filesystem
///   callers derive it from the directory; external sources name their own.
pub(crate) struct PrepareContext<'a> {
    pub uri: &'a str,
    pub directory: &'a str,
    pub filename: &'a str,
    pub source_type: &'a str,
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
///
/// The first step normalizes the whole input, plus `ctx.directory` and
/// `ctx.filename`, to Unicode NFC — the latter two come straight from the
/// filesystem walker (which does not itself normalize) and are embedded
/// verbatim into every chunk's section-path prefix and the document title, so
/// leaving them raw would let a non-NFC path silently reintroduce the mismatch
/// this module exists to close. Once normalized, every downstream stage —
/// frontmatter parse, chunking, lua extract, content hashing — sees a single
/// canonical representation regardless of how the source was encoded.
pub(crate) fn prepare_text(text: &str, ctx: &PrepareContext) -> PreparedFile {
    let text = crate::normalize::nfc(text);
    let text = text.as_ref();
    let directory = crate::normalize::nfc(ctx.directory);
    let filename = crate::normalize::nfc(ctx.filename);
    let (fm, body) = frontmatter::parse(text);
    let fm_map = frontmatter::parse_map(text);
    let metadata_json =
        lua_hooks::run_extract(&lua_hooks::hooks(), ctx.uri, body, &fm_map).to_string();
    let tags_str = if fm.tags.is_empty() {
        None
    } else {
        Some(format!("{:?}", fm.tags))
    };
    let chunk_inputs = chunk_markdown_default(body, &directory, &filename)
        .into_iter()
        .map(|c| {
            debug_assert!(
                crate::normalize::is_normalized(&c.content),
                "chunk content must be NFC-normalized (see src/normalize.rs)"
            );
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
        source_type: ctx.source_type.to_string(),
        title: filename.to_string(),
        frontmatter: fm,
        tags_str,
        metadata_json,
        chunk_inputs,
        text: text.to_string(),
        policy: ctx.policy,
    }
}

/// Load a Markdown file and run [`prepare_text`] with the filesystem
/// context: directory-derived source_type, full side-index participation.
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
            uri: rel_path,
            directory,
            filename,
            source_type: &config::source_type_from_dir(directory),
            policy: SourcePolicy::full(),
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
            uri: "daily/notes/x.md",
            directory: "daily/notes",
            filename: "x",
            source_type: "note",
            policy: SourcePolicy::full(),
        };

        let p = prepare_text(text, &ctx);

        assert_eq!(p.title, "x");
        assert_eq!(p.source_type, "note");
        assert_eq!(p.frontmatter.status.as_deref(), Some("current"));
        assert!(!p.chunk_inputs.is_empty());
        assert_eq!(p.chunk_inputs[0].content_hash.len(), 64);
        assert!(p.metadata_json.contains("status"));
        assert!(p.text.contains("body text"));
    }

    /// External sources identify themselves with a `<scheme>:<id>` uri and
    /// an explicit source_type — this layer stores them verbatim, deriving
    /// nothing from the content directories.
    #[test]
    fn test_prepare_text_external_source_identity() {
        let ctx = PrepareContext {
            uri: "session:abc",
            directory: "session",
            filename: "abc",
            source_type: "session",
            policy: SourcePolicy::text_only(),
        };

        let p = prepare_text("## Q: hello\n\nQ: hello\n\nA: world\n", &ctx);

        assert_eq!(p.source_type, "session");
    }

    #[test]
    fn test_source_policy_constructors() {
        let full = SourcePolicy::full();
        assert!(full.entity_graph && full.doc_links && full.dict_candidates);

        let text_only = SourcePolicy::text_only();
        assert!(!text_only.entity_graph && !text_only.doc_links && !text_only.dict_candidates);
    }

    #[test]
    fn test_prepared_file_carries_policy() {
        let ctx = PrepareContext {
            uri: "session:abc",
            directory: "session",
            filename: "abc",
            source_type: "session",
            policy: SourcePolicy::text_only(),
        };

        let p = prepare_text("## Q: hi\n\nQ: hi there\n", &ctx);

        assert!(!p.policy.entity_graph);
        assert!(!p.policy.doc_links);
        assert!(!p.policy.dict_candidates);
    }

    /// The filesystem wrapper derives source_type from the directory.
    #[test]
    fn test_prepare_wrapper_derives_source_type_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily/notes/x.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# H\n\nbody\n").unwrap();

        let p = prepare(&path, "daily/notes/x.md", "daily/notes", "x").unwrap();

        assert_eq!(p.source_type, "note");
        assert!(p.policy.entity_graph && p.policy.doc_links && p.policy.dict_candidates);
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

    /// A Markdown file containing NFD text (decomposed dakuten) must produce
    /// the same chunk content and content_hash as the equivalent NFC source —
    /// `prepare_text` normalizes before chunking/hashing.
    #[test]
    fn test_prepare_text_normalizes_nfd_to_match_nfc_source() {
        let nfd_ga = "\u{30ab}\u{3099}"; // ガ (decomposed: カ + combining dakuten)
        let nfc_ga = "\u{30ac}"; // ガ (precomposed)
        assert_ne!(nfd_ga, nfc_ga);

        let ctx = PrepareContext {
            uri: "daily/notes/x.md",
            directory: "daily/notes",
            filename: "x",
            source_type: "note",
            policy: SourcePolicy::full(),
        };

        let nfd_text = format!("# H\n\n{nfd_ga}ワーカー\n");
        let nfc_text = format!("# H\n\n{nfc_ga}ワーカー\n");

        let p_nfd = prepare_text(&nfd_text, &ctx);
        let p_nfc = prepare_text(&nfc_text, &ctx);

        assert_eq!(p_nfd.chunk_inputs[0].content, p_nfc.chunk_inputs[0].content);
        assert_eq!(
            p_nfd.chunk_inputs[0].content_hash,
            p_nfc.chunk_inputs[0].content_hash
        );
        assert!(p_nfd.chunk_inputs[0].content.contains(nfc_ga));
    }

    /// `ctx.directory` and `ctx.filename` come straight from the filesystem
    /// walker, which does not itself normalize — an NFD path (as returned by
    /// some filesystems for decomposed Japanese names) must still converge to
    /// NFC, since both are embedded verbatim into every chunk's
    /// `【directory/filename】` prefix and used as the H1-fallback title.
    #[test]
    fn test_prepare_text_normalizes_nfd_directory_and_filename() {
        let nfd_ga = "\u{30ab}\u{3099}"; // ガ (decomposed: カ + combining dakuten)
        let nfc_ga = "\u{30ac}"; // ガ (precomposed)
        let nfd_directory = format!("daily/{nfd_ga}note");
        let nfd_filename = format!("{nfd_ga}memo");
        let ctx = PrepareContext {
            uri: "daily/x/x.md",
            directory: &nfd_directory,
            filename: &nfd_filename,
            source_type: "note",
            policy: SourcePolicy::full(),
        };

        let p = prepare_text("body text with no heading\n", &ctx);

        assert!(!p.chunk_inputs.is_empty());
        for chunk in &p.chunk_inputs {
            assert!(
                crate::normalize::is_normalized(&chunk.content),
                "chunk content (including the 【dir/file】 prefix) must be NFC: {:?}",
                chunk.content
            );
        }
        assert!(crate::normalize::is_normalized(&p.title));
        assert_eq!(p.title, format!("{nfc_ga}memo"));
    }
}
