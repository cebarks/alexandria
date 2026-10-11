use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_pipeline::embedding::granite::model::{configure_tokenizer, normalize_cls};
use alexandria_pipeline::embedding::granite::{GraniteDevice, GraniteOptions, GraniteProvider};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct PublicCase {
    text: Option<String>,
    repeat_text: Option<String>,
    repeat: Option<usize>,
}

impl PublicCase {
    fn text(&self) -> String {
        self.text.clone().unwrap_or_else(|| {
            self.repeat_text
                .as_deref()
                .unwrap()
                .repeat(self.repeat.unwrap())
        })
    }
}

#[derive(Deserialize)]
struct TokenGolden {
    #[serde(flatten)]
    input: PublicCase,
    max_tokens: usize,
    token_count: usize,
    untruncated_count: usize,
    ids_sha256: String,
    mask_sha256: String,
    first_ids: Vec<u32>,
    last_ids: Vec<u32>,
}

#[derive(Deserialize)]
struct VectorGolden {
    #[serde(flatten)]
    input: PublicCase,
    vector: Vec<f32>,
}

fn artifact_directory() -> std::path::PathBuf {
    std::env::var_os("ALEXANDRIA_GRANITE_TEST_ARTIFACT_DIR")
        .expect("provide verified local artifact directory")
        .into()
}

fn native_device() -> GraniteDevice {
    match std::env::var("ALEXANDRIA_GRANITE_TEST_DEVICE").as_deref() {
        Ok("rocm:0") => GraniteDevice::Rocm { index: 0 },
        Ok("vulkan:0") => GraniteDevice::Vulkan { index: 0 },
        Ok("cpu") | Err(_) => GraniteDevice::Cpu,
        value => panic!("unsupported test device: {value:?}"),
    }
}

fn unavailable_options(device: GraniteDevice, max_tokens: usize) -> GraniteOptions {
    use alexandria_pipeline::embedding::granite::artifacts::{ArtifactManifest, ResolvedArtifacts};
    let manifest = ArtifactManifest::parse_json(include_str!(
        "../src/embedding/granite/artifact-manifest.json"
    ))
    .unwrap();
    GraniteOptions {
        device,
        max_tokens,
        artifacts: ResolvedArtifacts {
            weights_path: "/must-not-load-model.bpk".into(),
            tokenizer_path: "/must-not-load-tokenizer.json".into(),
            space: manifest.space,
        },
    }
}

#[tokio::test]
async fn native_provider_cannot_advertise_a_legacy_artifact_identity() {
    let mut options = unavailable_options(GraniteDevice::Cpu, 256);
    options.artifacts.space = alexandria_storage::models::embedding::EmbeddingSpace::Legacy {
        model: "sentence-transformers/all-MiniLM-L6-v2".into(),
        dimensions: 384,
        max_tokens: 256,
    };
    let result = GraniteProvider::new(options).await;
    let Err(error) = result else {
        panic!("native provider accepted legacy identity")
    };
    assert!(error.to_string().contains("Granite artifact identity"));
}

#[tokio::test]
async fn invalid_provider_budget_refuses_before_artifact_loading() {
    let result = GraniteProvider::new(unavailable_options(GraniteDevice::Cpu, 1)).await;
    let Err(error) = result else {
        panic!("invalid provider budget accepted")
    };
    assert!(error.to_string().contains("2..=8192"));
}

#[cfg(not(feature = "granite-rocm"))]
#[tokio::test]
async fn requested_rocm_cannot_fall_back_when_not_compiled() {
    let result =
        GraniteProvider::new(unavailable_options(GraniteDevice::Rocm { index: 0 }, 256)).await;
    let Err(error) = result else {
        panic!("ROCm request silently accepted without ROCm")
    };
    assert!(error.to_string().contains("enable granite-rocm"));
}

#[cfg(not(feature = "granite-vulkan"))]
#[tokio::test]
async fn requested_vulkan_cannot_fall_back_when_not_compiled() {
    let result =
        GraniteProvider::new(unavailable_options(GraniteDevice::Vulkan { index: 0 }, 256)).await;
    let Err(error) = result else {
        panic!("Vulkan request silently accepted without Vulkan")
    };
    assert!(error.to_string().contains("enable granite-vulkan"));
}

#[tokio::test]
#[ignore = "requires native GPU backend; unavailable ordinal must fail readiness"]
async fn missing_requested_gpu_refuses_readiness() {
    use alexandria_pipeline::embedding::granite::artifacts::resolve_artifacts;
    let artifacts = resolve_artifacts(Some(&artifact_directory()))
        .await
        .unwrap();
    let device = match native_device() {
        GraniteDevice::Rocm { .. } => GraniteDevice::Rocm { index: usize::MAX },
        GraniteDevice::Vulkan { .. } => GraniteDevice::Vulkan { index: usize::MAX },
        GraniteDevice::Cpu => panic!("select a native GPU backend for missing-GPU acceptance"),
    };
    let result = GraniteProvider::new(GraniteOptions {
        device,
        max_tokens: 256,
        artifacts,
    })
    .await;
    let Err(error) = result else {
        panic!("unavailable GPU silently accepted")
    };
    assert!(
        format!("{error:#}").contains("Requested GPU is unavailable"),
        "device admission must reject unavailable hardware before weight import: {error:#}"
    );
}

fn ids_sha(values: &[u32]) -> String {
    let mut hash = Sha256::new();
    for value in values {
        hash.update(value.to_le_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[tokio::test]
#[ignore = "requires verified local tokenizer; public HF goldens, no inference"]
async fn real_tokenizer_matches_public_hf_goldens() {
    use alexandria_pipeline::embedding::granite::artifacts::resolve_artifacts;
    let artifacts = resolve_artifacts(Some(&artifact_directory()))
        .await
        .unwrap();
    let mut original = tokenizers::Tokenizer::from_file(artifacts.tokenizer_path).unwrap();
    original.with_padding(None);
    original.with_truncation(None).unwrap();
    #[derive(Deserialize)]
    struct Goldens {
        cases: Vec<TokenGolden>,
    }
    let goldens: Goldens =
        serde_json::from_str(include_str!("fixtures/granite-public-tokenizer.json")).unwrap();
    for case in goldens.cases {
        let text = case.input.text();
        assert_eq!(
            original.encode(text.as_str(), true).unwrap().len(),
            case.untruncated_count
        );
        let tokenizer = configure_tokenizer(original.clone(), case.max_tokens).unwrap();
        let encoded = tokenizer.encode(text.as_str(), true).unwrap();
        assert_eq!(encoded.len(), case.token_count);
        assert_eq!(ids_sha(encoded.get_ids()), case.ids_sha256);
        assert_eq!(ids_sha(encoded.get_attention_mask()), case.mask_sha256);
        assert_eq!(&encoded.get_ids()[..case.first_ids.len()], &case.first_ids);
        assert_eq!(
            &encoded.get_ids()[encoded.len() - case.last_ids.len()..],
            &case.last_ids
        );
    }
}

#[tokio::test]
#[ignore = "requires verified model; explicit CPU/GPU hardware acceptance"]
async fn native_provider_matches_independent_public_baseline_vectors() {
    use alexandria_pipeline::embedding::granite::artifacts::resolve_artifacts;
    let artifacts = resolve_artifacts(Some(&artifact_directory()))
        .await
        .unwrap();
    let provider = GraniteProvider::new(GraniteOptions {
        device: native_device(),
        max_tokens: 256,
        artifacts,
    })
    .await
    .unwrap();
    #[derive(Deserialize)]
    struct Goldens {
        cases: Vec<VectorGolden>,
    }
    let goldens: Goldens =
        serde_json::from_str(include_str!("fixtures/granite-public-vectors.json")).unwrap();
    let texts: Vec<_> = goldens.cases.iter().map(|case| case.input.text()).collect();
    let references: Vec<_> = texts.iter().map(String::as_str).collect();
    let results = provider.embed(&references).await.unwrap();
    assert_eq!(provider.dimensions(), 768);
    assert_eq!(
        provider.model_id(),
        "ibm-granite/granite-embedding-311m-multilingual-r2"
    );
    assert_eq!(results.len(), 4);
    for (actual, case) in results.iter().zip(&goldens.cases) {
        assert_eq!(actual.len(), 768);
        assert!(actual.iter().all(|value| value.is_finite()));
        let norm: f64 = actual.iter().map(|&v| f64::from(v).powi(2)).sum();
        assert!((norm - 1.0).abs() < 1.0e-6);
        assert!(
            actual
                .iter()
                .zip(&case.vector)
                .all(|(a, b)| (a - b).abs() < 1.0e-5),
            "native CLS must match the independent normalized baseline"
        );
    }
    assert_eq!(provider.overflow(""), None);
    assert_eq!(provider.overflow("memory"), None);
    assert!(provider.overflow(&texts[3]).is_some());
    assert!(provider.embed(&[]).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "explicit8K capacity job; requires host RAM/VRAM budget and deadline"]
async fn native_provider_accepts_exact_8192_and_reports_overflow() {
    use alexandria_pipeline::embedding::granite::artifacts::resolve_artifacts;
    let artifacts = resolve_artifacts(Some(&artifact_directory()))
        .await
        .unwrap();
    let provider = GraniteProvider::new(GraniteOptions {
        device: native_device(),
        max_tokens: 8192,
        artifacts,
    })
    .await
    .unwrap();
    let text = "configuration memory retrieval ".repeat(10000);
    assert!(provider.overflow(&text).unwrap() > 8192);
    let result = provider.embed(&[&text]).await.unwrap();
    assert_eq!(result[0].len(), 768);
    let norm: f64 = result[0].iter().map(|&v| f64::from(v).powi(2)).sum();
    assert!((norm - 1.0).abs() < 1.0e-6);
}

#[test]
fn cls_is_normalized_with_f64_accumulation() {
    let mut input = vec![0.0; 768];
    input[0] = 3.0;
    input[1] = 4.0;
    let result = normalize_cls(input).unwrap();
    assert_eq!(&result[..2], &[0.6, 0.8]);
    assert_eq!(result.len(), 768);
}

#[test]
fn large_and_tiny_finite_cls_do_not_overflow_or_underflow_norm() {
    for scale in [1.0e30_f32, 1.0e-30_f32] {
        let normalized = normalize_cls(vec![scale; 768]).unwrap();
        let norm: f64 = normalized.iter().map(|&v| f64::from(v).powi(2)).sum();
        assert!((norm - 1.0).abs() < 1.0e-6);
    }
}

#[test]
fn malformed_cls_never_returns_a_vector() {
    for input in [
        vec![1.0; 767],
        vec![1.0; 769],
        vec![0.0; 768],
        vec![f32::NAN; 768],
        vec![f32::INFINITY; 768],
    ] {
        assert!(normalize_cls(input).is_err());
    }
}

fn fixture_tokenizer() -> tokenizers::Tokenizer {
    // A tiny WordLevel tokenizer exercises our truncation/padding configuration,
    // not Gemma tokenization or Granite inference. Public real goldens are separate.
    let json = serde_json::json!({
        "version": "1.0", "truncation": null, "padding": null,
        "added_tokens": [{"id": 2,"content": "<bos>","single_word": false,"lstrip": false,"rstrip": false,"normalized": false,"special": true}],
        "normalizer": null, "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [{"SpecialToken": {"id": "<bos>","type_id": 0}}, {"Sequence": {"id": "A","type_id": 0}}],
            "pair": [{"SpecialToken": {"id": "<bos>","type_id": 0}}, {"Sequence": {"id": "A","type_id": 0}}, {"Sequence": {"id": "B","type_id": 1}}],
            "special_tokens": {"<bos>": {"id": "<bos>","ids": [2],"tokens": ["<bos>"]}}
        },
        "decoder": null,
        "model": {"type": "WordLevel", "vocab": {"<unk>": 0,"hello": 1,"<bos>": 2}, "unk_token": "<unk>"}
    });
    tokenizers::Tokenizer::from_bytes(json.to_string()).unwrap()
}

#[test]
fn right_truncation_preserves_bos_and_exact_short_lengths() {
    let tokenizer = configure_tokenizer(fixture_tokenizer(), 2).unwrap();
    let long = tokenizer.encode("hello hello hello", true).unwrap();
    assert_eq!(long.get_ids(), &[2, 1]);
    assert_eq!(long.get_attention_mask(), &[1, 1]);
    let empty = tokenizer.encode("", true).unwrap();
    assert_eq!(empty.get_ids(), &[2]);
    assert_eq!(empty.get_attention_mask(), &[1]);
}

#[test]
fn invalid_text_budgets_are_refused() {
    for limit in [0, 1, 8193, usize::MAX] {
        assert!(configure_tokenizer(fixture_tokenizer(), limit).is_err());
    }
}

#[test]
fn existing_padding_and_truncation_settings_are_replaced() {
    let mut tokenizer = fixture_tokenizer();
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: 2,
            ..Default::default()
        }))
        .unwrap();
    tokenizer.with_padding(Some(tokenizers::PaddingParams {
        strategy: tokenizers::PaddingStrategy::Fixed(16),
        ..Default::default()
    }));
    let tokenizer = configure_tokenizer(tokenizer, 16).unwrap();
    let encoded = tokenizer.encode("hello hello hello", true).unwrap();
    assert_eq!(encoded.get_ids(), &[2, 1, 1, 1]);
    assert_eq!(encoded.get_attention_mask(), &[1, 1, 1, 1]);
}
