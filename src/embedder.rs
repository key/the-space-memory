use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use candle_core::{DType, Device, Tensor, D};
use tokenizers::Tokenizer;

use crate::config;
use crate::ipc::{read_message, write_message};

/// Bench-only instrumentation. Compiled out unless `bench-counters` feature is enabled.
///
/// Counts client-side `embed_via_socket_at` calls. Used by bench harness to verify
/// the regression invariant "embedder is not called more often than expected".
#[cfg(feature = "bench-counters")]
pub mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};

    static EMBEDDER_CALLS: AtomicU64 = AtomicU64::new(0);

    pub(super) fn increment_embedder_calls() {
        EMBEDDER_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    /// Total number of `embed_via_socket_at` calls since process start (or last reset).
    pub fn embedder_call_count() -> u64 {
        EMBEDDER_CALLS.load(Ordering::Relaxed)
    }

    /// Reset the counter to zero. For bench setup.
    pub fn reset_embedder_calls() {
        EMBEDDER_CALLS.store(0, Ordering::Relaxed);
    }
}

/// Abstraction over the model's forward pass so the encode pipeline can be
/// unit-tested against a mock. The real ruri/ModernBert wrapper lives behind
/// this trait in `model_loader.rs` (kept out of the coverage gate).
pub trait EmbeddingModel {
    /// Run the transformer forward pass: `(input_ids, attention_mask)` →
    /// hidden states shaped `(batch, seq_len, hidden)`.
    fn forward(&self, input_ids: &Tensor, attention_mask: &Tensor) -> Result<Tensor>;
}

/// The embedder engine: holds a model + tokenizer and produces embeddings.
///
/// The model is type-erased behind [`EmbeddingModel`] so the encode pipeline is
/// testable with a mock; construction from real files lives in `model_loader.rs`.
pub struct Embedder {
    model: Box<dyn EmbeddingModel>,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Assemble an embedder from its parts. Used by the loader (`model_loader.rs`)
    /// and by tests; the struct fields stay private.
    pub(crate) fn from_parts(
        model: Box<dyn EmbeddingModel>,
        tokenizer: Tokenizer,
        device: Device,
    ) -> Self {
        Self {
            model,
            tokenizer,
            device,
        }
    }

    /// Encode texts into embedding vectors.
    pub fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        encode_impl(self.model.as_ref(), &self.tokenizer, &self.device, texts)
    }

    /// Get the embedding dimension.
    pub fn dim(&self) -> usize {
        config::EMBEDDING_DIM
    }
}

/// Cheap pre-filter measured in BYTES (str::len), truncating on a UTF-8 char
/// boundary. Real bound is the tokenizer's MAX_TOKENS truncation (see
/// `model_loader::configure_tokenizer`); this only avoids tokenizing
/// pathologically large inputs.
fn truncate_texts(texts: &[String]) -> Vec<String> {
    const MAX_CHARS: usize = 8192;
    texts
        .iter()
        .map(|t| {
            if t.len() > MAX_CHARS {
                let mut end = MAX_CHARS;
                while end > 0 && !t.is_char_boundary(end) {
                    end -= 1;
                }
                t[..end].to_string()
            } else {
                t.clone()
            }
        })
        .collect()
}

/// Tokenize, run the model forward pass, and mean-pool into embedding vectors.
///
/// Parameterized over [`EmbeddingModel`] so it runs against a mock model and a
/// code-built tokenizer without any model files.
fn encode_impl(
    model: &dyn EmbeddingModel,
    tokenizer: &Tokenizer,
    device: &Device,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let truncated = truncate_texts(texts);

    let encodings = tokenizer
        .encode_batch(truncated, true)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let input_ids: Vec<Vec<u32>> = encodings.iter().map(|e| e.get_ids().to_vec()).collect();
    let attention_masks: Vec<Vec<u32>> = encodings
        .iter()
        .map(|e| e.get_attention_mask().to_vec())
        .collect();

    let batch_size = input_ids.len();
    let seq_len = input_ids[0].len();

    let input_ids_flat: Vec<u32> = input_ids.into_iter().flatten().collect();
    let mask_flat: Vec<u32> = attention_masks.into_iter().flatten().collect();

    let input_ids_tensor =
        Tensor::from_vec(input_ids_flat, (batch_size, seq_len), device)?.to_dtype(DType::U32)?;
    let attention_mask_tensor =
        Tensor::from_vec(mask_flat, (batch_size, seq_len), device)?.to_dtype(DType::U32)?;

    let output = model.forward(&input_ids_tensor, &attention_mask_tensor)?;
    let embeddings = mean_pooling(&output, &attention_mask_tensor)?;

    // Convert to Vec<Vec<f32>>
    let mut result = Vec::with_capacity(batch_size);
    for i in 0..batch_size {
        let row = embeddings.get(i)?;
        let values: Vec<f32> = row.to_vec1()?;
        result.push(values);
    }
    Ok(result)
}

/// Mean pooling with L2 normalization.
fn mean_pooling(output: &Tensor, attention_mask: &Tensor) -> Result<Tensor> {
    let mask = attention_mask.unsqueeze(D::Minus1)?.to_dtype(DType::F32)?;
    let masked = output.broadcast_mul(&mask)?;
    let sum = masked.sum(1)?;
    let count = attention_mask
        .sum_keepdim(1)?
        .to_dtype(DType::F32)?
        .clamp(1e-9, f64::MAX)?;
    let pooled = sum.broadcast_div(&count)?;

    let norms = pooled
        .sqr()?
        .sum_keepdim(D::Minus1)?
        .sqrt()?
        .clamp(1e-9, f64::MAX)?;
    Ok(pooled.broadcast_div(&norms)?)
}

// ─── Client ────────────────────────────────────────────────────────

/// Send texts to the embedder daemon and get embeddings back.
/// Returns None if the embedder is not running.
pub fn embed_via_socket(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    embed_via_socket_at(&config::embedder_socket_path(), texts)
}

/// Send texts to the embedder daemon at a specific socket path.
pub fn embed_via_socket_at(socket_path: &Path, texts: &[String]) -> Option<Vec<Vec<f32>>> {
    #[cfg(feature = "bench-counters")]
    counters::increment_embedder_calls();

    if !socket_path.exists() {
        return None;
    }

    let mut stream = UnixStream::connect(socket_path).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;

    let request = serde_json::json!({ "texts": texts });
    let request_bytes = serde_json::to_vec(&request).ok()?;
    write_message(&mut stream, &request_bytes).ok()?;

    let response_data = read_message(&mut stream).ok()?;
    let response: serde_json::Value = serde_json::from_slice(&response_data).ok()?;

    let embeddings = response.get("embeddings")?.as_array()?;
    let result: Vec<Vec<f32>> = embeddings
        .iter()
        .filter_map(|row| {
            row.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect()
            })
        })
        .collect();

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    // ─── mock-boundary tests (model-free) ───────────────────────────

    /// A model stand-in: returns an all-ones hidden-state tensor shaped to the
    /// input so `encode_impl`/`mean_pooling` run without a real ModernBert.
    struct MockModel {
        hidden: usize,
    }

    impl EmbeddingModel for MockModel {
        fn forward(&self, input_ids: &Tensor, _attention_mask: &Tensor) -> Result<Tensor> {
            let (batch, seq) = input_ids.dims2()?;
            Tensor::ones((batch, seq, self.hidden), DType::F32, input_ids.device())
                .map_err(Into::into)
        }
    }

    /// A tiny whitespace tokenizer built in code (no model files).
    fn tiny_tokenizer() -> Tokenizer {
        use tokenizers::models::wordlevel::WordLevel;
        use tokenizers::pre_tokenizers::whitespace::Whitespace;
        use tokenizers::{PaddingParams, PaddingStrategy};

        let wl = WordLevel::builder()
            .vocab(
                ["[PAD]", "[UNK]", "hello", "world", "foo"]
                    .into_iter()
                    .enumerate()
                    .map(|(i, w)| (w.to_string(), i as u32))
                    .collect(),
            )
            .unk_token("[UNK]".to_string())
            .build()
            .unwrap();
        let mut tk = Tokenizer::new(wl);
        tk.with_pre_tokenizer(Some(Whitespace {}));
        tk.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));
        tk
    }

    #[test]
    fn test_encode_impl_produces_one_vector_per_text() {
        let model = MockModel { hidden: 4 };
        let tok = tiny_tokenizer();
        let texts = vec!["hello world".to_string(), "foo".to_string()];

        let out = encode_impl(&model, &tok, &Device::Cpu, &texts).unwrap();

        assert_eq!(out.len(), 2, "one embedding per input text");
        assert_eq!(out[0].len(), 4, "embedding dim follows the model output");
        // Mean-pooled all-ones rows are L2-normalized → unit norm.
        let norm: f32 = out[0].iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "rows are L2-normalized, got {norm}"
        );
    }

    #[test]
    fn test_encode_impl_empty_input_returns_empty() {
        let model = MockModel { hidden: 4 };
        let tok = tiny_tokenizer();
        let out = encode_impl(&model, &tok, &Device::Cpu, &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_embedder_holder_encode_and_dim() {
        // Exercises the holder methods (from_parts / encode / dim) so they are
        // not left uncovered when tests call encode_impl directly.
        let embedder = Embedder::from_parts(
            Box::new(MockModel { hidden: 4 }),
            tiny_tokenizer(),
            Device::Cpu,
        );
        let out = embedder.encode(&["hello world".to_string()]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(embedder.dim(), config::EMBEDDING_DIM);
    }

    #[test]
    fn test_truncate_texts_caps_long_input_at_char_boundary() {
        let long = "あ".repeat(10_000); // 30_000 bytes, well over MAX_CHARS
        let out = truncate_texts(&[long.clone(), "short".to_string()]);
        assert!(out[0].len() <= 8192);
        assert!(long.starts_with(&out[0]), "truncation keeps a valid prefix");
        assert_eq!(out[1], "short", "short text is untouched");
    }

    #[test]
    fn test_mean_pooling_masks_padding_and_normalizes() {
        // batch=1, seq=2, hidden=2. Second token is padding (mask=0): only the
        // first token contributes, so the pooled vector is that token's row,
        // L2-normalized.
        let output =
            Tensor::from_vec(vec![3.0f32, 4.0, 100.0, 100.0], (1, 2, 2), &Device::Cpu).unwrap();
        let mask = Tensor::from_vec(vec![1u32, 0u32], (1, 2), &Device::Cpu).unwrap();

        let pooled = mean_pooling(&output, &mask).unwrap();
        let v: Vec<f32> = pooled.get(0).unwrap().to_vec1().unwrap();

        // (3,4) normalized → (0.6, 0.8); padding row ignored despite huge values.
        assert!((v[0] - 0.6).abs() < 1e-5, "got {v:?}");
        assert!((v[1] - 0.8).abs() < 1e-5, "got {v:?}");
    }

    #[test]
    fn test_read_write_message_roundtrip() {
        let original = b"hello world";
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_read_write_message_empty() {
        let original = b"";
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original.to_vec());
    }

    #[test]
    fn test_read_write_message_large() {
        let original: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let mut buf = Vec::new();
        write_message(&mut buf, &original).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_message_format_is_big_endian_length_prefix() {
        let data = b"test";
        let mut buf = Vec::new();
        write_message(&mut buf, data).unwrap();

        assert_eq!(&buf[0..4], &[0, 0, 0, 4]); // 4 bytes, big-endian
        assert_eq!(&buf[4..], b"test");
    }

    #[test]
    #[cfg_attr(feature = "bench-counters", serial_test::serial(embedder_counter))]
    fn test_embed_via_socket_nonexistent_path() {
        let result = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        assert!(result.is_none());
    }

    #[cfg(feature = "bench-counters")]
    #[test]
    #[serial_test::serial(embedder_counter)]
    fn test_embedder_call_counter_increments() {
        counters::reset_embedder_calls();
        let _ = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        assert_eq!(counters::embedder_call_count(), 1);
    }

    #[cfg(feature = "bench-counters")]
    #[test]
    #[serial_test::serial(embedder_counter)]
    fn test_embedder_call_counter_accumulates() {
        // Each call bumps the counter by exactly one; guards against an
        // off-by-N regression that the count==1 test alone would miss.
        counters::reset_embedder_calls();
        let _ = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        let _ = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        assert_eq!(counters::embedder_call_count(), 2);
    }

    #[cfg(feature = "bench-counters")]
    #[test]
    #[serial_test::serial(embedder_counter)]
    fn test_embedder_call_counter_reset_zeroes() {
        // Dirty the counter first, otherwise this only asserts that a freshly
        // initialized (or already-reset) counter reads zero — a no-op `reset`
        // would still pass. Reset must actually clear a non-zero value.
        let _ = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        assert_ne!(counters::embedder_call_count(), 0);
        counters::reset_embedder_calls();
        assert_eq!(counters::embedder_call_count(), 0);
    }

    #[test]
    #[cfg_attr(feature = "bench-counters", serial_test::serial(embedder_counter))]
    fn test_embed_via_socket_integration() {
        // Start a mock server that echoes fixed embeddings
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");

        let sock_path_clone = sock_path.clone();
        let server = std::thread::spawn(move || {
            let listener = UnixListener::bind(&sock_path_clone).unwrap();
            let (mut stream, _) = listener.accept().unwrap();

            // Read request
            let req_data = read_message(&mut stream).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&req_data).unwrap();
            let n = req["texts"].as_array().unwrap().len();

            // Send back fake embeddings (3-dim for simplicity)
            let embeddings: Vec<Vec<f32>> = (0..n).map(|_| vec![0.1, 0.2, 0.3]).collect();
            let response = serde_json::json!({ "embeddings": embeddings });
            let resp_bytes = serde_json::to_vec(&response).unwrap();
            write_message(&mut stream, &resp_bytes).unwrap();
        });

        // Wait for server to be ready
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let texts = vec!["hello".to_string(), "world".to_string()];
        let result = embed_via_socket_at(&sock_path, &texts);
        assert!(result.is_some());
        let embeddings = result.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0], vec![0.1, 0.2, 0.3]);

        server.join().unwrap();
    }
}
