use alexandria_pipeline::embedding::granite::GraniteDevice;
use alexandria_pipeline::embedding::{
    ProviderOptions, default_max_tokens, load_provider, parse_granite_device, validate_max_tokens,
};
use alexandria_storage::models::embedding::{EmbeddingSpace, GRANITE_MODEL_ID};

#[test]
fn token_policy_preserves_model_specific_boundaries() {
    for (model, minimum, maximum, default) in [
        ("sentence-transformers/all-MiniLM-L6-v2", 3, 512, 256),
        (GRANITE_MODEL_ID, 2, 8192, 8192),
    ] {
        assert_eq!(default_max_tokens(model), default);
        for cap in [minimum, maximum, default] {
            validate_max_tokens(model, cap).expect("supported budget");
        }
        for cap in [0, minimum - 1, maximum + 1] {
            assert!(validate_max_tokens(model, cap).is_err(), "{model}/{cap}");
        }
    }
}

#[test]
fn explicit_selectors_and_bare_gpu_aliases_select_exact_devices() {
    for (selector, expected) in [
        ("cpu", GraniteDevice::Cpu),
        ("vulkan", GraniteDevice::Vulkan { index: 0 }),
        ("vulkan:3", GraniteDevice::Vulkan { index: 3 }),
        ("rocm", GraniteDevice::Rocm { index: 0 }),
        ("rocm:7", GraniteDevice::Rocm { index: 7 }),
    ] {
        assert_eq!(parse_granite_device(selector).unwrap(), expected);
    }
    for selector in [
        "",
        "CPU",
        "cpu:0",
        "cuda",
        "rocm:-1",
        "rocm:+0",
        "vulkan:",
        "vulkan:1:2",
        "rocm:18446744073709551616",
    ] {
        assert!(parse_granite_device(selector).is_err(), "{selector}");
    }
}

#[tokio::test]
async fn invalid_budgets_refuse_before_artifacts_or_model_downloads() {
    for (model, cap) in [
        (GRANITE_MODEL_ID, 8193),
        ("sentence-transformers/all-MiniLM-L6-v2", 2),
    ] {
        let error = load_provider(ProviderOptions {
            model: model.to_owned(),
            max_tokens: cap,
            ..Default::default()
        })
        .await
        .err()
        .expect("invalid budget must refuse");
        assert!(
            error.to_string().contains("embedding.max_tokens"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn legacy_gpu_request_never_silently_falls_back_to_cpu() {
    for selector in ["rocm:0", "vulkan:0", "cuda", "garbage"] {
        let error = load_provider(ProviderOptions {
            device: selector.to_owned(),
            ..Default::default()
        })
        .await
        .err()
        .expect("unsupported legacy device must refuse");
        assert!(
            error
                .to_string()
                .contains("Legacy Candle supports only cpu"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn invalid_granite_selector_refuses_before_artifact_resolution() {
    let error = load_provider(ProviderOptions {
        model: GRANITE_MODEL_ID.to_owned(),
        device: "rocm:bad".to_owned(),
        max_tokens: 8192,
        ..Default::default()
    })
    .await
    .err()
    .expect("invalid selector must refuse");
    assert!(error.to_string().contains("device selector"), "{error:#}");
}

#[cfg(not(feature = "granite-rocm"))]
#[tokio::test]
async fn uncompiled_rocm_refuses_before_artifact_resolution() {
    let error = load_provider(ProviderOptions {
        model: GRANITE_MODEL_ID.to_owned(),
        device: "rocm:0".to_owned(),
        max_tokens: 8192,
        ..Default::default()
    })
    .await
    .err()
    .expect("uncompiled GPU must refuse");
    assert!(
        error.to_string().contains("enable granite-rocm"),
        "{error:#}"
    );
}

#[cfg(not(feature = "granite-vulkan"))]
#[tokio::test]
async fn uncompiled_vulkan_refuses_before_artifact_resolution() {
    let error = load_provider(ProviderOptions {
        model: GRANITE_MODEL_ID.to_owned(),
        device: "vulkan:0".to_owned(),
        max_tokens: 8192,
        ..Default::default()
    })
    .await
    .err()
    .expect("uncompiled GPU must refuse");
    assert!(
        error.to_string().contains("enable granite-vulkan"),
        "{error:#}"
    );
}

#[tokio::test]
#[ignore = "loads the real cached legacy model; explicit acceptance only"]
async fn explicit_legacy_model_keeps_cpu_vectors_and_legacy_identity() {
    let loaded = load_provider(ProviderOptions::default()).await.unwrap();
    assert_eq!(
        loaded.provider.model_id(),
        "sentence-transformers/all-MiniLM-L6-v2"
    );
    assert_eq!(
        loaded.space,
        EmbeddingSpace::Legacy {
            model: "sentence-transformers/all-MiniLM-L6-v2".into(),
            dimensions: 384,
            max_tokens: 256,
        }
    );
    assert_eq!(loaded.effective_max_tokens, 256);
    let vectors = loaded.provider.embed(&["memory"]).await.unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), 384);
}

#[tokio::test]
#[ignore = "requires checksum-pinned artifacts and an explicitly selected compiled GPU"]
async fn factory_explicit_gpu_preserves_cpu_embedding_space() {
    let dir = std::env::var_os("ALEXANDRIA_GRANITE_TEST_ARTIFACT_DIR")
        .expect("explicit artifact directory");
    let device = std::env::var("ALEXANDRIA_GRANITE_TEST_DEVICE").expect("explicit GPU selector");
    assert!(device.starts_with("rocm:") || device.starts_with("vulkan:"));
    let options = |device: String| ProviderOptions {
        model: GRANITE_MODEL_ID.to_owned(),
        device,
        max_tokens: 16,
        artifact_dir: Some(dir.clone().into()),
    };
    let cpu = load_provider(options("cpu".into())).await.unwrap();
    let space = cpu.space.clone();
    let cpu_vectors = cpu.provider.embed(&["memory", ""]).await.unwrap();
    drop(cpu);
    let gpu = load_provider(options(device)).await.unwrap();
    assert_eq!(gpu.space, space);
    assert_eq!(gpu.effective_max_tokens, 16);
    let gpu_vectors = gpu.provider.embed(&["memory", ""]).await.unwrap();
    assert_eq!(gpu_vectors.len(), 2);
    for (cpu, gpu) in cpu_vectors.iter().zip(&gpu_vectors) {
        assert_eq!(gpu.len(), 768);
        assert!(gpu.iter().all(|x| x.is_finite()));
        assert!(cpu.iter().zip(gpu).all(|(a, b)| (a - b).abs() < 1e-5));
    }
}

#[tokio::test]
#[ignore = "requires the checksum-pinned local Granite artifact directory"]
async fn factory_cpu_default_and_explicit_budget_match_actual_vectors_and_space() {
    let dir = std::env::var_os("ALEXANDRIA_GRANITE_TEST_ARTIFACT_DIR")
        .expect("explicit artifact directory");
    let loaded = load_provider(ProviderOptions {
        model: GRANITE_MODEL_ID.to_owned(),
        max_tokens: 16,
        artifact_dir: Some(dir.into()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(loaded.provider.model_id(), GRANITE_MODEL_ID);
    assert_eq!(loaded.space.model(), GRANITE_MODEL_ID);
    assert_eq!(loaded.space.dimensions(), 768);
    assert_eq!(loaded.space.max_tokens(), 16);
    assert_eq!(loaded.effective_max_tokens, 16);
    loaded.space.validate().unwrap();
    let vectors = loaded.provider.embed(&["memory", ""]).await.unwrap();
    assert_eq!(vectors.len(), 2);
    for vector in vectors {
        assert_eq!(vector.len(), 768);
        let norm = vector
            .iter()
            .map(|&x| f64::from(x).powi(2))
            .sum::<f64>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }
}
