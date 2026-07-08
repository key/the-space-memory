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

/// Encode function type: takes a slice of texts, returns embedding vectors.
/// Mirrors `crate::indexer::embed::EncodeFn` — same shape, separate binary.
pub type EncodeFn<'a> = &'a dyn Fn(&[String]) -> Result<Vec<Vec<f32>>>;

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
///
/// The two directory-bearing variants carry the path, so the caller pattern-binds
/// it instead of re-deriving it from the original `Option` — the "a directory was
/// given" invariant lives in the type, not in a panic message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLoadPlan<'a> {
    /// Load from the explicit directory (all files present).
    FromDir(&'a Path),
    /// The explicit directory is incomplete; warn and fall back to default
    /// resolution (state-dir override → cache-dir).
    FallbackIncomplete(&'a Path),
    /// No directory was given; use default resolution.
    Default,
}

/// Decide how to load the model. The filesystem completeness check is the
/// caller's job (it passes `all_files_present`); this only encodes the policy.
pub fn resolve_model_load(model_dir: Option<&Path>, all_files_present: bool) -> ModelLoadPlan<'_> {
    match model_dir {
        Some(dir) if all_files_present => ModelLoadPlan::FromDir(dir),
        Some(dir) => ModelLoadPlan::FallbackIncomplete(dir),
        None => ModelLoadPlan::Default,
    }
}

/// Split `texts` into sub-batches of at most `cap` items, run `encode` on each
/// sub-batch, and concatenate the resulting embeddings in order.
///
/// This is the server-side counterpart of the indexer's client-side
/// `BACKFILL_BATCH_SIZE` cap: a client that talks to the embedder socket
/// directly (bypassing the indexer) could otherwise submit an arbitrarily
/// large `texts` array — bounded only by ipc's 64MB message cap — and force
/// candle's ModernBert to materialize an O(batch × heads × seq²) attention
/// tensor per layer. Sub-batching here means no caller, trusted or not, can
/// trigger that blowup through the socket.
///
/// A failure in any sub-batch aborts the whole call and returns that error,
/// matching the pre-existing whole-request failure behavior (the caller
/// still sees one `Err` for the whole request, never a partial result).
pub fn encode_in_batches(
    encode: EncodeFn,
    texts: &[String],
    cap: usize,
) -> Result<Vec<Vec<f32>>> {
    let cap = cap.max(1);
    let mut result = Vec::with_capacity(texts.len());
    for batch in texts.chunks(cap) {
        result.extend(encode(batch)?);
    }
    Ok(result)
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
    fn test_parse_texts_null_value() {
        // `texts` present but JSON null: `null.as_array()` is None → empty vec.
        let req = serde_json::json!({ "texts": null });
        assert!(parse_texts(&req).is_empty());
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
    fn test_encode_response_ok_empty() {
        // Empty texts → embedder returns Ok(vec![]); the reply is an empty
        // embeddings array, NOT an error.
        let resp = encode_response(Ok(vec![]));
        assert_eq!(resp, serde_json::json!({ "embeddings": [] }));
    }

    #[test]
    fn test_resolve_model_load_from_dir_when_complete() {
        let dir = PathBuf::from("/cache/ruri-v3-30m");
        assert_eq!(
            resolve_model_load(Some(&dir), true),
            ModelLoadPlan::FromDir(&dir)
        );
    }

    #[test]
    fn test_resolve_model_load_fallback_when_incomplete() {
        let dir = PathBuf::from("/cache/ruri-v3-30m");
        assert_eq!(
            resolve_model_load(Some(&dir), false),
            ModelLoadPlan::FallbackIncomplete(&dir)
        );
    }

    #[test]
    fn test_resolve_model_load_default_when_no_dir() {
        assert_eq!(resolve_model_load(None, false), ModelLoadPlan::Default);
        // Defensive: a `true` flag with no dir is still Default (no dir to use).
        assert_eq!(resolve_model_load(None, true), ModelLoadPlan::Default);
    }

    // ─── encode_in_batches ─────────────────────────────

    /// Deterministic stand-in for a real encode call: one embedding per text,
    /// each holding the text's length so order is verifiable end-to-end.
    fn len_encode(texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| vec![t.len() as f32]).collect())
    }

    #[test]
    fn test_encode_in_batches_preserves_order_across_sub_batches() {
        let texts: Vec<String> = (0..10).map(|i| "x".repeat(i + 1)).collect();
        let result = encode_in_batches(&len_encode, &texts, 3).unwrap();
        let expected: Vec<Vec<f32>> = texts.iter().map(|t| vec![t.len() as f32]).collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_encode_in_batches_respects_cap() {
        let texts: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let sizes = std::cell::RefCell::new(Vec::new());
        let recording = |batch: &[String]| {
            sizes.borrow_mut().push(batch.len());
            len_encode(batch)
        };
        let result = encode_in_batches(&recording, &texts, 4).unwrap();
        assert_eq!(*sizes.borrow(), vec![4, 4, 2]);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn test_encode_in_batches_propagates_error_from_any_sub_batch() {
        let texts: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
        let calls = std::cell::Cell::new(0usize);
        let flaky = |batch: &[String]| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                anyhow::bail!("sub-batch encode failed");
            }
            len_encode(batch)
        };
        let result = encode_in_batches(&flaky, &texts, 2);
        assert!(result.is_err());
        assert_eq!(calls.get(), 2, "should stop at the failing sub-batch");
    }

    #[test]
    fn test_encode_in_batches_empty_input() {
        let texts: Vec<String> = Vec::new();
        let result = encode_in_batches(&len_encode, &texts, 4).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_encode_in_batches_cap_zero_treated_as_one() {
        // A cap of 0 would make `chunks()` panic; guard against a misconfigured
        // caller by treating 0 as 1 (still makes progress, one text at a time).
        let texts: Vec<String> = vec!["a".to_string(), "bb".to_string()];
        let sizes = std::cell::RefCell::new(Vec::new());
        let recording = |batch: &[String]| {
            sizes.borrow_mut().push(batch.len());
            len_encode(batch)
        };
        let result = encode_in_batches(&recording, &texts, 0).unwrap();
        assert_eq!(*sizes.borrow(), vec![1, 1]);
        assert_eq!(result, vec![vec![1.0], vec![2.0]]);
    }
}
