use regex::Regex;
use std::sync::LazyLock;

use crate::config::MAX_CHUNK_CHARS;

static H1_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#\s+(.+)").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub content: String,
    pub section_path: String,
    pub chunk_index: usize,
}

/// Split a Markdown body into chunks with context prefixes.
pub fn chunk_markdown(body: &str, directory: &str, filename: &str, max_chars: usize) -> Vec<Chunk> {
    chunk_markdown_inner(body, directory, filename, max_chars)
}

/// Convenience wrapper using default max_chars.
pub fn chunk_markdown_default(body: &str, directory: &str, filename: &str) -> Vec<Chunk> {
    chunk_markdown(body, directory, filename, MAX_CHUNK_CHARS)
}

fn chunk_markdown_inner(
    body: &str,
    directory: &str,
    filename: &str,
    max_chars: usize,
) -> Vec<Chunk> {
    if body.trim().is_empty() {
        return Vec::new();
    }

    let title = extract_title(body).unwrap_or(filename);
    let h2_sections = split_by_header(body, 2);

    let mut chunks = Vec::new();
    let mut index = 0usize;

    for (h2_heading, h2_text) in &h2_sections {
        let section_name = if h2_heading.is_empty() {
            title.to_string()
        } else {
            format!("{title} > {h2_heading}")
        };

        if h2_text.trim().is_empty() {
            continue;
        }

        if h2_text.len() <= max_chars {
            let prefix = format!("【{directory}/{filename}】{section_name}\n");
            chunks.push(Chunk {
                content: format!("{prefix}{}", h2_text.trim()),
                section_path: section_name,
                chunk_index: index,
            });
            index += 1;
            continue;
        }

        // Split by H3
        let h3_sections = split_by_header(h2_text, 3);
        for (h3_heading, h3_text) in &h3_sections {
            let sub_section_name = if h3_heading.is_empty() {
                section_name.clone()
            } else {
                format!("{section_name} > {h3_heading}")
            };

            if h3_text.trim().is_empty() {
                continue;
            }

            if h3_text.len() <= max_chars {
                let prefix = format!("【{directory}/{filename}】{sub_section_name}\n");
                chunks.push(Chunk {
                    content: format!("{prefix}{}", h3_text.trim()),
                    section_path: sub_section_name,
                    chunk_index: index,
                });
                index += 1;
                continue;
            }

            // Split by paragraphs and merge small ones
            let paragraphs: Vec<&str> = split_paragraphs(h3_text)
                .into_iter()
                .flat_map(|p| hard_split(p, max_chars))
                .collect();
            let merged = merge_small_chunks(&paragraphs, max_chars);
            for part in merged {
                let prefix = format!("【{directory}/{filename}】{sub_section_name}\n");
                chunks.push(Chunk {
                    content: format!("{prefix}{}", part.trim()),
                    section_path: sub_section_name.clone(),
                    chunk_index: index,
                });
                index += 1;
            }
        }
    }

    chunks
}

fn extract_title(body: &str) -> Option<&str> {
    let first_line = body.lines().find(|l| !l.trim().is_empty())?;
    let caps = H1_RE.captures(first_line)?;
    Some(caps.get(1)?.as_str().trim())
}

fn split_by_header(body: &str, level: usize) -> Vec<(String, String)> {
    let prefix = "#".repeat(level);
    let pattern = format!(r"(?m)^{prefix}\s+(.+)");
    let re = Regex::new(&pattern).unwrap();

    let mut sections = Vec::new();
    let mut last_end = 0;
    let mut last_heading = String::new();

    for caps in re.captures_iter(body) {
        let m = caps.get(0).unwrap();
        let heading = caps.get(1).unwrap().as_str().trim().to_string();

        // Text before this header
        let text_before = &body[last_end..m.start()];
        if last_end == 0 {
            // Pre-header text
            if !text_before.trim().is_empty() {
                sections.push((String::new(), text_before.to_string()));
            }
        } else {
            sections.push((last_heading.clone(), text_before.to_string()));
        }

        last_heading = heading;
        last_end = m.end();
    }

    // Remaining text after last header
    let remaining = &body[last_end..];
    if last_end == 0 {
        // No headers found at all
        sections.push((String::new(), body.to_string()));
    } else {
        sections.push((last_heading, remaining.to_string()));
    }

    sections
}

fn split_paragraphs(text: &str) -> Vec<&str> {
    let re = Regex::new(r"\n\s*\n").unwrap();
    re.split(text)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split a text into pieces of at most `max_bytes` bytes, cutting only at
/// UTF-8 char boundaries. Needed because a single paragraph (e.g. a pasted
/// log or minified line) can exceed max_chars and would otherwise become
/// one unbounded chunk.
fn hard_split(text: &str, max_bytes: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = text;
    while rest.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            break; // max_bytes smaller than one char; emit remainder as-is
        }
        parts.push(&rest[..end]);
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        parts.push(rest);
    }
    parts
}

fn merge_small_chunks(paragraphs: &[&str], max_chars: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();

    for &para in paragraphs {
        if current.is_empty() {
            current = para.to_string();
        } else if current.len() + para.len() + 2 > max_chars {
            result.push(current);
            current = para.to_string();
        } else {
            current.push_str("\n\n");
            current.push_str(para);
        }
    }

    if !current.is_empty() {
        result.push(current);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_section() {
        let body = "# タイトル\n\nこれはテスト本文です。";
        let chunks = chunk_markdown(body, "daily/notes", "test", 800);
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].chunk_index, 0);
        assert!(chunks[0].section_path.contains("タイトル"));
    }

    #[test]
    fn test_h2_split() {
        let body = "# Title\n\n## Section A\n\nText A.\n\n## Section B\n\nText B.\n";
        let chunks = chunk_markdown(body, "daily/notes", "test", 800);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn test_prefix_format() {
        let body = "# Title\n\nSome content here.\n";
        let chunks = chunk_markdown(body, "company/knowledge", "sample", 800);
        assert!(!chunks.is_empty());
        assert!(chunks[0]
            .content
            .starts_with("【company/knowledge/sample】"));
    }

    #[test]
    fn test_empty_body() {
        let chunks = chunk_markdown("", "daily/notes", "test", 800);
        assert!(chunks.is_empty());

        let chunks = chunk_markdown("   \n  \n  ", "daily/notes", "test", 800);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_h3_subsections() {
        let body = "# Title\n\n## Section\n\n### Sub A\n\nText A.\n\n### Sub B\n\nText B.\n";
        let chunks = chunk_markdown(body, "daily/notes", "test", 30);
        assert!(chunks.len() >= 2);
        let has_sub = chunks.iter().any(|c| c.section_path.contains("Sub"));
        assert!(has_sub);
    }

    #[test]
    fn test_chunk_index_sequential() {
        let body = "# Title\n\n## A\n\nText A.\n\n## B\n\nText B.\n\n## C\n\nText C.\n";
        let chunks = chunk_markdown(body, "daily/notes", "test", 800);
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.chunk_index, i);
        }
    }

    #[test]
    fn test_large_section_splits() {
        let big_text = "あ".repeat(500);
        let body = format!("# Title\n\n## Section\n\n{big_text}\n\n{big_text}\n\n{big_text}\n");
        let chunks = chunk_markdown(&body, "daily/notes", "test", 800);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn test_no_h1_uses_filename() {
        let body = "Some text without heading.\n";
        let chunks = chunk_markdown(body, "daily/notes", "myfile", 800);
        assert!(!chunks.is_empty());
        assert!(chunks[0].section_path.contains("myfile"));
    }

    #[test]
    fn test_oversized_single_paragraph_is_split() {
        // One giant paragraph (no blank lines) far above max_chars
        let big_para = "word ".repeat(1000); // 5000 bytes, single paragraph
        let body = format!("# Title\n\n## Section\n\n{big_para}\n");
        let chunks = chunk_markdown(&body, "daily/notes", "test", 800);
        assert!(
            chunks.len() >= 5,
            "expected >=5 chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            // prefix is ~40 bytes; body part must be <= 800
            assert!(
                c.content.len() <= 900,
                "chunk too large: {} bytes",
                c.content.len()
            );
        }
    }

    #[test]
    fn test_oversized_paragraph_split_respects_char_boundary() {
        // Multibyte text: 3-byte chars; naive byte cut would panic on slice
        let big_para = "あ".repeat(1000); // 3000 bytes, no whitespace at all
        let body = format!("# Title\n\n## Section\n\n{big_para}\n");
        let chunks = chunk_markdown(&body, "daily/notes", "test", 800);
        assert!(chunks.len() >= 3);
        let joined: String = chunks
            .iter()
            .map(|c| c.content.split_once('\n').unwrap().1)
            .collect();
        assert_eq!(joined.chars().filter(|c| *c == 'あ').count(), 1000);
    }

    #[test]
    fn test_hard_split_boundaries() {
        assert_eq!(hard_split("abcdef", 6), vec!["abcdef"]); // exact fit
        assert_eq!(hard_split("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(hard_split("", 3), Vec::<&str>::new());
        // 3-byte chars, limit 4 → one char per part (cut aligned down to 3)
        assert_eq!(hard_split("ああ", 4), vec!["あ", "あ"]);
        // max_bytes smaller than one char → remainder emitted as-is (end == 0 branch)
        assert_eq!(hard_split("あ", 1), vec!["あ"]);
    }
}
