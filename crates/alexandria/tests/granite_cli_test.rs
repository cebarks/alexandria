use alexandria_storage::models::embedding::GRANITE_MODEL_ID;

#[test]
fn in_place_granite_cli_refuses_without_creating_a_source_store() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("must-not-be-created");
    let config = directory.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_alexandria"))
        .arg("migrate-embeddings")
        .arg("--force")
        .env("ALEXANDRIA_CONFIG", config)
        .env("ALEXANDRIA_DATA_DIR", &source)
        .env("ALEXANDRIA_EMBEDDING_MODEL", GRANITE_MODEL_ID)
        .env("ALEXANDRIA_EMBEDDING_DEVICE", "cpu")
        .env("XDG_CACHE_HOME", directory.path().join("cache"))
        .env("ALEXANDRIA_EMBEDDING_MAX_TOKENS", "8192")
        .env_remove("ALEXANDRIA_EMBEDDING_ARTIFACT_DIR")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("separate offline"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !source.exists(),
        "a refused migration must not create or open its source"
    );
}
