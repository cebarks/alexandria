//! Native FP32 Granite inference with explicit device selection and bounded
//! serial execution. Artifact publication and tokenizer licensing are separate
//! gates; ordinary Cargo builds do not download or convert model assets.

pub mod artifacts;
pub mod model;
pub mod worker;

use super::EmbeddingProvider;
use anyhow::{Context, Result, ensure};
use artifacts::ResolvedArtifacts;
use async_trait::async_trait;
use worker::Worker;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraniteDevice {
    Cpu,
    Vulkan { index: usize },
    Rocm { index: usize },
}

impl GraniteDevice {
    pub(crate) fn check_compiled(self) -> Result<()> {
        ensure!(
            !matches!(self, Self::Rocm { .. }) || cfg!(feature = "granite-rocm"),
            "Requested ROCm backend is not compiled; enable granite-rocm"
        );
        ensure!(
            !matches!(self, Self::Vulkan { .. }) || cfg!(feature = "granite-vulkan"),
            "Requested Vulkan backend is not compiled; enable granite-vulkan"
        );
        Ok(())
    }
}

/// Checksum-verified artifacts and effective text budget (2..=8192).
/// GPU requests require their Cargo feature and an available matching device.
pub struct GraniteOptions {
    pub device: GraniteDevice,
    pub max_tokens: usize,
    pub artifacts: ResolvedArtifacts,
}

pub struct GraniteProvider {
    worker: Worker,
    overflow_tokenizer: tokenizers::Tokenizer,
    max_tokens: usize,
}

impl GraniteProvider {
    /// Readiness includes model loading and validation/warmup on the owning thread.
    /// CPU validates a short input; GPU warms32/64/128/256/512/1024-token shapes.
    /// Serving uses exact lengths, so unseen shapes can remain cold. Native aborts
    /// are not recoverable through Rust channels; device preflight precedes the
    /// Burn-store weight-import window where a panic would abort the process.
    pub async fn new(mut options: GraniteOptions) -> Result<Self> {
        ensure!(
            (2..=8192).contains(&options.max_tokens),
            "Granite token budget must be 2..=8192"
        );
        options.device.check_compiled()?;
        let alexandria_storage::models::embedding::EmbeddingSpace::GraniteV1(space) =
            &mut options.artifacts.space
        else {
            anyhow::bail!("Native provider requires a Granite artifact identity");
        };
        space.max_tokens = options.max_tokens;
        options.artifacts.space.validate()?;
        let path = options.artifacts.tokenizer_path.clone();
        let tokenizer = tokio::task::spawn_blocking(move || -> Result<tokenizers::Tokenizer> {
            let mut tokenizer = tokenizers::Tokenizer::from_file(path)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            tokenizer.with_padding(None);
            tokenizer
                .with_truncation(None)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(tokenizer)
        })
        .await??;
        let overflow_tokenizer = tokenizer.clone();
        let max_tokens = options.max_tokens;
        let requested = options.device;
        let worker = Worker::start(move || {
            let encoder = model::Encoder::load(options, tokenizer)?;
            Ok(move |text: &str| encoder.embed(text))
        })
        .await
        .with_context(|| format!("initialize Granite on {requested:?}"))?;
        Ok(Self {
            worker,
            overflow_tokenizer,
            max_tokens,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for GraniteProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.worker.embed(texts).await
    }

    fn dimensions(&self) -> usize {
        768
    }

    fn model_id(&self) -> &str {
        alexandria_storage::models::embedding::GRANITE_MODEL_ID
    }

    fn overflow(&self, text: &str) -> Option<usize> {
        self.overflow_tokenizer
            .encode(text, true)
            .ok()
            .map(|encoded| encoded.len())
            .filter(|&length| length > self.max_tokens)
    }
}
