use super::{GraniteDevice, GraniteOptions};
use anyhow::{Context, Result, ensure};
use burn::tensor::{DType, Device, Int, Tensor, TensorData};
use sha2::{Digest, Sha256};

// Keep reviewed importer output byte-for-byte; generated diagnostics are not
// first-party lint findings. The enclosing crate still forbids unsafe code.
#[allow(warnings)]
#[rustfmt::skip]
#[path = "generated.rs"]
mod generated;

pub(super) struct Encoder {
    model: generated::Model,
    device: Device,
    tokenizer: tokenizers::Tokenizer,
}

impl Encoder {
    pub(super) fn load(options: GraniteOptions, tokenizer: tokenizers::Tokenizer) -> Result<Self> {
        let manifest =
            super::artifacts::ArtifactManifest::parse_json(include_str!("artifact-manifest.json"))?;
        ensure!(
            format!("{:x}", Sha256::digest(include_bytes!("generated.rs")))
                == manifest.generated_model_sha256,
            "Compiled generated model does not match artifact provenance"
        );
        let device = select_device(options.device)?;
        // Burn-store aborts on a panic during ModuleSnapshot::apply. Open the
        // selected driver before entering that moved-from-module window.
        device.sync().context("preflight selected Granite device")?;
        let load_start = std::time::Instant::now();
        let path = options
            .artifacts
            .weights_path
            .to_str()
            .context("weights path must be UTF-8")?;
        let model = generated::Model::from_file(path, &device);
        device.sync()?;
        let load_ms = load_start.elapsed().as_millis();
        let encoder = Self {
            model,
            device,
            tokenizer: configure_tokenizer(tokenizer, options.max_tokens)?,
        };
        let warm_start = std::time::Instant::now();
        let lengths: &[usize] = match options.device {
            GraniteDevice::Cpu => &[2],
            _ => &[32, 64, 128, 256, 512, 1024],
        };
        for &length in lengths {
            encoder.forward(vec![2_i64; length], vec![1_i64; length])?;
        }
        tracing::info!(device = ?options.device, dimensions = 768, max_tokens = options.max_tokens,
            load_ms, warmup_ms = warm_start.elapsed().as_millis(), dtype = "f32",
            model = %options.artifacts.space.model(), identity = ?options.artifacts.space,
            backend = ?encoder.device, "Granite worker ready");
        Ok(encoder)
    }

    pub(super) fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoded = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.forward(
            encoded.get_ids().iter().map(|&id| i64::from(id)).collect(),
            encoded
                .get_attention_mask()
                .iter()
                .map(|&mask| i64::from(mask))
                .collect(),
        )
    }

    fn forward(&self, ids: Vec<i64>, mask: Vec<i64>) -> Result<Vec<f32>> {
        let length = ids.len();
        ensure!(
            (1..=8192).contains(&length) && mask.len() == length,
            "Invalid Granite input shape"
        );
        let ids = Tensor::<2, Int>::from_data(
            TensorData::new(ids, [1, length]),
            (&self.device, DType::I64),
        );
        let mask = Tensor::<2, Int>::from_data(
            TensorData::new(mask, [1, length]),
            (&self.device, DType::I64),
        );
        let output = self.model.forward(ids, mask);
        ensure!(
            output.dtype() == DType::F32 && output.dims() == [1, length, 768],
            "Unexpected Granite output dtype/shape"
        );
        let cls = output
            .slice([0..1, 0..1, 0..768])
            .reshape([768])
            .try_to_data()?;
        normalize_cls(cls.try_into_vec::<f32>()?)
    }
}

fn select_device(requested: GraniteDevice) -> Result<Device> {
    match requested {
        GraniteDevice::Cpu => Ok(Device::flex()),
        #[cfg(feature = "granite-vulkan")]
        GraniteDevice::Vulkan { index } => {
            let device = Device::vulkan(burn::tensor::DeviceKind::DiscreteGpu(index));
            ensure!(
                Device::enumerate(burn::tensor::DeviceType::Vulkan).contains(&device),
                "Requested GPU is unavailable: {requested:?}"
            );
            Ok(device)
        }
        #[cfg(feature = "granite-rocm")]
        GraniteDevice::Rocm { index } => {
            let device = Device::rocm(index);
            ensure!(
                Device::enumerate(burn::tensor::DeviceType::Rocm).contains(&device),
                "Requested GPU is unavailable: {requested:?}"
            );
            Ok(device)
        }
        #[cfg(not(feature = "granite-vulkan"))]
        GraniteDevice::Vulkan { .. } => anyhow::bail!("Vulkan backend not compiled"),
        #[cfg(not(feature = "granite-rocm"))]
        GraniteDevice::Rocm { .. } => anyhow::bail!("ROCm backend not compiled"),
    }
}

/// Text budgets include special tokens; empty text may still encode to one token.
pub fn configure_tokenizer(
    mut tokenizer: tokenizers::Tokenizer,
    max_tokens: usize,
) -> Result<tokenizers::Tokenizer> {
    ensure!(
        (2..=8192).contains(&max_tokens),
        "Granite token budget must be 2..=8192"
    );
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: max_tokens,
            direction: tokenizers::TruncationDirection::Right,
            ..Default::default()
        }))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(tokenizer)
}

/// Native FP32 CLS, with FP64 norm accumulation and final conversion to FP32.
pub fn normalize_cls(mut values: Vec<f32>) -> Result<Vec<f32>> {
    ensure!(values.len() == 768, "Granite CLS must have 768 dimensions");
    ensure!(
        values.iter().all(|value| value.is_finite()),
        "Nonfinite Granite CLS"
    );
    let norm = values
        .iter()
        .map(|&v| f64::from(v).powi(2))
        .sum::<f64>()
        .sqrt();
    ensure!(norm.is_finite() && norm > 0.0, "Invalid Granite CLS norm");
    for value in &mut values {
        *value = (f64::from(*value) / norm) as f32;
    }
    ensure!(
        values.iter().all(|value| value.is_finite()),
        "Nonfinite normalized Granite CLS"
    );
    Ok(values)
}
