//! Real ruri-v3-30m model wrapper: file loading + the candle/ModernBert forward
//! pass behind the [`EmbeddingModel`] trait. This is the irreducible
//! model-coupled shell (needs real model files + candle inference), kept out of
//! the coverage gate; the testable encode pipeline lives in `embedder.rs`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::modernbert::{Config, ModernBert};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::config;
use crate::embedder::{Embedder, EmbeddingModel};

/// Hard cap on tokens per input. candle's ModernBert materializes the full
/// (batch, heads, seq, seq) attention matrix, so memory grows with seq².
/// Chunks target ~800 chars (see config::MAX_CHUNK_CHARS); 2048 tokens is
/// ample headroom. The batch dimension is bounded by callers in the indexer
/// (BACKFILL_BATCH_SIZE = 8), so a full-size request stays around ~0.5GB
/// of attention memory.
const MAX_TOKENS: usize = 2048;

/// Apply padding and truncation settings shared by all load paths.
fn configure_tokenizer(tokenizer: &mut Tokenizer, pad_token_id: u32) -> Result<()> {
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        pad_id: pad_token_id,
        pad_token: "[PAD]".to_string(),
        ..Default::default()
    }));
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: MAX_TOKENS,
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// The real ModernBert-backed model. Thin wrapper: forwards to candle.
pub struct RuriModel(ModernBert);

impl EmbeddingModel for RuriModel {
    fn forward(&self, input_ids: &Tensor, attention_mask: &Tensor) -> Result<Tensor> {
        Ok(self.0.forward(input_ids, attention_mask)?)
    }
}

impl Embedder {
    /// Load the ruri-v3-30m model.
    ///
    /// Resolution order:
    /// 1. `{state_dir}/models/ruri-v3-30m/` — workspace override
    ///    (lets users drop a fine-tuned model into a single workspace)
    /// 2. `{cache_dir}/models/ruri-v3-30m/` — machine-wide cache
    ///    populated by `tsm setup`
    /// 3. Neither populated → fail with an actionable error message
    pub fn load(device: &Device) -> Result<Self> {
        if let Some(dir) = config::models_dir_complete() {
            log::info!("Loading model from workspace override: {}", dir.display());
            return Self::load_from_dir(&dir, device);
        }

        if let Some(dir) = config::cache_models_dir_complete() {
            log::info!("Loading model from cache: {}", dir.display());
            return Self::load_from_dir(&dir, device);
        }

        anyhow::bail!(
            "ruri-v3-30m model not found. Run `tsm setup` to populate the cache, \
             or place the model files at {} (workspace override).",
            config::models_dir().display()
        );
    }

    /// Helper: load all three required files from a single directory.
    fn load_from_dir(dir: &Path, device: &Device) -> Result<Self> {
        Self::load_from_paths(
            &dir.join("config.json"),
            &dir.join("tokenizer.json"),
            &dir.join("model.safetensors"),
            device,
        )
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
        configure_tokenizer(&mut tokenizer, pad_token_id)?;

        let tensors = load_tensors_with_prefix(weights_path, device)?;
        let vb = VarBuilder::from_tensors(tensors, DType::F32, device);
        let model = ModernBert::load(vb, &config)?;

        Ok(Self::from_parts(
            Box::new(RuriModel(model)),
            tokenizer,
            device.clone(),
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configure_tokenizer_truncates_and_pads() {
        // Minimal word-level tokenizer built from inline JSON (no model files)
        let tok_json = r#"{
            "version": "1.0",
            "model": {
                "type": "WordLevel",
                "vocab": {"[PAD]": 0, "hello": 1},
                "unk_token": "[PAD]"
            },
            "pre_tokenizer": {"type": "Whitespace"}
        }"#;
        let mut tokenizer = Tokenizer::from_bytes(tok_json.as_bytes()).unwrap();
        configure_tokenizer(&mut tokenizer, 0).unwrap();

        let long_text = "hello ".repeat(3000); // 3000 tokens before truncation
        let encodings = tokenizer
            .encode_batch(vec![long_text, "hello".to_string()], true)
            .unwrap();

        // Long input truncated to MAX_TOKENS; short input padded to batch longest
        assert_eq!(encodings[0].get_ids().len(), MAX_TOKENS);
        assert_eq!(encodings[1].get_ids().len(), MAX_TOKENS);
    }

    #[test]
    #[serial_test::serial]
    fn test_embedder_load_fails_with_helpful_message_when_no_model() {
        // Both workspace override and cache are empty — load() must bail with
        // an actionable message that points the user at `tsm setup`.
        let tmp_state = tempfile::TempDir::new().unwrap();
        let tmp_cache = tempfile::TempDir::new().unwrap();

        std::env::set_var("TSM_STATE_DIR", tmp_state.path());
        std::env::set_var("TSM_CACHE_DIR", tmp_cache.path());
        config::reload();

        let result = Embedder::load(&Device::Cpu);

        std::env::remove_var("TSM_STATE_DIR");
        std::env::remove_var("TSM_CACHE_DIR");
        config::reload();

        // Embedder doesn't implement Debug; use match instead of unwrap_err.
        let err = match result {
            Ok(_) => panic!("Embedder::load unexpectedly succeeded with empty dirs"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("tsm setup"),
            "expected error to mention `tsm setup`, got: {msg}"
        );
        assert!(
            msg.contains("ruri-v3-30m"),
            "expected error to mention model name, got: {msg}"
        );
    }
}
