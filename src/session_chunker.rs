use std::io::BufRead;
use std::path::Path;

use crate::config::MAX_CHUNK_CHARS;

const MIN_MESSAGE_LEN: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionChunk {
    pub content: String,
    pub chunk_index: usize,
    pub timestamp: Option<String>,
}

/// Parse a Claude session JSONL file into Q&A chunks.
pub fn parse_session_jsonl(path: &Path) -> anyhow::Result<Vec<SessionChunk>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    // (role, text, timestamp)
    let mut messages: Vec<(String, String, Option<String>)> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        let json: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let message = &json["message"];
        let role = match message["role"].as_str() {
            Some(r) if r == "user" || r == "assistant" => r.to_string(),
            _ => continue,
        };

        let text = match extract_text_content(message) {
            Some(t) if t.chars().count() >= MIN_MESSAGE_LEN => t,
            _ => continue,
        };

        let timestamp = json["timestamp"].as_str().map(|s| s.to_string());

        messages.push((role, text, timestamp));
    }

    let mut chunks = Vec::new();
    let mut i = 0;

    while i < messages.len() {
        let (role, text, ts) = &messages[i];

        if role == "user" {
            let q_text = truncate_text(text, MAX_CHUNK_CHARS);
            let q_ts = ts.clone();
            if i + 1 < messages.len() && messages[i + 1].0 == "assistant" {
                let a_text = truncate_text(&messages[i + 1].1, MAX_CHUNK_CHARS);

                // Blank line between Q and A: they must be separate paragraphs
                // so the markdown chunker can split an oversized pair at the
                // Q/A boundary (mirrors the split below).
                let pair = format!("Q: {q_text}\n\nA: {a_text}");
                if pair.chars().count() <= MAX_CHUNK_CHARS * 2 {
                    chunks.push(SessionChunk {
                        content: pair,
                        chunk_index: chunks.len(),
                        timestamp: q_ts,
                    });
                } else {
                    chunks.push(SessionChunk {
                        content: format!("Q: {q_text}"),
                        chunk_index: chunks.len(),
                        timestamp: q_ts,
                    });
                    chunks.push(SessionChunk {
                        content: format!("A: {a_text}"),
                        chunk_index: chunks.len(),
                        timestamp: messages[i + 1].2.clone(),
                    });
                }
                i += 2;
                continue;
            }
            // Orphan user message
            chunks.push(SessionChunk {
                content: format!("Q: {q_text}"),
                chunk_index: chunks.len(),
                timestamp: q_ts,
            });
        } else {
            // Orphan assistant message
            let a_text = truncate_text(text, MAX_CHUNK_CHARS);
            chunks.push(SessionChunk {
                content: a_text,
                chunk_index: chunks.len(),
                timestamp: ts.clone(),
            });
        }
        i += 1;
    }

    Ok(chunks)
}

/// Max chars of a chunk's first line used as its H2 heading text.
/// Kept well under the chunker's own 120-byte heading cap.
const HEADING_SNIPPET_CHARS: usize = 40;

/// Serialize session chunks into Markdown for the Prepare stage (#366).
///
/// One session chunk = one H2 section, so `chunk_markdown_default`
/// reproduces the Q&A-level chunk boundaries. Document-level created /
/// updated (first / last conversation timestamp, falling back to
/// `fallback_ts`) and `status: current` ride in the frontmatter, which
/// `persist()` writes to the documents row.
pub fn session_to_markdown(chunks: &[SessionChunk], fallback_ts: &str) -> String {
    let created = chunks
        .iter()
        .filter_map(|c| c.timestamp.as_deref())
        .next()
        .unwrap_or(fallback_ts);
    let updated = chunks
        .iter()
        .filter_map(|c| c.timestamp.as_deref())
        .next_back()
        .unwrap_or(fallback_ts);

    let mut out = format!(
        "---\nstatus: current\ncreated: \"{}\"\nupdated: \"{}\"\n---\n\n",
        sanitize_yaml_value(created),
        sanitize_yaml_value(updated),
    );
    for chunk in chunks {
        out.push_str("## ");
        out.push_str(&heading_snippet(&chunk.content));
        out.push_str("\n\n");
        out.push_str(&escape_heading_lines(&chunk.content));
        out.push_str("\n\n");
    }
    out
}

/// Strip characters that would break out of a double-quoted YAML scalar.
fn sanitize_yaml_value(value: &str) -> String {
    value.replace(['"', '\\', '\n'], "")
}

/// First line of the chunk content, truncated, as H2 heading text.
fn heading_snippet(content: &str) -> String {
    let first_line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    truncate_text(first_line.trim(), HEADING_SNIPPET_CHARS)
}

/// Backslash-escape lines that the markdown chunker would read as H1–H3
/// headings, so message-internal headings (or `#` comment lines in pasted
/// code) cannot create extra section boundaries. H4+ is not a boundary.
fn escape_heading_lines(content: &str) -> String {
    let escaped: Vec<String> = content
        .lines()
        .map(|line| {
            let hashes = line.bytes().take_while(|b| *b == b'#').count();
            let is_heading = (1..=3).contains(&hashes)
                && matches!(line.as_bytes().get(hashes), Some(b' ') | Some(b'\t'));
            if is_heading {
                format!("\\{line}")
            } else {
                line.to_string()
            }
        })
        .collect();
    escaped.join("\n")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let s: String = text.chars().take(max_chars).collect();
        format!("{s}...")
    }
}

fn extract_text_content(message: &serde_json::Value) -> Option<String> {
    let content = &message["content"];

    // String content
    if let Some(s) = content.as_str() {
        let s = s.trim();
        return if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }

    // Array content
    if let Some(arr) = content.as_array() {
        // List of dicts with type=="text"
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                if item["type"].as_str() == Some("text") {
                    item["text"].as_str().map(|s| s.to_string())
                } else {
                    item.as_str().map(|s| s.to_string())
                }
            })
            .collect();

        if texts.is_empty() {
            return None;
        }
        let joined = texts.join("\n").trim().to_string();
        return if joined.is_empty() {
            None
        } else {
            Some(joined)
        };
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_normal_qa_pair() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"これはテストの質問です。長いテキスト。"}}"#,
            r#"{"message":{"role":"assistant","content":"これはテストの回答です。長いテキスト。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("Q: "));
        assert!(chunks[0].content.contains("\nA: "));
    }

    #[test]
    fn test_content_list_of_dicts() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":[{"type":"text","text":"テスト質問のテキストです。"}]}}"#,
            r#"{"message":{"role":"assistant","content":"テスト回答のテキストです。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("テスト質問"));
    }

    #[test]
    fn test_content_list_of_strings() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":["テスト文字列のリストです。"]}}"#,
            r#"{"message":{"role":"assistant","content":"回答テキストの内容です。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_short_message_filtered() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"短い"}}"#,
            r#"{"message":{"role":"assistant","content":"短い回答"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_assistant_truncation() {
        let long_text = "あ".repeat(1000);
        let line = format!(r#"{{"message":{{"role":"assistant","content":"{long_text}"}}}}"#);
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"テスト質問のテキストです。"}}"#,
            &line,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("..."));
    }

    #[test]
    fn test_large_pair_split() {
        let long_q = "質".repeat(900);
        let long_a = "答".repeat(900);
        let q_line = format!(r#"{{"message":{{"role":"user","content":"{long_q}"}}}}"#);
        let a_line = format!(r#"{{"message":{{"role":"assistant","content":"{long_a}"}}}}"#);
        let f = write_jsonl(&[&q_line, &a_line]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert!(chunks.len() >= 2);
        assert!(chunks[0].content.starts_with("Q: "));
        assert!(chunks[1].content.starts_with("A: "));
    }

    #[test]
    fn test_invalid_json_skipped() {
        let f = write_jsonl(&[
            "not valid json",
            r#"{"message":{"role":"user","content":"有効なメッセージテキスト。"}}"#,
            r#"{"message":{"role":"assistant","content":"有効な回答テキストです。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_timestamp_extracted() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"これはテストの質問です。長いテキスト。"},"timestamp":"2026-03-30T08:00:00.000Z"}"#,
            r#"{"message":{"role":"assistant","content":"これはテストの回答です。長いテキスト。"},"timestamp":"2026-03-30T08:01:00.000Z"}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        // Q&A pair uses Q's timestamp
        assert_eq!(
            chunks[0].timestamp.as_deref(),
            Some("2026-03-30T08:00:00.000Z")
        );
    }

    #[test]
    fn test_timestamp_none_when_missing() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"タイムスタンプなしの質問メッセージ。"}}"#,
            r#"{"message":{"role":"assistant","content":"タイムスタンプなしの回答メッセージ。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].timestamp.is_none());
    }

    #[test]
    fn test_session_to_markdown_frontmatter_timestamps() {
        let chunks = vec![
            SessionChunk {
                content: "Q: 最初の質問です。\n\nA: 最初の回答です。".to_string(),
                chunk_index: 0,
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            },
            SessionChunk {
                content: "Q: 次の質問です。\n\nA: 次の回答です。".to_string(),
                chunk_index: 1,
                timestamp: Some("2026-02-02T00:00:00Z".to_string()),
            },
        ];

        let md = session_to_markdown(&chunks, "2026-03-03T00:00:00Z");

        let (fm, _) = crate::frontmatter::parse(&md);
        assert_eq!(fm.status.as_deref(), Some("current"));
        assert_eq!(fm.created.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(fm.updated.as_deref(), Some("2026-02-02T00:00:00Z"));
    }

    #[test]
    fn test_session_to_markdown_fallback_timestamp() {
        let chunks = vec![SessionChunk {
            content: "Q: タイムスタンプなしの質問。".to_string(),
            chunk_index: 0,
            timestamp: None,
        }];

        let md = session_to_markdown(&chunks, "2026-03-03T00:00:00Z");

        let (fm, _) = crate::frontmatter::parse(&md);
        assert_eq!(fm.created.as_deref(), Some("2026-03-03T00:00:00Z"));
        assert_eq!(fm.updated.as_deref(), Some("2026-03-03T00:00:00Z"));
    }

    #[test]
    fn test_session_to_markdown_one_h2_per_chunk() {
        let chunks = vec![
            SessionChunk {
                content: "Q: 一つ目の質問文。\n\nA: 一つ目の回答文。".to_string(),
                chunk_index: 0,
                timestamp: None,
            },
            SessionChunk {
                content: "Q: 二つ目の質問文。\n\nA: 二つ目の回答文。".to_string(),
                chunk_index: 1,
                timestamp: None,
            },
        ];

        let md = session_to_markdown(&chunks, "2026-01-01T00:00:00Z");

        let h2_count = md.lines().filter(|l| l.starts_with("## ")).count();
        assert_eq!(h2_count, 2, "one H2 heading per session chunk");
        assert!(md.contains("## Q: 一つ目の質問文。"));
    }

    #[test]
    fn test_session_to_markdown_escapes_embedded_heading_lines() {
        // Message text containing Markdown headings (assistant answers often
        // do) must not create extra section boundaries in the chunker.
        let chunks = vec![SessionChunk {
            content: "Q: 見出しを含む質問です。\n\nA: 回答です。\n\n## 埋め込み見出し\n\n### サブ見出し\n\n#### H4はそのまま\n\n本文です。"
                .to_string(),
            chunk_index: 0,
            timestamp: None,
        }];

        let md = session_to_markdown(&chunks, "2026-01-01T00:00:00Z");

        assert!(md.contains("\\## 埋め込み見出し"));
        assert!(md.contains("\\### サブ見出し"));
        assert!(
            md.contains("\n#### H4はそのまま"),
            "H4+ is not a chunk boundary and stays unescaped"
        );
    }

    /// The core equivalence guarantee (#366): serialized Markdown, run through
    /// the standard chunker, reproduces the current Q&A chunk granularity —
    /// one chunk per session chunk, heading-based section_path, even when
    /// message text contains heading-looking lines.
    #[test]
    fn test_markdown_roundtrip_preserves_qa_boundaries() {
        let chunks = vec![
            SessionChunk {
                content: "Q: 最初の質問です。\n\nA: 最初の回答です。".to_string(),
                chunk_index: 0,
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            },
            SessionChunk {
                content: "Q: 見出し入りの質問。\n\nA: 回答本文。\n\n## 埋め込み見出し\n\n本文が続きます。"
                    .to_string(),
                chunk_index: 1,
                timestamp: Some("2026-01-02T00:00:00Z".to_string()),
            },
            SessionChunk {
                content: "孤立したアシスタントメッセージです。".to_string(),
                chunk_index: 2,
                timestamp: None,
            },
        ];

        let md = session_to_markdown(&chunks, "2026-01-03T00:00:00Z");
        let (_, body) = crate::frontmatter::parse(&md);
        let out = crate::chunker::chunk_markdown_default(body, "session", "abc123");

        assert_eq!(out.len(), chunks.len(), "one chunk per Q&A unit");
        assert!(out[0].content.contains("最初の質問です。"));
        assert!(out[1].content.contains("埋め込み見出し"));
        assert!(out[2].content.contains("孤立したアシスタント"));
        // section_path is heading-based (was the constant "session" before)
        assert!(out[1].section_path.contains("見出し入りの質問"));
    }

    /// An oversized pair splits at the Q/A paragraph boundary — the same
    /// shape as the current session chunker's Q/A split for large pairs.
    #[test]
    fn test_markdown_roundtrip_oversized_pair_splits_at_qa_boundary() {
        let q = "q".repeat(600);
        let a = "a".repeat(600);
        let chunks = vec![SessionChunk {
            content: format!("Q: {q}\n\nA: {a}"),
            chunk_index: 0,
            timestamp: None,
        }];

        let md = session_to_markdown(&chunks, "2026-01-01T00:00:00Z");
        let (_, body) = crate::frontmatter::parse(&md);
        let out = crate::chunker::chunk_markdown_default(body, "session", "abc123");

        assert_eq!(out.len(), 2, "oversized pair splits into Q and A chunks");
        assert!(out[0].content.contains("Q: qqq"));
        assert!(out[1].content.contains("A: aaa"));
    }

    #[test]
    fn test_qa_pair_joined_with_blank_line() {
        // Q and A must be separate paragraphs so the markdown chunker can
        // split oversized pairs at the Q/A boundary.
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"これはテストの質問です。長いテキスト。"}}"#,
            r#"{"message":{"role":"assistant","content":"これはテストの回答です。長いテキスト。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("\n\nA: "));
    }

    #[test]
    fn test_empty_file() {
        let f = write_jsonl(&[]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_assistant_first() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"assistant","content":"先に来たアシスタントのメッセージ。"}}"#,
            r#"{"message":{"role":"user","content":"後から来たユーザーのメッセージ。"}}"#,
            r#"{"message":{"role":"assistant","content":"ペアになるアシスタントのメッセージ。"}}"#,
        ]);
        let chunks = parse_session_jsonl(f.path()).unwrap();
        // First assistant is orphan, then user+assistant pair
        assert!(chunks.len() >= 2);
    }
}
