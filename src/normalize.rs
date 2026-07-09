//! Unicode canonical normalization (NFC), applied at the pipeline's two entry
//! points — Index/Prepare and Search/Plan — plus load-time auxiliary points
//! for user-edited files (see #357). Compatibility folding (NFKC: half/full
//! width, dash variants) is explicitly out of scope; see the issue for why.

use std::borrow::Cow;

use unicode_normalization::{is_nfc, is_nfc_quick, IsNormalized, UnicodeNormalization};

/// Normalize `text` to Unicode NFC (canonical composition).
///
/// Fast path: `is_nfc_quick` recognizes the overwhelming majority of inputs
/// (already-composed Markdown/Japanese text) as NFC without allocating,
/// returning `Cow::Borrowed`. Anything not conclusively NFC (`No` or the
/// ambiguous `Maybe`) takes the slow path and is recomposed into an owned
/// `String`.
pub fn nfc(text: &str) -> Cow<'_, str> {
    match is_nfc_quick(text.chars()) {
        IsNormalized::Yes => Cow::Borrowed(text),
        _ => Cow::Owned(text.nfc().collect()),
    }
}

/// Whether `text` is already NFC-normalized. Used as a `debug_assert!` safety
/// net at points downstream of [`nfc`] that must never see non-NFC content.
pub fn is_normalized(text: &str) -> bool {
    is_nfc(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nfd_composes_to_nfc() {
        // "ワーカー" with the dakuten on カ held as a combining character
        // (U+30AB U+3099) rather than the precomposed ガ... here specifically
        // the long vowel mark katakana with a decomposed voiced sound mark.
        let nfd = "\u{30ef}\u{30fc}\u{30ab}\u{3099}\u{30fc}"; // ワーガ(decomposed)ー
        let nfc_form = "\u{30ef}\u{30fc}\u{30ac}\u{30fc}"; // ワーガー (precomposed)

        let result = nfc(nfd);

        assert_eq!(result.as_ref(), nfc_form);
        assert!(is_normalized(&result));
    }

    #[test]
    fn test_already_nfc_borrows() {
        let text = "ワーカースレッドの実装について";

        let result = nfc(text);

        assert!(
            matches!(result, Cow::Borrowed(_)),
            "already-NFC input must not allocate"
        );
        assert_eq!(result.as_ref(), text);
    }

    #[test]
    fn test_ascii_passthrough() {
        let text = "hello world 123";

        let result = nfc(text);

        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result.as_ref(), text);
    }

    #[test]
    fn test_empty_string() {
        let result = nfc("");

        assert_eq!(result.as_ref(), "");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn test_mixed_content() {
        // ASCII + already-NFC Japanese + a decomposed sequence, all in one
        // string — the slow path must still normalize only what's needed
        // while preserving the rest verbatim.
        let nfd_ga = "\u{30ab}\u{3099}"; // ガ decomposed
        let text = format!("note: {nfd_ga} worker thread");
        let expected = "note: \u{30ac} worker thread"; // ガ precomposed

        let result = nfc(&text);

        assert_eq!(result.as_ref(), expected);
    }

    #[test]
    fn test_is_normalized_detects_nfd() {
        let nfd = "\u{30ab}\u{3099}"; // ガ decomposed
        let nfc_form = "\u{30ac}"; // ガ precomposed

        assert!(!is_normalized(nfd));
        assert!(is_normalized(nfc_form));
    }
}
