//! Pure embedder-side helpers extracted from the embedder process shell.
//!
//! These take values and return values — the UNIX socket I/O, the model load,
//! and the `catch_unwind` itself stay in `embedder_proc.rs`. Keeping the request
//! parsing, panic-message extraction, response shaping, and model-load decision
//! here makes them unit-testable (and counted) without a live embedder.

use std::any::Any;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

/// Extract the `texts` array from an embed request, dropping non-string and
/// absent entries. A missing or non-array `texts` yields an empty vec (the
/// embedder then returns no embeddings rather than erroring).
pub fn parse_texts(request: &Value) -> Vec<String> {
    request
        .get("texts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Recover a human-readable message from a panic payload (`catch_unwind`'s
/// `Err`). Panics carry either a `String` or a `&'static str`; anything else
/// degrades to a fixed label.
pub fn panic_message(payload: &dyn Any) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(&s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "unknown panic".to_string()
    }
}

/// Shape the embedder's reply to an encode call. `Ok` carries the embeddings
/// under `embeddings`; `Err` carries the error string under `error`. (The panic
/// case is shaped by the caller, which prefixes `panic:` after
/// [`panic_message`].)
pub fn encode_response(result: Result<Vec<Vec<f32>>>) -> Value {
    match result {
        Ok(embeddings) => serde_json::json!({ "embeddings": embeddings }),
        Err(e) => serde_json::json!({ "error": format!("{e}") }),
    }
}

/// How the embedder should obtain its model, given an optional explicit
/// directory and whether that directory holds every required file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLoadPlan {
    /// Load from the explicit directory (all files present).
    FromDir,
    /// The explicit directory is incomplete; warn and fall back to default
    /// resolution (state-dir override → cache-dir).
    FallbackIncomplete,
    /// No directory was given; use default resolution.
    Default,
}

/// Decide how to load the model. The filesystem completeness check is the
/// caller's job (it passes `all_files_present`); this only encodes the policy.
pub fn resolve_model_load(model_dir: Option<&Path>, all_files_present: bool) -> ModelLoadPlan {
    match model_dir {
        Some(_) if all_files_present => ModelLoadPlan::FromDir,
        Some(_) => ModelLoadPlan::FallbackIncomplete,
        None => ModelLoadPlan::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_parse_texts_extracts_strings() {
        let req = serde_json::json!({ "texts": ["a", "b", "c"] });
        assert_eq!(parse_texts(&req), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_texts_empty_array() {
        let req = serde_json::json!({ "texts": [] });
        assert!(parse_texts(&req).is_empty());
    }

    #[test]
    fn test_parse_texts_missing_key() {
        let req = serde_json::json!({ "other": 1 });
        assert!(parse_texts(&req).is_empty());
    }

    #[test]
    fn test_parse_texts_non_array() {
        let req = serde_json::json!({ "texts": "not-an-array" });
        assert!(parse_texts(&req).is_empty());
    }

    #[test]
    fn test_parse_texts_drops_non_string_entries() {
        let req = serde_json::json!({ "texts": ["a", 1, null, "b", true] });
        assert_eq!(parse_texts(&req), vec!["a", "b"]);
    }

    #[test]
    fn test_panic_message_from_string() {
        let payload: Box<dyn Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*payload), "boom");
    }

    #[test]
    fn test_panic_message_from_str() {
        let payload: Box<dyn Any + Send> = Box::new("boom-static");
        assert_eq!(panic_message(&*payload), "boom-static");
    }

    #[test]
    fn test_panic_message_unknown() {
        let payload: Box<dyn Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(&*payload), "unknown panic");
    }

    #[test]
    fn test_encode_response_ok() {
        let resp = encode_response(Ok(vec![vec![0.5, 0.25], vec![1.0]]));
        assert_eq!(
            resp,
            serde_json::json!({ "embeddings": [[0.5, 0.25], [1.0]] })
        );
    }

    #[test]
    fn test_encode_response_err() {
        let resp = encode_response(Err(anyhow::anyhow!("model exploded")));
        assert_eq!(resp, serde_json::json!({ "error": "model exploded" }));
    }

    #[test]
    fn test_resolve_model_load_from_dir_when_complete() {
        let dir = PathBuf::from("/cache/ruri-v3-30m");
        assert_eq!(resolve_model_load(Some(&dir), true), ModelLoadPlan::FromDir);
    }

    #[test]
    fn test_resolve_model_load_fallback_when_incomplete() {
        let dir = PathBuf::from("/cache/ruri-v3-30m");
        assert_eq!(
            resolve_model_load(Some(&dir), false),
            ModelLoadPlan::FallbackIncomplete
        );
    }

    #[test]
    fn test_resolve_model_load_default_when_no_dir() {
        assert_eq!(resolve_model_load(None, false), ModelLoadPlan::Default);
        // Defensive: a `true` flag with no dir is still Default (no dir to use).
        assert_eq!(resolve_model_load(None, true), ModelLoadPlan::Default);
    }
}
