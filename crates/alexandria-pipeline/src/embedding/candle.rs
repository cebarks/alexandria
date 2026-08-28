use anyhow::{Context, Result};
use async_trait::async_trait;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{api::sync::Api, Repo, RepoType};
use tokenizers::Tokenizer;

use super::provider::EmbeddingProvider;

pub struct CandleProvider {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    model_id: String,
    dimensions: usize,
}

impl CandleProvider {
    pub async fn new(model_id: &str, device_str: &str) -> Result<Self> {
        let model_id_owned = model_id.to_string();
        let device_str_owned = device_str.to_string();

        // Model loading is CPU-bound, run in blocking task
        let (model, tokenizer, device, dimensions) = tokio::task::spawn_blocking(move || {
            Self::load_model(&model_id_owned, &device_str_owned)
        })
        .await??;

        Ok(Self {
            model,
            tokenizer,
            device,
            model_id: model_id.to_string(),
            dimensions,
        })
    }

    fn load_model(
        model_id: &str,
        device_str: &str,
    ) -> Result<(BertModel, Tokenizer, Device, usize)> {
        let device = match device_str {
            "cpu" => Device::Cpu,
            _ => Device::Cpu, // fallback to CPU
        };

        let api = Api::new().context("Failed to create HuggingFace Hub API")?;
        let repo = api.repo(Repo::new(model_id.to_string(), RepoType::Model));

        let config_path = repo
            .get("config.json")
            .context("Failed to download config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("Failed to download tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("Failed to download model.safetensors")?;

        let config_str = std::fs::read_to_string(&config_path)?;
        let config: BertConfig = serde_json::from_str(&config_str)?;
        let dimensions = config.hidden_size;

        let tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)?
        };
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
            let mean_pooled = (summed / counts)?;

            // L2 normalize
            let norm = mean_pooled
                .sqr()?
                .sum(1)?
                .sqrt()?
                .unsqueeze(1)?
                .broadcast_as(mean_pooled.shape())?;
            let normalized = (mean_pooled / norm)?;

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
