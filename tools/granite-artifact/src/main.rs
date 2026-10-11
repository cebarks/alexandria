use anyhow::{Context, Result, ensure};
use burn_onnx::{LoadStrategy, ModelGen};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const REVISION: &str = "44399559930365213510b1ee2eb15ded83374f0e";
const ONNX_SHA: &str = "75f9f258bf5013f5fe8a4dad61dd0fd16ac0cbaa7a106e3d3f41c2d04a42d541";
const TOKENIZER_SHA: &str = "0087c868b33bad550a78a08d19798cfd7f713cde4f020803b8f51f405503e15f";
const IMPORTER_SHA: &str = "5c83ad0957c8160303e453d7d6f015c603e7f8af0828835b52fd6f8301d6cc34";
const LICENSE_FILES: [&str; 3] = ["APACHE-2.0.txt", "GEMMA-TERMS.html", "NOTICE.txt"];

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [command, onnx, tokenizer, output, licenses] if command == "build" => build(
            Path::new(onnx),
            Path::new(tokenizer),
            Path::new(output),
            Path::new(licenses),
        ),
        [command, pack] if command == "inspect" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&inspect_pack(Path::new(pack))?)?
            );
            Ok(())
        }
        _ => anyhow::bail!(
            "Usage: granite-artifact build MODEL.onnx TOKENIZER.json NEW_OUTPUT_DIR LICENSE_DIR\n       granite-artifact inspect MODEL.bpk"
        ),
    }
}

fn build(onnx: &Path, tokenizer: &Path, output: &Path, licenses: &Path) -> Result<()> {
    ensure!(
        !output.exists(),
        "output directory already exists; never reused"
    );
    ensure!(file_sha(onnx)? == ONNX_SHA, "ONNX SHA256 mismatch");
    ensure!(
        file_sha(tokenizer)? == TOKENIZER_SHA,
        "tokenizer SHA256 mismatch"
    );
    ensure!(
        onnx.file_stem().is_some_and(|stem| stem == "model"),
        "pinned input must be named model.onnx"
    );
    for filename in LICENSE_FILES {
        let path = licenses.join(filename);
        ensure!(
            path.is_file() && path.metadata()?.len() > 0,
            "Missing license packet file: {filename}"
        );
    }
    let onnx = input_path(onnx)?;
    let tokenizer = tokenizer.canonicalize()?;
    let licenses = licenses.canonicalize()?;
    fs::create_dir(output)?;
    let output = output.canonicalize()?;
    fs::write(
        output.join("BUILD_INCOMPLETE"),
        "Do not use or publish this unfinished artifact.\n",
    )?;
    // This standalone process changes cwd before the importer creates any threads.
    // A canonical relative output keeps generated source free of machine-local paths.
    std::env::set_current_dir(&output)?;
    fs::hard_link(onnx.canonicalize()?, "model.onnx")
        .or_else(|_| fs::copy(&onnx, "model.onnx").map(|_| ()))
        .context("stage pinned ONNX under its portable input name")?;
    ModelGen::new()
        .input("model.onnx")
        .out_dir("generated")
        .development(false)
        .simplify(true)
        .partition(true)
        .load_strategy(LoadStrategy::File)
        .run_from_cli();
    fs::remove_file("model.onnx")?;
    fs::rename("generated/model.rs", "model.rs")?;
    fs::rename("generated/model.bpk", "model.bpk")?;
    fs::remove_dir("generated")?;
    fs::copy(tokenizer, "tokenizer.json")?;
    emit_metadata(&output, &licenses)?;
    fs::remove_file("BUILD_INCOMPLETE")?;
    println!(
        "Built verified local artifact: {} (publication still requires approval and licensing review)",
        output.display()
    );
    Ok(())
}

fn emit_metadata(output: &Path, licenses: &Path) -> Result<()> {
    let pack_sha = file_sha(&output.join("model.bpk"))?;
    let source_sha = file_sha(&output.join("model.rs"))?;
    let manifest = json!({
        "format_version": 1,
        "space": {
            "kind": "granite_v1", "model": "ibm-granite/granite-embedding-311m-multilingual-r2",
            "revision": REVISION, "onnx_sha256": ONNX_SHA,
            "burnpack_sha256": pack_sha, "tokenizer_sha256": TOKENIZER_SHA,
            "pooling": "cls", "normalization": "l2-f64-accum-f32", "precision": "f32",
            "semantics_version": 1, "dimensions": 768, "max_tokens": 8192
        },
        "weights": {"filename": "model.bpk", "url": null, "sha256": pack_sha},
        "tokenizer": {"filename": "tokenizer.json", "url": null, "sha256": TOKENIZER_SHA},
        "generated_model_sha256": source_sha, "importer_version": "0.22.0"
    });
    write_json(&output.join("artifact-manifest.json"), &manifest)?;
    write_json(
        &output.join("semantic-tensors.json"),
        &inspect_pack(&output.join("model.bpk"))?,
    )?;
    fs::create_dir(output.join("licenses"))?;
    let mut license_hashes = BTreeMap::new();
    for filename in LICENSE_FILES {
        let destination = output.join("licenses").join(filename);
        fs::copy(licenses.join(filename), &destination)?;
        license_hashes.insert(filename, file_sha(&destination)?);
    }
    let lock_sha = format!("{:x}", Sha256::digest(include_bytes!("../Cargo.lock")));
    write_json(
        &output.join("provenance.json"),
        &json!({
            "source_revision": REVISION, "source_onnx_sha256": ONNX_SHA,
            "importer_crate_sha256": IMPORTER_SHA, "importer_lock_sha256": lock_sha,
            "generated_model_sha256": source_sha, "burnpack_sha256": pack_sha,
            "license_files_sha256": license_hashes,
            "weights_license": "Apache-2.0",
            "tokenizer_terms": "Gemma Terms of Use (Gemma3-derived)",
            "redistribution_review_required": true,
            "settings": {"development": false, "simplify": true, "partition": true, "load_strategy": "file"}
        }),
    )
}

fn inspect_pack(path: &Path) -> Result<Value> {
    let reader = burn_pack::Reader::from_file(path)?;
    let scalars = format!("{:?}", reader.scalars());
    let metadata = reader.metadata().clone();
    let mut tensors = BTreeMap::new();
    for tensor in reader.into_tensors()? {
        let (name, dtype, shape, _parameter_id, bytes) = tensor.into_parts()?;
        tensors.insert(
            name,
            json!({
                "dtype": format!("{dtype:?}"), "shape": format!("{shape:?}"),
                "sha256": format!("{:x}", Sha256::digest(&*bytes)), "bytes": bytes.len()
            }),
        );
    }
    Ok(json!({"metadata": metadata, "scalars": scalars, "tensors": tensors}))
}

fn input_path(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn huggingface_symlink_keeps_the_model_filename() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("granite-source-alias-{nonce}"));
        fs::create_dir(&directory).unwrap();
        let blob = directory.join("opaque-hash");
        fs::write(&blob, b"fixture").unwrap();
        let alias = directory.join("model.onnx");
        std::os::unix::fs::symlink(&blob, &alias).unwrap();
        let resolved = input_path(&alias).unwrap();
        fs::remove_dir_all(directory).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved.file_name(),
            Some(std::ffi::OsStr::new("model.onnx"))
        );
    }
}

fn file_sha(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "Not a regular artifact file: {}",
        path.display()
    );
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
