//! Opt-in native evaluation over public/synthetic or private frozen fixtures.
//! No DB access, ordinary test downloads, implicit overwrite or Python inference.

use anyhow::{Result, ensure};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    format_version: u32,
    samples: Vec<Sample>,
}
impl Fixture {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let fixture: Self = serde_json::from_slice(bytes)?;
        ensure!(
            fixture.format_version == 1,
            "Unsupported evaluation fixture version"
        );
        ensure!(
            !fixture.samples.is_empty(),
            "Evaluation roster must not be empty"
        );
        let mut ids = HashSet::with_capacity(fixture.samples.len());
        for sample in &fixture.samples {
            ensure!(
                !sample.id.trim().is_empty(),
                "Evaluation sample ID must not be blank"
            );
            ensure!(ids.insert(&sample.id), "Duplicate evaluation sample ID");
        }
        Ok(fixture)
    }
}
fn create_output(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
fn check_vector(vectors: &[Vec<f32>], dimensions: usize) -> Result<()> {
    ensure!(
        vectors.len() == 1,
        "Evaluation requires one vector per single-text call"
    );
    let vector = &vectors[0];
    ensure!(
        vector.len() == dimensions,
        "Evaluation vector dimension mismatch"
    );
    ensure!(
        vector.iter().all(|v| v.is_finite()),
        "Evaluation vector is not finite"
    );
    let norm = vector
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    ensure!(
        (norm - 1.0).abs() <= 1e-5,
        "Evaluation vector is not a unit vector"
    );
    Ok(())
}
fn write_line(output: &mut impl Write, record: &serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *output, record)?;
    output.write_all(b"\n")?;
    Ok(())
}

#[test]
fn fixture_accepts_empty_text_and_opaque_ids_but_rejects_invalid_rosters() {
    let fixture =
        Fixture::parse(br#"{"format_version":1,"samples":[{"id":"opaque:quote'","text":""}]}"#)
            .unwrap();
    assert_eq!(fixture.samples[0].id, "opaque:quote'");
    assert!(fixture.samples[0].text.is_empty());
    assert_eq!(fixture.format_version, 1);
    for bad in [
        r#"{"format_version":2,"samples":[{"id":"x","text":"t"}]}"#,
        r#"{"format_version":1,"samples":[]}"#,
        r#"{"format_version":1,"samples":[{"id":" ","text":"t"}]}"#,
        r#"{"format_version":1,"samples":[{"id":"x","text":"t"},{"id":"x","text":"s"}]}"#,
        r#"{"format_version":1,"samples":[{"id":"x","text":"t","embedding":[1]}]}"#,
        r#"{"format_version":1,"samples":[{"id":"x","text":"t"}],"model":"untrusted"}"#,
    ] {
        assert!(Fixture::parse(bad.as_bytes()).is_err(), "{bad}");
    }
}

#[test]
fn evaluation_never_overwrites_a_prior_success_or_failure() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("vectors.jsonl");
    let mut file = create_output(&path).unwrap();
    file.write_all(b"failure evidence").unwrap();
    file.sync_all().unwrap();
    assert!(create_output(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"failure evidence");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn evaluation_checks_cardinality_shape_finiteness_and_unit_norm_without_repair() {
    check_vector(&[vec![1.0, 0.0]], 2).unwrap();
    for vectors in [
        vec![],
        vec![vec![1.0, 0.0]; 2],
        vec![vec![1.0]],
        vec![vec![f32::NAN, 0.0]],
        vec![vec![f32::INFINITY, 0.0]],
        vec![vec![2.0, 0.0]],
        vec![vec![0.0, 0.0]],
    ] {
        assert!(check_vector(&vectors, 2).is_err());
    }
}

#[test]
fn public_pilot_labels_cover_explicit_same_group_targets_and_unanswerable_cases() {
    let pilot: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/retrieval-pilot.json")).unwrap();
    let documents = pilot["documents"].as_array().unwrap();
    let queries = pilot["queries"].as_array().unwrap();
    let mut ids = HashSet::new();
    for sample in documents.iter().chain(queries) {
        assert!(ids.insert(sample["id"].as_str().unwrap()));
    }
    assert_eq!(pilot["adjudication"]["release_calibration"], false);
    for group in ["memory", "code"] {
        let cases: Vec<_> = queries.iter().filter(|q| q["group"] == group).collect();
        assert_eq!(
            cases
                .iter()
                .filter(|q| q["relevant_ids"].as_array().unwrap().is_empty())
                .count(),
            4
        );
        assert!(
            cases
                .iter()
                .any(|q| q["relevant_ids"].as_array().unwrap().len() > 1)
        );
    }
    for query in queries {
        let mut labels = HashSet::new();
        for id in query["relevant_ids"].as_array().unwrap() {
            assert!(labels.insert(id.as_str().unwrap()));
            assert!(
                documents
                    .iter()
                    .any(|doc| doc["id"] == *id && doc["group"] == query["group"])
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit frozen input, new private output and verified local model artifacts/cache"]
async fn native_fixture_evaluation() -> Result<()> {
    use alexandria_pipeline::embedding::{ProviderOptions, default_max_tokens, load_provider};
    use alexandria_storage::models::embedding::GRANITE_MODEL_ID;
    use sha2::{Digest, Sha256};
    use std::time::Instant;
    let input = std::env::var_os("ALEXANDRIA_EVAL_INPUT").expect("provide frozen input");
    let output = std::env::var_os("ALEXANDRIA_EVAL_OUTPUT").expect("provide a new private output");
    let bytes = std::fs::read(input)?;
    let fixture = Fixture::parse(&bytes)?;
    let mut output = std::io::BufWriter::new(create_output(Path::new(&output))?);
    write_line(
        &mut output,
        &serde_json::json!({"type":"start", "format_version":1, "input_sha256":format!("{:x}", Sha256::digest(&bytes)), "sample_count":fixture.samples.len()}),
    )?;
    output.flush()?;
    let model = std::env::var("ALEXANDRIA_EVAL_MODEL").unwrap_or_else(|_| GRANITE_MODEL_ID.into());
    let device = std::env::var("ALEXANDRIA_GRANITE_TEST_DEVICE").unwrap_or_else(|_| "cpu".into());
    let max_tokens = std::env::var("ALEXANDRIA_EVAL_MAX_TOKENS")
        .ok()
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or_else(|| default_max_tokens(&model));
    let started = Instant::now();
    let loaded = load_provider(ProviderOptions {
        model,
        device: device.clone(),
        max_tokens,
        artifact_dir: std::env::var_os("ALEXANDRIA_GRANITE_TEST_ARTIFACT_DIR").map(Into::into),
    })
    .await?;
    write_line(
        &mut output,
        &serde_json::json!({"type":"ready", "space":loaded.space, "device":device, "initialization_ms":started.elapsed().as_secs_f64()*1000.0}),
    )?;
    for (index, sample) in fixture.samples.iter().enumerate() {
        let started = Instant::now();
        let vectors = loaded.provider.embed(&[sample.text.as_str()]).await?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        check_vector(&vectors, loaded.space.dimensions())?;
        write_line(
            &mut output,
            &serde_json::json!({"type":"vector", "id":sample.id, "vector":vectors[0], "embedding_ms":elapsed_ms, "overflow":loaded.provider.overflow(&sample.text)}),
        )?;
        if (index + 1) % 256 == 0 {
            output.flush()?;
            eprintln!(
                "Native evaluation: {}/{} samples",
                index + 1,
                fixture.samples.len()
            );
        }
    }
    write_line(
        &mut output,
        &serde_json::json!({"type":"complete", "sample_count":fixture.samples.len()}),
    )?;
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(())
}
