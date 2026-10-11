//! Shared provider selection and embedding-space metadata for all entry points.

use std::{path::PathBuf, sync::Arc};

use alexandria_storage::models::embedding::{EmbeddingSpace, GRANITE_MODEL_ID};
use anyhow::{Context, Result, ensure};

use super::{
    CandleProvider, DEFAULT_MAX_TOKENS, EmbeddingProvider,
    granite::{GraniteDevice, GraniteOptions, GraniteProvider, artifacts::resolve_artifacts},
};

/// Explicit configuration after file/environment precedence and budget resolution.
/// The default remains legacy MiniLM until release, license and calibration gates pass.
pub struct ProviderOptions {
    pub model: String,
    pub device: String,
    pub max_tokens: usize,
    pub artifact_dir: Option<PathBuf>,
}

impl Default for ProviderOptions {
    fn default() -> Self {
        Self {
            model: "sentence-transformers/all-MiniLM-L6-v2".into(),
            device: "cpu".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            artifact_dir: None,
        }
    }
}

/// Ready provider and the exact device-independent space its vectors inhabit.
pub struct LoadedEmbedding {
    pub provider: Arc<dyn EmbeddingProvider>,
    pub space: EmbeddingSpace,
    pub effective_max_tokens: usize,
}

pub fn default_max_tokens(model: &str) -> usize {
    if model == GRANITE_MODEL_ID {
        8192
    } else {
        DEFAULT_MAX_TOKENS
    }
}

pub fn validate_max_tokens(model: &str, max_tokens: usize) -> Result<()> {
    let (minimum, maximum) = if model == GRANITE_MODEL_ID {
        (2, 8192)
    } else {
        (3, 512)
    };
    ensure!(
        (minimum..=maximum).contains(&max_tokens),
        "embedding.max_tokens must be between {minimum} and {maximum}"
    );
    Ok(())
}

/// GPU aliases without an ordinal select zero; ordinals are unsigned decimal.
/// Availability and compiled-backend checks are deliberately separate from syntax.
pub fn parse_granite_device(selector: &str) -> Result<GraniteDevice> {
    if selector == "cpu" {
        return Ok(GraniteDevice::Cpu);
    }
    let (backend, ordinal) = selector.split_once(':').unwrap_or((selector, "0"));
    ensure!(
        !ordinal.is_empty() && ordinal.bytes().all(|c| c.is_ascii_digit()),
        "Invalid embedding device selector: {selector}"
    );
    let index = ordinal
        .parse::<usize>()
        .with_context(|| format!("Invalid embedding device selector: {selector}"))?;
    match backend {
        "vulkan" => Ok(GraniteDevice::Vulkan { index }),
        "rocm" => Ok(GraniteDevice::Rocm { index }),
        _ => anyhow::bail!(
            "Invalid embedding device selector: {selector}; use cpu, vulkan[:index] or rocm[:index]"
        ),
    }
}

pub async fn load_provider(options: ProviderOptions) -> Result<LoadedEmbedding> {
    validate_max_tokens(&options.model, options.max_tokens)?;
    ensure!(
        !options.model.trim().is_empty(),
        "embedding.model must not be blank"
    );
    if options.model == GRANITE_MODEL_ID {
        return load_granite(options).await;
    }
    ensure!(
        options.device == "cpu",
        "Legacy Candle supports only cpu; requested {}",
        options.device
    );
    let provider = CandleProvider::new(&options.model, &options.device, options.max_tokens).await?;
    let space = EmbeddingSpace::Legacy {
        model: provider.model_id().to_owned(),
        dimensions: provider.dimensions(),
        max_tokens: provider.max_tokens(),
    };
    space.validate()?;
    Ok(LoadedEmbedding {
        effective_max_tokens: provider.max_tokens(),
        provider: Arc::new(provider),
        space,
    })
}

async fn load_granite(options: ProviderOptions) -> Result<LoadedEmbedding> {
    let device = parse_granite_device(&options.device)?;
    device.check_compiled()?;
    let artifacts = resolve_artifacts(options.artifact_dir.as_deref()).await?;
    let mut space = artifacts.space.clone();
    let EmbeddingSpace::GraniteV1(identity) = &mut space else {
        anyhow::bail!("Granite resolver returned a non-Granite identity");
    };
    identity.max_tokens = options.max_tokens;
    space.validate()?;
    let provider = GraniteProvider::new(GraniteOptions {
        device,
        max_tokens: options.max_tokens,
        artifacts,
    })
    .await?;
    Ok(LoadedEmbedding {
        provider: Arc::new(provider),
        space,
        effective_max_tokens: options.max_tokens,
    })
}
