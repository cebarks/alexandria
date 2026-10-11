use std::path::PathBuf;
use std::process::Command;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("granite-artifact-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn invalid_onnx_digest_is_rejected_before_output_creation() {
    let directory = Directory::new();
    let onnx = directory.0.join("model.onnx");
    let tokenizer = directory.0.join("tokenizer.json");
    let destination = directory.0.join("output");
    std::fs::write(&onnx, b"not the pinned ONNX").unwrap();
    std::fs::write(&tokenizer, b"{}").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_granite-artifact"))
        .arg("build")
        .arg(onnx)
        .arg(tokenizer)
        .arg(&destination)
        .arg(&directory.0)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("ONNX SHA256 mismatch"));
    assert!(!destination.exists());
}

#[test]
fn existing_output_directory_is_never_reused() {
    let directory = Directory::new();
    let destination = directory.0.join("output");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("owned"), b"untouched").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_granite-artifact"))
        .arg("build")
        .arg("unused.onnx")
        .arg("unused-tokenizer.json")
        .arg(&destination)
        .arg(&directory.0)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("output directory already exists"));
    assert_eq!(
        std::fs::read(destination.join("owned")).unwrap(),
        b"untouched"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires pinned local ONNX/tokenizer/license packet; runs conversion, not inference"]
fn generated_source_is_independent_of_the_local_input_alias() {
    let directory = Directory::new();
    let source = PathBuf::from(
        std::env::var_os("ALEXANDRIA_GRANITE_TEST_ONNX").expect("provide pinned ONNX"),
    );
    let tokenizer =
        std::env::var_os("ALEXANDRIA_GRANITE_TEST_TOKENIZER").expect("provide pinned tokenizer");
    let licenses =
        std::env::var_os("ALEXANDRIA_GRANITE_TEST_LICENSE_DIR").expect("provide license packet");
    let mut generated = Vec::new();
    for name in ["first-alias", "second-alias"] {
        let alias = directory.0.join(name);
        std::fs::create_dir(&alias).unwrap();
        let onnx = alias.join("model.onnx");
        std::os::unix::fs::symlink(&source, &onnx).unwrap();
        let output = alias.join("output");
        let result = Command::new(env!("CARGO_BIN_EXE_granite-artifact"))
            .arg("build")
            .arg(&onnx)
            .arg(&tokenizer)
            .arg(&output)
            .arg(&licenses)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        generated.push(std::fs::read(output.join("model.rs")).unwrap());
    }
    assert!(
        generated[0] == generated[1],
        "identical input bytes through different local aliases must produce identical source"
    );
}

#[test]
fn missing_arguments_report_usage() {
    let result = Command::new(env!("CARGO_BIN_EXE_granite-artifact"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Usage:"));
}
