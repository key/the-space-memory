use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use lindera::dictionary::{
    load_embedded_dictionary, load_user_dictionary_from_csv, DictionaryKind, UserDictionary,
};
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;

use crate::config;

// ─── IPADIC POS labels ──────────────────────────────────────
// These constants come from the IPADIC dictionary format used by lindera.
// lindera returns POS info via token.details(): details[0] is the top-level
// POS category, details[1..] are subcategories specific to IPADIC's schema.
// simpledic (user dict) entries only have [pos, reading] — subcategories
// do not apply to user dictionary tokens.
//
// Top-level (details[0])
pub const POS_NOUN: &str = "名詞";
// Noun subcategories (details[1]) — IPADIC-specific, not used by simpledic
pub const POS_SUB_PROPER: &str = "固有名詞";
pub const POS_SUB_DEPENDENT: &str = "非自立";
pub const POS_SUB_SUFFIX: &str = "接尾";
pub const POS_SUB_PRONOUN: &str = "代名詞";
pub const POS_SUB_NUMBER: &str = "数";

static SEGMENTER: Mutex<Option<Arc<Segmenter>>> = Mutex::new(None);
static STOPWORDS: OnceLock<HashSet<String>> = OnceLock::new();

pub fn get_segmenter() -> Arc<Segmenter> {
    let mut guard = SEGMENTER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(seg) = guard.as_ref() {
        return Arc::clone(seg);
    }
    let dictionary =
        load_embedded_dictionary(DictionaryKind::IPADIC).expect("Failed to load IPADIC dictionary");
    let user_dict = load_user_dict(&dictionary.metadata);
    let seg = Arc::new(Segmenter::new(Mode::Normal, dictionary, user_dict));
    *guard = Some(Arc::clone(&seg));
    seg
}

/// Invalidate the cached segmenter so that the next call to
/// `get_segmenter()` reloads the dictionary (including user dict).
pub fn reset_segmenter() {
    let mut guard = SEGMENTER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Strip comment lines (`#`) and blank lines from simpledic content.
/// Returns only the data lines joined by newlines, or `None` if empty.
fn strip_simpledic_comments(content: &str) -> Option<String> {
    let lines: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn load_user_dict(metadata: &lindera::dictionary::Metadata) -> Option<UserDictionary> {
    let path = config::user_dict_path();

    if !path.exists() {
        return None;
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("failed to read user dictionary: {e}");
            return None;
        }
    };

    // Strip comments and blank lines before passing to lindera
    // (lindera's CSV parser does not support comments)
    let stripped = strip_simpledic_comments(&content)?;

    // Write stripped content to a temp file for lindera
    let tmp_path = std::env::temp_dir().join("tsm_user_dict.simpledic");
    if let Err(e) = std::fs::write(&tmp_path, &stripped) {
        log::warn!("failed to write temp user dictionary: {e}");
        return None;
    }

    let result = load_user_dictionary_from_csv(metadata, &tmp_path);
    let _ = std::fs::remove_file(&tmp_path);

    match result {
        Ok(ud) => {
            log::info!("user dictionary loaded: {}", path.display());
            Some(ud)
        }
        Err(e) => {
            log::warn!("failed to load user dictionary: {e}");
            None
        }
    }
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

        // Keep only nouns: 名詞-一般, 名詞-固有名詞, 名詞-サ変接続, etc.
        // User dictionary terms also use POS "名詞" so they pass this filter naturally.
        // Skip 名詞-非自立 (もの, こと, etc.) and 名詞-接尾 (的, 化, etc.)
        // and 名詞-代名詞 (これ, それ, etc.)
        if details.is_empty() || details[0] != POS_NOUN {
            continue;
        }
        if details.len() >= 2
            && matches!(
                details[1],
                POS_SUB_DEPENDENT | POS_SUB_SUFFIX | POS_SUB_PRONOUN | POS_SUB_NUMBER
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
            c == 'ー' || c == '〜' || c == '…' || c == 'w' || c == 'W' || c.is_ascii_punctuation()
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
            if details.len() >= 2 && details[0] == POS_NOUN && details[1] == POS_SUB_PROPER {
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
    #[serial_test::serial]
    fn test_singleton_cached() {
        let s1 = get_segmenter();
        let s2 = get_segmenter();
        assert!(Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    #[serial_test::serial]
    fn test_reset_segmenter() {
        let s1 = get_segmenter();
        reset_segmenter();
        let s2 = get_segmenter();
        assert!(
            !Arc::ptr_eq(&s1, &s2),
            "After reset, get_segmenter() should return a new instance"
        );
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

    // ─── strip_simpledic_comments tests ─────────────────────────

    #[test]
    fn test_strip_simpledic_comments_removes_comments_and_blanks() {
        let input = "# date comment\n\nLoRa,名詞,LoRa\n# another\ntsmd,名詞,tsmd\n";
        let result = strip_simpledic_comments(input).unwrap();
        assert_eq!(result, "LoRa,名詞,LoRa\ntsmd,名詞,tsmd");
    }

    #[test]
    fn test_strip_simpledic_comments_empty_returns_none() {
        assert!(strip_simpledic_comments("").is_none());
        assert!(strip_simpledic_comments("# only comments\n\n").is_none());
    }

    // ─── user dictionary tests ────────────────────────────────

    #[test]
    fn test_user_dict_loaded_into_segmenter() {
        use lindera::dictionary::{
            load_embedded_dictionary, load_user_dictionary_from_csv, DictionaryKind,
        };
        use lindera::mode::Mode;
        use lindera::segmenter::Segmenter;

        // Create a temp user dict with a compound term
        let dir = tempfile::TempDir::new().unwrap();
        let dict_path = dir.path().join("test.simpledic");
        // lindera user dict: 3 fields (surface, part_of_speech, reading)
        std::fs::write(&dict_path, "ドッグトラッカー,名詞,ドッグトラッカー\n").unwrap();

        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC).unwrap();
        let user_dict = load_user_dictionary_from_csv(&dictionary.metadata, &dict_path).unwrap();
        let segmenter = Segmenter::new(Mode::Normal, dictionary, Some(user_dict));

        // With user dict: "ドッグトラッカー" should be one token
        let mut tokens = segmenter
            .segment(std::borrow::Cow::Borrowed("ドッグトラッカーの開発"))
            .unwrap();
        let surfaces: Vec<String> = tokens
            .iter_mut()
            .map(|t| t.surface.as_ref().to_string())
            .collect();
        assert!(
            surfaces.contains(&"ドッグトラッカー".to_string()),
            "User dict term should be a single token: {:?}",
            surfaces
        );
    }

    #[test]
    fn test_load_user_dict_nonexistent() {
        use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC).unwrap();
        // load_user_dict uses config::user_dict_path() which may not exist in test
        // Directly test that nonexistent path returns None
        let result = load_user_dictionary_from_csv(
            &dictionary.metadata,
            std::path::Path::new("/nonexistent/dict.simpledic"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_keywords_includes_user_dict_terms() {
        use lindera::dictionary::{
            load_embedded_dictionary, load_user_dictionary_from_csv, DictionaryKind,
        };
        use lindera::mode::Mode;
        use lindera::segmenter::Segmenter;

        let dir = tempfile::TempDir::new().unwrap();
        let dict_path = dir.path().join("test.simpledic");
        std::fs::write(&dict_path, "ドッグトラッカー,名詞,ドッグトラッカー\n").unwrap();

        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC).unwrap();
        let user_dict = load_user_dictionary_from_csv(&dictionary.metadata, &dict_path).unwrap();
        let segmenter = Segmenter::new(Mode::Normal, dictionary, Some(user_dict));

        // Tokenize and extract keywords using the same logic as extract_search_keywords
        let stopwords = get_stopwords();
        let mut tokens = segmenter
            .segment(std::borrow::Cow::Borrowed("ドッグトラッカーの開発"))
            .unwrap();

        let mut keywords = Vec::new();
        for token in tokens.iter_mut() {
            let details = token.details();
            if details.is_empty() || details[0] != POS_NOUN {
                continue;
            }
            if details.len() >= 2
                && matches!(
                    details[1],
                    POS_SUB_DEPENDENT | POS_SUB_SUFFIX | POS_SUB_PRONOUN | POS_SUB_NUMBER
                )
            {
                continue;
            }
            let surface = token.surface.as_ref();
            if !stopwords.contains(surface) {
                keywords.push(surface.to_string());
            }
        }

        assert!(
            keywords.contains(&"ドッグトラッカー".to_string()),
            "User dict term with 名詞 POS should be included in keywords: {:?}",
            keywords
        );
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
