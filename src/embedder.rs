use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor, D};
use candle_nn::VarBuilder;
use candle_transformers::models::modernbert::{Config, ModernBert};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer};

use crate::config;
use crate::ipc::{read_message, write_message};

const MODEL_ID: &str = "cl-nagoya/ruri-v3-30m";

/// The embedder engine: loads model and produces embeddings.
pub struct Embedder {
    model: ModernBert,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Load the ruri-v3-30m model from HuggingFace Hub cache.
    pub fn load(device: &Device) -> Result<Self> {
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.repo(hf_hub::Repo::new(
            MODEL_ID.to_string(),
            hf_hub::RepoType::Model,
        ));

        let config_path = repo.get("config.json").context("config.json not found")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("tokenizer.json not found")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("model.safetensors not found")?;

        Self::load_from_paths(&config_path, &tokenizer_path, &weights_path, device)
    }

    /// Load model from explicit file paths.
    pub fn load_from_paths(
        config_path: &Path,
        tokenizer_path: &Path,
        weights_path: &Path,
        device: &Device,
    ) -> Result<Self> {
        let config_str = std::fs::read_to_string(config_path)?;
        let config: Config = serde_json::from_str(&config_str)?;
        let pad_token_id = config.pad_token_id;

        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: pad_token_id,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));

        let tensors = load_tensors_with_prefix(weights_path, device)?;
        let vb = VarBuilder::from_tensors(tensors, DType::F32, device);
        let model = ModernBert::load(vb, &config)?;

        Ok(Self {
            model,
            tokenizer,
            device: device.clone(),
        })
    }

    /// Encode texts into embedding vectors.
    pub fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Truncate texts to avoid OOM/crash on very long inputs.
        // ruri-v3-30m supports up to 8192 tokens; we limit input chars
        // conservatively (Japanese averages ~1.5 chars/token).
        const MAX_CHARS: usize = 8192;
        let truncated: Vec<String> = texts
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
            .collect();

        let encodings = self
            .tokenizer
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
            Tensor::from_vec(input_ids_flat, (batch_size, seq_len), &self.device)?
                .to_dtype(DType::U32)?;
        let attention_mask_tensor =
            Tensor::from_vec(mask_flat, (batch_size, seq_len), &self.device)?
                .to_dtype(DType::U32)?;

        let output = self
            .model
            .forward(&input_ids_tensor, &attention_mask_tensor)?;
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

    /// Get the embedding dimension.
    pub fn dim(&self) -> usize {
        config::EMBEDDING_DIM
    }
}

/// Load safetensors with "model." prefix added to tensor names.
/// Uses mmap to leverage OS page cache for faster repeated loads.
fn load_tensors_with_prefix(path: &Path, device: &Device) -> Result<HashMap<String, Tensor>> {
    let mmaped = unsafe { candle_core::safetensors::MmapedSafetensors::new(path)? };
    let mut renamed = HashMap::new();
    for (name, _view) in mmaped.tensors() {
        let tensor = mmaped.load(&name, device)?;
        renamed.insert(format!("model.{name}"), tensor);
    }
    Ok(renamed)
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
    fn test_embed_via_socket_nonexistent_path() {
        let result = embed_via_socket_at(Path::new("/tmp/nonexistent.sock"), &[]);
        assert!(result.is_none());
    }

    #[test]
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
