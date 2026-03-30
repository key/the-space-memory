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

                let pair = format!("Q: {q_text}\nA: {a_text}");
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
