//! Session JSONL → Markdown source transform (ADR-0016 `source` doctrine).
//!
//! This module makes **content** decisions only: which messages are worth
//! indexing, how they pair into Q&A units, the `Q:` / `A:` labels, and the
//! per-message length cap. Chunk boundaries, sizes, and indexes are owned
//! entirely by the markdown chunker downstream — the serialized document is
//! chunked like any other Markdown source.

use std::io::BufRead;
use std::path::Path;

use crate::config::MAX_CHUNK_CHARS;

const MIN_MESSAGE_LEN: usize = 10;

/// One conversation unit: a user question with its assistant answer, or an
/// orphan message (question-only / answer-only).
#[derive(Debug, Clone, PartialEq)]
pub struct QaUnit {
    pub question: Option<String>,
    pub answer: Option<String>,
    /// Timestamp of the unit's first message.
    pub timestamp: Option<String>,
}

/// Parse a Claude session JSONL file into Q&A units.
///
/// Each message is capped at [`MAX_CHUNK_CHARS`] chars — a content decision
/// ("how much of a pasted log is worth indexing"), not a chunk-size one;
/// oversized units are split downstream by the markdown chunker.
pub fn parse_session_jsonl(path: &Path) -> anyhow::Result<Vec<QaUnit>> {
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

    let mut units = Vec::new();
    let mut i = 0;

    while i < messages.len() {
        let (role, text, ts) = &messages[i];

        if role == "user" {
            let question = Some(truncate_text(text, MAX_CHUNK_CHARS));
            if i + 1 < messages.len() && messages[i + 1].0 == "assistant" {
                units.push(QaUnit {
                    question,
                    answer: Some(truncate_text(&messages[i + 1].1, MAX_CHUNK_CHARS)),
                    timestamp: ts.clone(),
                });
                i += 2;
                continue;
            }
            // Orphan user message
            units.push(QaUnit {
                question,
                answer: None,
                timestamp: ts.clone(),
            });
        } else {
            // Orphan assistant message
            units.push(QaUnit {
                question: None,
                answer: Some(truncate_text(text, MAX_CHUNK_CHARS)),
                timestamp: ts.clone(),
            });
        }
        i += 1;
    }

    Ok(units)
}

/// Max chars of a unit's first line used as its H2 heading text.
/// Kept well under the chunker's own 120-byte heading cap.
const HEADING_SNIPPET_CHARS: usize = 40;

/// Serialize Q&A units into Markdown for the Prepare stage (#366).
///
/// One unit = one H2 section; Q and A are separate paragraphs, so the
/// markdown chunker keeps a normal-sized unit as one chunk and splits an
/// oversized one at the Q/A boundary. Document-level created / updated
/// (first / last unit timestamp, falling back to `fallback_ts`) and
/// `status: current` ride in the frontmatter, which `persist()` writes.
pub fn session_to_markdown(units: &[QaUnit], fallback_ts: &str) -> String {
    let created = units
        .iter()
        .filter_map(|u| u.timestamp.as_deref())
        .next()
        .unwrap_or(fallback_ts);
    let updated = units
        .iter()
        .filter_map(|u| u.timestamp.as_deref())
        .next_back()
        .unwrap_or(fallback_ts);

    let mut out = format!(
        "---\nstatus: current\ncreated: \"{}\"\nupdated: \"{}\"\n---\n\n",
        sanitize_yaml_value(created),
        sanitize_yaml_value(updated),
    );
    for unit in units {
        let body = unit_body(unit);
        out.push_str("## ");
        out.push_str(&heading_snippet(&body));
        out.push_str("\n\n");
        out.push_str(&escape_heading_lines(&body));
        out.push_str("\n\n");
    }
    out
}

/// Section body text for one unit. Orphan assistant messages stay unlabeled
/// (they answer nothing in this transcript).
fn unit_body(unit: &QaUnit) -> String {
    match (&unit.question, &unit.answer) {
        (Some(q), Some(a)) => format!("Q: {q}\n\nA: {a}"),
        (Some(q), None) => format!("Q: {q}"),
        (None, Some(a)) => a.clone(),
        (None, None) => String::new(),
    }
}

/// Strip characters that would break out of a double-quoted YAML scalar.
fn sanitize_yaml_value(value: &str) -> String {
    value.replace(['"', '\\', '\n'], "")
}

/// First line of the unit body, truncated, as H2 heading text.
fn heading_snippet(body: &str) -> String {
    let first_line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
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
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].question.as_deref().unwrap().contains("質問"));
        assert!(units[0].answer.as_deref().unwrap().contains("回答"));
    }

    #[test]
    fn test_content_list_of_dicts() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":[{"type":"text","text":"テスト質問のテキストです。"}]}}"#,
            r#"{"message":{"role":"assistant","content":"テスト回答のテキストです。"}}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].question.as_deref().unwrap().contains("テスト質問"));
    }

    #[test]
    fn test_content_list_of_strings() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":["テスト文字列のリストです。"]}}"#,
            r#"{"message":{"role":"assistant","content":"回答テキストの内容です。"}}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn test_short_message_filtered() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"短い"}}"#,
            r#"{"message":{"role":"assistant","content":"短い回答"}}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn test_long_message_capped() {
        // The per-message cap is a content decision (how much of a pasted
        // log is worth indexing) — the unit stays whole, just truncated.
        let long_text = "あ".repeat(1000);
        let line = format!(r#"{{"message":{{"role":"assistant","content":"{long_text}"}}}}"#);
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"テスト質問のテキストです。"}}"#,
            &line,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
        let answer = units[0].answer.as_deref().unwrap();
        assert!(answer.ends_with("..."));
        assert_eq!(answer.chars().count(), MAX_CHUNK_CHARS + 3);
    }

    #[test]
    fn test_invalid_json_skipped() {
        let f = write_jsonl(&[
            "not valid json",
            r#"{"message":{"role":"user","content":"有効なメッセージテキスト。"}}"#,
            r#"{"message":{"role":"assistant","content":"有効な回答テキストです。"}}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn test_timestamp_extracted() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"これはテストの質問です。長いテキスト。"},"timestamp":"2026-03-30T08:00:00.000Z"}"#,
            r#"{"message":{"role":"assistant","content":"これはテストの回答です。長いテキスト。"},"timestamp":"2026-03-30T08:01:00.000Z"}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
        // A Q&A unit carries its first message's (the question's) timestamp
        assert_eq!(
            units[0].timestamp.as_deref(),
            Some("2026-03-30T08:00:00.000Z")
        );
    }

    #[test]
    fn test_timestamp_none_when_missing() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"user","content":"タイムスタンプなしの質問メッセージ。"}}"#,
            r#"{"message":{"role":"assistant","content":"タイムスタンプなしの回答メッセージ。"}}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].timestamp.is_none());
    }

    #[test]
    fn test_empty_file() {
        let f = write_jsonl(&[]);
        let units = parse_session_jsonl(f.path()).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn test_assistant_first() {
        let f = write_jsonl(&[
            r#"{"message":{"role":"assistant","content":"先に来たアシスタントのメッセージ。"}}"#,
            r#"{"message":{"role":"user","content":"後から来たユーザーのメッセージ。"}}"#,
            r#"{"message":{"role":"assistant","content":"ペアになるアシスタントのメッセージ。"}}"#,
        ]);
        let units = parse_session_jsonl(f.path()).unwrap();
        // First assistant is an orphan unit, then a user+assistant pair
        assert_eq!(units.len(), 2);
        assert!(units[0].question.is_none());
        assert!(units[1].question.is_some() && units[1].answer.is_some());
    }

    #[test]
    fn test_session_to_markdown_frontmatter_timestamps() {
        let units = vec![
            QaUnit {
                question: Some("最初の質問です。".to_string()),
                answer: Some("最初の回答です。".to_string()),
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            },
            QaUnit {
                question: Some("次の質問です。".to_string()),
                answer: Some("次の回答です。".to_string()),
                timestamp: Some("2026-02-02T00:00:00Z".to_string()),
            },
        ];

        let md = session_to_markdown(&units, "2026-03-03T00:00:00Z");

        let (fm, _) = crate::frontmatter::parse(&md);
        assert_eq!(fm.status.as_deref(), Some("current"));
        assert_eq!(fm.created.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(fm.updated.as_deref(), Some("2026-02-02T00:00:00Z"));
    }

    #[test]
    fn test_session_to_markdown_fallback_timestamp() {
        let units = vec![QaUnit {
            question: Some("タイムスタンプなしの質問。".to_string()),
            answer: None,
            timestamp: None,
        }];

        let md = session_to_markdown(&units, "2026-03-03T00:00:00Z");

        let (fm, _) = crate::frontmatter::parse(&md);
        assert_eq!(fm.created.as_deref(), Some("2026-03-03T00:00:00Z"));
        assert_eq!(fm.updated.as_deref(), Some("2026-03-03T00:00:00Z"));
    }

    #[test]
    fn test_session_to_markdown_one_h2_per_unit() {
        let units = vec![
            QaUnit {
                question: Some("一つ目の質問文。".to_string()),
                answer: Some("一つ目の回答文。".to_string()),
                timestamp: None,
            },
            QaUnit {
                question: Some("二つ目の質問文。".to_string()),
                answer: Some("二つ目の回答文。".to_string()),
                timestamp: None,
            },
        ];

        let md = session_to_markdown(&units, "2026-01-01T00:00:00Z");

        let h2_count = md.lines().filter(|l| l.starts_with("## ")).count();
        assert_eq!(h2_count, 2, "one H2 heading per Q&A unit");
        assert!(md.contains("## Q: 一つ目の質問文。"));
    }

    #[test]
    fn test_session_to_markdown_escapes_embedded_heading_lines() {
        // Message text containing Markdown headings (assistant answers often
        // do) must not create extra section boundaries in the chunker.
        let units = vec![QaUnit {
            question: Some("見出しを含む質問です。".to_string()),
            answer: Some(
                "回答です。\n\n## 埋め込み見出し\n\n### サブ見出し\n\n#### H4はそのまま\n\n本文です。"
                    .to_string(),
            ),
            timestamp: None,
        }];

        let md = session_to_markdown(&units, "2026-01-01T00:00:00Z");

        assert!(md.contains("\\## 埋め込み見出し"));
        assert!(md.contains("\\### サブ見出し"));
        assert!(
            md.contains("\n#### H4はそのまま"),
            "H4+ is not a chunk boundary and stays unescaped"
        );
    }

    /// The core equivalence guarantee (#366): serialized Markdown, run through
    /// the standard chunker, reproduces the Q&A granularity — one chunk per
    /// unit, heading-based section_path, even when message text contains
    /// heading-looking lines.
    #[test]
    fn test_markdown_roundtrip_preserves_qa_boundaries() {
        let units = vec![
            QaUnit {
                question: Some("最初の質問です。".to_string()),
                answer: Some("最初の回答です。".to_string()),
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            },
            QaUnit {
                question: Some("見出し入りの質問。".to_string()),
                answer: Some("回答本文。\n\n## 埋め込み見出し\n\n本文が続きます。".to_string()),
                timestamp: Some("2026-01-02T00:00:00Z".to_string()),
            },
            QaUnit {
                question: None,
                answer: Some("孤立したアシスタントメッセージです。".to_string()),
                timestamp: None,
            },
        ];

        let md = session_to_markdown(&units, "2026-01-03T00:00:00Z");
        let (_, body) = crate::frontmatter::parse(&md);
        let out = crate::chunker::chunk_markdown_default(body, "session", "abc123");

        assert_eq!(out.len(), units.len(), "one chunk per Q&A unit");
        assert!(out[0].content.contains("最初の質問です。"));
        assert!(out[1].content.contains("埋め込み見出し"));
        assert!(out[2].content.contains("孤立したアシスタント"));
        // section_path is heading-based (was the constant "session" before)
        assert!(out[1].section_path.contains("見出し入りの質問"));
    }

    /// Boundary decisions belong to the markdown chunker: an oversized unit
    /// is ONE H2 section that the chunker splits at the Q/A paragraph
    /// boundary — the transform itself never pre-splits.
    #[test]
    fn test_markdown_roundtrip_oversized_unit_splits_at_qa_boundary() {
        let units = vec![QaUnit {
            question: Some("q".repeat(600)),
            answer: Some("a".repeat(600)),
            timestamp: None,
        }];

        let md = session_to_markdown(&units, "2026-01-01T00:00:00Z");
        let h2_count = md.lines().filter(|l| l.starts_with("## ")).count();
        assert_eq!(h2_count, 1, "the transform emits one section per unit");

        let (_, body) = crate::frontmatter::parse(&md);
        let out = crate::chunker::chunk_markdown_default(body, "session", "abc123");

        assert_eq!(out.len(), 2, "the chunker splits into Q and A chunks");
        assert!(out[0].content.contains("Q: qqq"));
        assert!(out[1].content.contains("A: aaa"));
    }
}
