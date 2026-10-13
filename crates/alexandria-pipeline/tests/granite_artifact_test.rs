use alexandria_pipeline::embedding::granite::artifacts::{ArtifactCache, PinnedArtifact};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn manifest_fixture() -> serde_json::Value {
    serde_json::json!({
        "format_version": 1,
        "space": {
            "kind": "granite_v1",
            "model": "ibm-granite/granite-embedding-311m-multilingual-r2",
            "revision": "44399559930365213510b1ee2eb15ded83374f0e",
            "onnx_sha256": "75f9f258bf5013f5fe8a4dad61dd0fd16ac0cbaa7a106e3d3f41c2d04a42d541",
            "burnpack_sha256": ABC_SHA256,
            "tokenizer_sha256": ABC_SHA256,
            "pooling": "cls", "normalization": "l2-f64-accum-f32",
            "precision": "f32", "semantics_version": 1,
            "dimensions": 768, "max_tokens": 8192
        },
        "weights": {"filename": "model.bpk", "url": null, "sha256": ABC_SHA256},
        "tokenizer": {"filename": "tokenizer.json", "url": null, "sha256": ABC_SHA256},
        "generated_model_sha256": ABC_SHA256,
        "importer_version": "0.22.0"
    })
}

#[test]
fn manifest_rejects_unknown_version_fields_and_conflicting_identity() {
    use alexandria_pipeline::embedding::granite::artifacts::ArtifactManifest;
    let fixture = manifest_fixture();
    assert!(ArtifactManifest::parse_json(&fixture.to_string()).is_ok());
    for (field, bad) in [
        ("format_version", serde_json::json!(2)),
        ("importer_version", serde_json::json!("0.23.0")),
        ("unexpected", serde_json::json!(true)),
    ] {
        let mut changed = fixture.clone();
        changed[field] = bad;
        assert!(ArtifactManifest::parse_json(&changed.to_string()).is_err());
    }
    let mut changed = fixture.clone();
    changed["weights"]["sha256"] = serde_json::json!("a".repeat(64));
    assert!(ArtifactManifest::parse_json(&changed.to_string()).is_err());
    let mut changed = fixture;
    changed["space"]["revision"] = serde_json::json!("a".repeat(40));
    assert!(ArtifactManifest::parse_json(&changed.to_string()).is_err());
}

struct Server {
    address: std::net::SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn new(body: &'static [u8], declared_length: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let counter = requests.clone();
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            for connection in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let mut connection = connection.unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 4096];
                let count = connection.read(&mut request).unwrap();
                assert!(count > 0);
                counter.fetch_add(1, Ordering::SeqCst);
                write!(connection, "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n").unwrap();
                connection.write_all(body).unwrap();
            }
        });
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn artifact(&self) -> PinnedArtifact {
        PinnedArtifact {
            filename: "model.bpk".into(),
            url: Some(format!("http://{}/model.bpk", self.address)),
            sha256: ABC_SHA256.into(),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn cache(directory: &std::path::Path) -> ArtifactCache {
    ArtifactCache::new(
        directory.to_owned(),
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
    )
}

#[tokio::test]
async fn local_manifest_cannot_replace_compiled_trusted_hashes() {
    use alexandria_pipeline::embedding::granite::artifacts::resolve_artifacts;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model.bpk"), b"abc").unwrap();
    std::fs::write(directory.path().join("tokenizer.json"), b"abc").unwrap();
    std::fs::write(
        directory.path().join("artifact-manifest.json"),
        manifest_fixture().to_string(),
    )
    .unwrap();
    let error = resolve_artifacts(Some(directory.path()))
        .await
        .expect_err("local hashes cannot become trusted");
    assert!(error.to_string().contains("SHA256 mismatch"));
}

#[tokio::test]
#[ignore = "requires explicitly provided, verified local Granite artifacts; no downloads"]
async fn verified_real_override_matches_compiled_identity() {
    use alexandria_pipeline::embedding::granite::artifacts::resolve_artifacts;
    let directory = std::path::PathBuf::from(
        std::env::var_os("ALEXANDRIA_GRANITE_TEST_ARTIFACT_DIR")
            .expect("provide the verified local artifact directory"),
    );
    let resolved = resolve_artifacts(Some(&directory)).await.unwrap();
    assert_eq!(resolved.weights_path, directory.join("model.bpk"));
    assert_eq!(resolved.tokenizer_path, directory.join("tokenizer.json"));
    assert_eq!(resolved.space.dimensions(), 768);
    assert_eq!(resolved.space.max_tokens(), 8192);
}

#[tokio::test]
async fn wrong_sha_never_publishes_a_cache_file() {
    let server = Server::new(b"bad", 3);
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cache(directory.path())
            .fetch(&server.artifact())
            .await
            .is_err()
    );
    assert!(!directory.path().join(ABC_SHA256).exists());
}

#[tokio::test]
async fn truncated_stream_never_publishes_a_cache_file() {
    let server = Server::new(b"abc", 30);
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cache(directory.path())
            .fetch(&server.artifact())
            .await
            .is_err()
    );
    assert!(!directory.path().join(ABC_SHA256).exists());
}

#[tokio::test]
async fn verified_cache_hit_is_offline_and_still_hash_checked() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(ABC_SHA256);
    std::fs::write(&path, b"abc").unwrap();
    let artifact = PinnedArtifact {
        filename: "model.bpk".into(),
        url: Some("http://127.0.0.1:1/not-available".into()),
        sha256: ABC_SHA256.into(),
    };
    assert_eq!(
        cache(directory.path()).fetch(&artifact).await.unwrap(),
        path
    );
    std::fs::write(&path, b"bad").unwrap();
    assert!(cache(directory.path()).fetch(&artifact).await.is_err());
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"bad",
        "corruption is reported, not silently replaced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_misses_download_once_and_return_verified_bytes() {
    let server = Server::new(b"abc", 3);
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path());
    let artifact = server.artifact();
    let (first, second) = tokio::join!(cache.fetch(&artifact), cache.fetch(&artifact));
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    assert_eq!(std::fs::read(first).unwrap(), b"abc");
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn local_override_obeys_the_same_checksum() {
    let server = Server::new(b"abc", 3);
    let directory = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    std::fs::write(local.path().join("model.bpk"), b"abc").unwrap();
    let cache = cache(directory.path());
    let resolved = cache.local(&server.artifact(), local.path()).await.unwrap();
    assert_eq!(std::fs::read(resolved).unwrap(), b"abc");
    std::fs::write(local.path().join("model.bpk"), b"bad").unwrap();
    assert!(cache.local(&server.artifact(), local.path()).await.is_err());
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn device_files_are_not_valid_artifacts_even_with_matching_bytes() {
    let directory = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/dev/null", directory.path().join("model.bpk")).unwrap();
    let artifact = PinnedArtifact {
        filename: "model.bpk".into(),
        url: None,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
    };
    assert!(
        cache(directory.path())
            .local(&artifact, directory.path())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_digest_and_path_traversal_are_rejected() {
    let server = Server::new(b"abc", 3);
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path());
    let mut artifact = server.artifact();
    artifact.sha256 = "../../outside".into();
    assert!(cache.fetch(&artifact).await.is_err());
    artifact.sha256 = ABC_SHA256.into();
    artifact.filename = "../outside".into();
    assert!(cache.local(&artifact, directory.path()).await.is_err());
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
}
