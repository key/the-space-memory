use std::collections::HashSet;
use std::sync::OnceLock;

use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;

use crate::config;

static SEGMENTER: OnceLock<Segmenter> = OnceLock::new();
static STOPWORDS: OnceLock<HashSet<String>> = OnceLock::new();

pub fn get_segmenter() -> &'static Segmenter {
    SEGMENTER.get_or_init(|| {
        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
            .expect("Failed to load IPADIC dictionary");
        Segmenter::new(Mode::Normal, dictionary, None)
    })
}

/// A token with surface form and byte positions in the original text.
#[derive(Debug, Clone)]
pub struct Token {
    pub surface: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Tokenize text into tokens with byte positions.
pub fn tokenize(text: &str) -> Vec<Token> {
    if text.is_empty() {
        return Vec::new();
    }
    let segmenter = get_segmenter();
    let tokens = segmenter
        .segment(std::borrow::Cow::Borrowed(text))
        .unwrap_or_default();
    let mut result = Vec::new();
    let mut byte_pos = 0;
    for t in &tokens {
        let surface = t.surface.as_ref();
        // Find the surface in the original text starting from byte_pos
        if let Some(offset) = text[byte_pos..].find(surface) {
            let start = byte_pos + offset;
            let end = start + surface.len();
            result.push(Token {
                surface: surface.to_string(),
                byte_start: start,
                byte_end: end,
            });
            byte_pos = end;
        } else {
            // Token not found at expected position — include with estimated position
            result.push(Token {
                surface: surface.to_string(),
                byte_start: byte_pos,
                byte_end: byte_pos,
            });
        }
    }
    result
}

/// Tokenize text into space-separated tokens (wakachi-gaki).
pub fn wakachi(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let segmenter = get_segmenter();
    let tokens = segmenter
        .segment(std::borrow::Cow::Borrowed(text))
        .unwrap_or_default();
    tokens
        .iter()
        .map(|t| t.surface.as_ref())
        .collect::<Vec<&str>>()
        .join(" ")
}

/// Load stopwords from data/stopwords.txt (one word per line).
fn get_stopwords() -> &'static HashSet<String> {
    STOPWORDS.get_or_init(|| {
        let path = config::stopwords_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => content
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
            Err(_) => HashSet::new(),
        }
    })
}

/// Extract meaningful search keywords from text using morphological analysis.
///
/// Filters tokens to keep only nouns (general, proper, unknown) and removes
/// stopwords. Returns the filtered surface forms suitable for search queries.
pub fn extract_search_keywords(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let segmenter = get_segmenter();
    let stopwords = get_stopwords();
    let mut tokens = segmenter
        .segment(std::borrow::Cow::Borrowed(text))
        .unwrap_or_default();

    let mut keywords = Vec::new();
    for token in tokens.iter_mut() {
        let details = token.details();

        // Keep only nouns: 名詞-一般, 名詞-固有名詞, 名詞-サ変接続, 名詞-未知語, etc.
        // Skip 名詞-非自立 (もの, こと, etc.) and 名詞-接尾 (的, 化, etc.)
        // and 名詞-代名詞 (これ, それ, etc.)
        if details.is_empty() || details[0] != "名詞" {
            continue;
        }
        if details.len() >= 2
            && matches!(
                details[1],
                "非自立" | "接尾" | "代名詞" | "数"
            )
        {
            continue;
        }

        let surface = token.surface.as_ref();

        // Skip single-char hiragana/katakana tokens (particles that got tagged as nouns)
        let chars: Vec<char> = surface.chars().collect();
        if chars.len() == 1 {
            let c = chars[0];
            if ('\u{3040}'..='\u{309F}').contains(&c) || ('\u{30A0}'..='\u{30FF}').contains(&c) {
                continue;
            }
        }

        // Skip tokens that are only prolonged sound marks, punctuation, or symbols
        if surface.chars().all(|c| {
            c == 'ー'
                || c == '〜'
                || c == '…'
                || c == 'w'
                || c == 'W'
                || c.is_ascii_punctuation()
        }) {
            continue;
        }

        // Skip stopwords
        if stopwords.contains(surface) {
            continue;
        }

        keywords.push(surface.to_string());
    }

    // Deduplicate while preserving order
    let mut seen = HashSet::new();
    keywords.retain(|k| seen.insert(k.clone()));

    keywords
}

/// Extract proper noun surface forms from text using IPADIC POS analysis.
/// Returns raw surface forms (not normalized).
pub fn extract_proper_nouns(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let segmenter = get_segmenter();
    let mut tokens = segmenter
        .segment(std::borrow::Cow::Borrowed(text))
        .unwrap_or_default();
    tokens
        .iter_mut()
        .filter_map(|token| {
            let details = token.details();
            if details.len() >= 2 && details[0] == "名詞" && details[1] == "固有名詞" {
                Some(token.surface.as_ref().to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_japanese() {
        let result = wakachi("射撃場のルール");
        let tokens: Vec<&str> = result.split_whitespace().collect();
        assert!(tokens.len() >= 2);
    }

    #[test]
    fn test_two_char_word() {
        let result = wakachi("射撃");
        assert!(!result.is_empty());
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(wakachi(""), "");
    }

    #[test]
    fn test_english() {
        let result = wakachi("hello world");
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_mixed_ja_en() {
        let result = wakachi("Rustでの開発");
        let tokens: Vec<&str> = result.split_whitespace().collect();
        assert!(tokens.len() >= 2);
    }

    #[test]
    fn test_singleton_cached() {
        let s1 = get_segmenter() as *const Segmenter;
        let s2 = get_segmenter() as *const Segmenter;
        assert_eq!(s1, s2);
    }

    // ─── extract_proper_nouns tests ──────────────────────────

    #[test]
    fn test_extract_proper_nouns_japanese() {
        let result = extract_proper_nouns("東京タワーは有名な観光地です");
        assert!(
            !result.is_empty(),
            "Should extract at least one proper noun from Japanese text"
        );
    }

    #[test]
    fn test_extract_proper_nouns_empty() {
        assert!(extract_proper_nouns("").is_empty());
    }

    #[test]
    fn test_extract_proper_nouns_no_proper() {
        let result = extract_proper_nouns("走る食べる寝る");
        // Verbs should not be extracted as proper nouns
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_proper_nouns_mixed() {
        let result = extract_proper_nouns("田中さんが東京に行った");
        // Should find at least one proper noun (田中 or 東京)
        assert!(!result.is_empty());
    }

    // ─── extract_search_keywords tests ──────────────────────────

    #[test]
    fn test_keywords_empty() {
        assert!(extract_search_keywords("").is_empty());
    }

    #[test]
    fn test_keywords_technical_term() {
        let result = extract_search_keywords("LoRaモジュールの開発");
        // Should include LoRa, モジュール, 開発 (all nouns)
        assert!(!result.is_empty());
        let joined = result.join(" ");
        assert!(
            joined.contains("LoRa") || joined.contains("モジュール") || joined.contains("開発"),
            "Should extract at least one meaningful noun: {:?}",
            result
        );
    }

    #[test]
    fn test_keywords_filters_particles() {
        let result = extract_search_keywords("射撃場のルールについて");
        // Particles (の, について) should be removed, nouns (射撃, 場, ルール) kept
        for kw in &result {
            assert_ne!(kw, "の");
            assert_ne!(kw, "に");
            assert_ne!(kw, "つい");
            assert_ne!(kw, "て");
        }
        assert!(!result.is_empty());
    }

    #[test]
    fn test_keywords_interjection_only() {
        // Pure interjection/greeting — should produce few or no keywords
        let result = extract_search_keywords("よかったーーーー");
        // "よかった" is an adjective, should be filtered out (not a noun)
        assert!(
            result.is_empty(),
            "Pure adjective/interjection should produce no noun keywords: {:?}",
            result
        );
    }

    #[test]
    fn test_keywords_stopword_removal() {
        // "なるほど" is in the stopword list
        let result = extract_search_keywords("なるほど");
        assert!(
            !result.contains(&"なるほど".to_string()),
            "Stopword should be removed: {:?}",
            result
        );
    }

    #[test]
    fn test_keywords_mixed_noise_and_content() {
        let result = extract_search_keywords("なるほど、LoRaモジュールについて教えて");
        let joined = result.join(" ");
        assert!(
            joined.contains("LoRa") || joined.contains("モジュール"),
            "Should keep content keywords: {:?}",
            result
        );
        assert!(
            !result.contains(&"なるほど".to_string()),
            "Should remove stopwords: {:?}",
            result
        );
    }

    #[test]
    fn test_keywords_english() {
        let result = extract_search_keywords("LoRa module development");
        assert!(!result.is_empty(), "English nouns should be extracted");
    }

    #[test]
    fn test_keywords_deduplicates() {
        let result = extract_search_keywords("LoRa LoRa LoRa");
        let lora_count = result.iter().filter(|k| k.as_str() == "LoRa").count();
        assert!(lora_count <= 1, "Should deduplicate: {:?}", result);
    }

    #[test]
    fn test_keywords_pronoun_filtered() {
        let result = extract_search_keywords("これはそれです");
        // 代名詞 (これ, それ) should be filtered
        assert!(
            !result.contains(&"これ".to_string()),
            "Pronouns should be filtered: {:?}",
            result
        );
        assert!(
            !result.contains(&"それ".to_string()),
            "Pronouns should be filtered: {:?}",
            result
        );
    }
}
