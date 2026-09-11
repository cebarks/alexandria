use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::Tokenizer;

use super::hub;
use super::provider::EmbeddingProvider;

/// sentence-transformers models ship `1_Pooling/config.json`. Only the CLS flag
/// matters to us; anything else (missing file, missing key, bad JSON) means mean pooling.
fn cls_pooling_from_json(s: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| v.get("pooling_mode_cls_token")?.as_bool())
        .unwrap_or(false)
}

pub struct CandleProvider {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    model_id: String,
    dimensions: usize,
    cls_pooling: bool,
}

impl CandleProvider {
    pub async fn new(model_id: &str, device_str: &str) -> Result<Self> {
        // Cache first, network only on a miss (see hub.rs).
        let required = async |file: &str| {
            hub::fetch(model_id, file)
                .await
                .and_then(|p| p.with_context(|| format!("{file} not found in {model_id}")))
                .with_context(|| format!("Failed to download {file}"))
        };
        let config_path = required("config.json").await?;
        let tokenizer_path = required("tokenizer.json").await?;
        let weights_path = required("model.safetensors").await?;

        // Optional: pooling config. Not every repo has it; absence means mean pooling.
        let cls_pooling = match hub::fetch(model_id, "1_Pooling/config.json").await {
            Ok(Some(p)) => std::fs::read_to_string(p)
                .map(|s| cls_pooling_from_json(&s))
                .unwrap_or(false),
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    "could not load 1_Pooling/config.json for {model_id} ({e}); assuming mean pooling"
                );
                false
            }
        };

        let device_str_owned = device_str.to_string();
        // Model loading is CPU-bound, run in blocking task
        let (model, tokenizer, device, dimensions) = tokio::task::spawn_blocking(move || {
            Self::load_model(
                &config_path,
                &tokenizer_path,
                &weights_path,
                &device_str_owned,
            )
        })
        .await??;

        tracing::info!("Pooling: {}", if cls_pooling { "cls" } else { "mean" });

        Ok(Self {
            model,
            tokenizer,
            device,
            model_id: model_id.to_string(),
            dimensions,
            cls_pooling,
        })
    }

    /// Test hook: override the pooling mode read from `1_Pooling/config.json`.
    /// Lets `tests/embedding_test.rs` prove the CLS branch runs without a second model download.
    #[doc(hidden)]
    pub fn set_cls_pooling(&mut self, cls: bool) {
        self.cls_pooling = cls;
    }

    fn load_model(
        config_path: &Path,
        tokenizer_path: &Path,
        weights_path: &Path,
        device_str: &str,
    ) -> Result<(BertModel, Tokenizer, Device, usize)> {
        let device = match device_str {
            "cpu" => Device::Cpu,
            _ => Device::Cpu, // fallback to CPU
        };

        let config_str = std::fs::read_to_string(config_path)?;
        let config: BertConfig = serde_json::from_str(&config_str)?;
        let dimensions = config.hidden_size;

        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;

        // Read into a Vec rather than mmap so the workspace can keep `unsafe_code = "forbid"`.
        // Costs a ~90 MB peak (buffer + built tensors) until `BertModel::load` returns; mmap
        // would not remove it, candle copies each tensor out anyway. Revisit with
        // `#[allow(unsafe_code)]` + `from_mmaped_safetensors` only for a much larger model.
        let vb = VarBuilder::from_buffered_safetensors(
            std::fs::read(weights_path)?,
            candle_core::DType::F32,
            &device,
        )?;
        let model = BertModel::load(vb, &config)?;

        Ok((model, tokenizer, device, dimensions))
    }

    fn embed_sync(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut all_embeddings = Vec::with_capacity(texts.len());

        for text in texts {
            let encoding = self
                .tokenizer
                .encode(*text, true)
                .map_err(|e| anyhow::anyhow!("Tokenization failed: {e}"))?;

            let input_ids = encoding.get_ids().to_vec();
            let attention_mask = encoding.get_attention_mask().to_vec();
            let token_type_ids = encoding.get_type_ids().to_vec();
            let len = input_ids.len();

            let input_ids = Tensor::new(input_ids.as_slice(), &self.device)?.reshape((1, len))?;
            let attention_mask =
                Tensor::new(attention_mask.as_slice(), &self.device)?.reshape((1, len))?;
            let token_type_ids =
                Tensor::new(token_type_ids.as_slice(), &self.device)?.reshape((1, len))?;

            let output = self
                .model
                .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

            let pooled = if self.cls_pooling {
                // CLS pooling: hidden state of the first token. Shape (1, hidden).
                output.narrow(1, 0, 1)?.squeeze(1)?
            } else {
                // Mean pooling over sequence length (dim 1), respecting attention mask
                let mask = attention_mask
                    .unsqueeze(2)?
                    .to_dtype(candle_core::DType::F32)?
                    .broadcast_as(output.shape())?;
                let masked = (output * mask)?;
                let summed = masked.sum(1)?;
                let counts = attention_mask
                    .to_dtype(candle_core::DType::F32)?
                    .sum(1)?
                    .unsqueeze(1)?
                    .broadcast_as(summed.shape())?;
                (summed / counts)?
            };

            // L2 normalize
            let norm = pooled
                .sqr()?
                .sum(1)?
                .sqrt()?
                .unsqueeze(1)?
                .broadcast_as(pooled.shape())?;
            let normalized = (pooled / norm)?;

            let embedding: Vec<f32> = normalized.squeeze(0)?.to_vec1()?;
            all_embeddings.push(embedding);
        }

        Ok(all_embeddings)
    }
}

#[async_trait]
impl EmbeddingProvider for CandleProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // Candle inference is CPU-bound but not easily movable to spawn_blocking
        // because &self borrows prevent Send. Run inline for now.
        self.embed_sync(texts)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

#[cfg(test)]
mod tests {
    use super::cls_pooling_from_json;

    #[test]
    fn cls_true_when_flag_set() {
        let json = r#"{"word_embedding_dimension": 384, "pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false}"#;
        assert!(cls_pooling_from_json(json));
    }

    #[test]
    fn mean_when_flag_false() {
        let json = r#"{"word_embedding_dimension": 384, "pooling_mode_cls_token": false, "pooling_mode_mean_tokens": true}"#;
        assert!(!cls_pooling_from_json(json));
    }

    #[test]
    fn mean_when_key_missing_or_unparseable() {
        assert!(!cls_pooling_from_json(
            r#"{"pooling_mode_mean_tokens": true}"#
        ));
        assert!(!cls_pooling_from_json("not json"));
    }
}
