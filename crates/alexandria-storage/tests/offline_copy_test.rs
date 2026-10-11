#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use alexandria_storage::offline_copy::{OfflineStoreGuard, SourceManifest, source_manifest_path};
use alexandria_storage::repos::MemoryRepo;
use alexandria_storage::{Database, schema};

struct EngineChild(Child);

impl EngineChild {
    fn start(source: &Path, ready: &Path) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "real_engine_child", "--ignored", "--nocapture"])
            .env("ALEXANDRIA_COPY_TEST_STORE", source)
            .env("ALEXANDRIA_COPY_TEST_READY", ready)
            .env("ALEXANDRIA_COPY_TEST_MODE", "hold")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Self(child);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "engine child exited before readiness"
            );
            assert!(Instant::now() < deadline, "engine readiness deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }
}

impl Drop for EngineChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn engine(mode: &str, path: &Path) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "real_engine_child", "--ignored", "--nocapture"])
        .env("ALEXANDRIA_COPY_TEST_STORE", path)
        .env("ALEXANDRIA_COPY_TEST_MODE", mode)
        .env_remove("ALEXANDRIA_COPY_TEST_READY")
        .output()
        .unwrap()
}

fn seeded_source(root: &Path) -> PathBuf {
    let source = root.join("source");
    let result = engine("seed", &source);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    source
}

fn payload(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path == root.join("LOCK") {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[tokio::test]
#[ignore = "parent tests launch this against isolated temporary stores"]
async fn real_engine_child() {
    let path = std::env::var_os("ALEXANDRIA_COPY_TEST_STORE").expect("parent-owned store");
    let mode = std::env::var("ALEXANDRIA_COPY_TEST_MODE").expect("child mode");
    let db = match Database::connect_persistent(Path::new(&path)).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(2);
        }
    };
    if mode == "seed" || mode == "hold" {
        schema::migrate(db.inner()).await.unwrap();
        MemoryRepo::new(db.inner())
            .create_fact("public isolated source fixture", 0.5, &[1.0, 0.0], &[])
            .await
            .unwrap();
    } else {
        assert_eq!(
            MemoryRepo::new(db.inner())
                .count(&Default::default())
                .await
                .unwrap(),
            1
        );
    }
    if mode == "hold" {
        let ready = std::env::var_os("ALEXANDRIA_COPY_TEST_READY").expect("parent readiness file");
        fs::write(ready, "ready").unwrap();
        let mut release = String::new();
        std::io::stdin().read_line(&mut release).unwrap();
    }
    // No database drop: the parent also exercises recovery after an abrupt exit.
    std::process::exit(0);
}

#[test]
fn multi_buffer_copy_has_independently_computed_digest_and_private_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("LOCK"), "stale PID").unwrap();
    fs::set_permissions(source.join("LOCK"), fs::Permissions::from_mode(0o400)).unwrap();
    let bytes: Vec<u8> = (0..3 * 65536 + 17)
        .map(|index| (index % 251) as u8)
        .collect();
    fs::write(source.join("chunked"), &bytes).unwrap();
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    let target = temporary.path().join("new-parent/deeper/target");
    let manifest = guard.copy_to(&target).unwrap();
    assert_eq!(manifest.files[0].size, bytes.len() as u64);
    assert_eq!(
        manifest.files[0].sha256,
        "4bb7439bc39bc2d0e3d6a915d7c81e38250a9c5bb320a94a19a95bba0d5fe40a"
    );
    assert_eq!(fs::read(target.join("chunked")).unwrap(), bytes);
    for path in [target.as_path(), target.parent().unwrap()] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    for path in [
        target.join("chunked"),
        source_manifest_path(&target).unwrap(),
    ] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    assert_eq!(fs::read(source.join("LOCK")).unwrap(), b"stale PID");
}

#[test]
fn symlinked_lock_and_non_regular_payload_refuse_without_touching_links() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let outside = temporary.path().join("outside");
    fs::write(&outside, "outside lock").unwrap();
    std::os::unix::fs::symlink(&outside, source.join("LOCK")).unwrap();
    assert!(OfflineStoreGuard::acquire(&source).is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside lock");
    let source = seeded_source(&temporary.path().join("isolated"));
    let socket = std::os::unix::net::UnixListener::bind(source.join("socket")).unwrap();
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    let target = temporary.path().join("target");
    assert!(guard.copy_to(&target).is_err());
    assert!(!target.exists());
    drop(socket);
}

#[test]
fn malformed_source_manifest_is_not_a_resume_receipt() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    let target = temporary.path().join("target");
    let manifest = OfflineStoreGuard::acquire(&source)
        .unwrap()
        .copy_to(&target)
        .unwrap();
    for variant in ["version", "duplicate", "parent", "absolute", "digest"] {
        let mut bad = manifest.clone();
        match variant {
            "version" => bad.format_version = 2,
            "duplicate" => bad.files.insert(0, bad.files[0].clone()),
            "parent" => bad.files[0].path = "../escape".into(),
            "absolute" => bad.files[0].path = "/escape".into(),
            "digest" => bad.files[0].sha256 = "invalid".into(),
            _ => unreachable!(),
        }
        use sha2::{Digest, Sha256};
        bad.sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(bad.format_version, &bad.files)).unwrap())
        );
        assert!(bad.validate().is_err(), "{variant}");
    }
    let mut json = serde_json::to_value(manifest).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("unknown".into(), true.into());
    assert!(serde_json::from_value::<SourceManifest>(json).is_err());
}

#[test]
fn live_writer_refused_without_lock_write() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let _child = EngineChild::start(&source, &temporary.path().join("ready"));
    let before = fs::read(source.join("LOCK")).unwrap();
    assert!(OfflineStoreGuard::acquire(&source).is_err());
    assert_eq!(fs::read(source.join("LOCK")).unwrap(), before);
}

#[test]
fn guard_blocks_engine_restart() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    let before = payload(&source);
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    let refused = engine("open", &source);
    assert!(
        !refused.status.success(),
        "guard must retain the native engine lock"
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("lock"));
    assert_eq!(
        payload(&source),
        before,
        "the failed opener may truncate LOCK, not payload"
    );
    drop(guard);
    assert!(engine("open", &source).status.success());
}

#[test]
fn stale_pid_not_a_live_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    fs::write(source.join("LOCK"), "999999999\n").unwrap();
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    assert_eq!(fs::read(source.join("LOCK")).unwrap(), b"999999999\n");
    assert!(!engine("open", &source).status.success());
    drop(guard);
}

#[test]
fn missing_lock_not_created() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    assert!(OfflineStoreGuard::acquire(&source).is_err());
    assert!(!source.join("LOCK").exists());
    let missing = temporary.path().join("missing");
    assert!(OfflineStoreGuard::acquire(&missing).is_err());
    assert!(!missing.exists());
    assert!(OfflineStoreGuard::acquire(Path::new(":memory:")).is_err());
}

#[test]
fn source_payload_unchanged() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    fs::create_dir(source.join("extra")).unwrap();
    fs::create_dir(source.join("empty")).unwrap();
    fs::write(
        source.join("extra/LOCK"),
        b"nested LOCK is ordinary payload",
    )
    .unwrap();
    fs::write(source.join("extra/α.txt"), b"public additional bytes").unwrap();
    fs::write(
        source.join("historical-manifest.json"),
        b"preserve prior in-store receipt bytes",
    )
    .unwrap();
    let before = payload(&source);
    let lock_before = fs::read(source.join("LOCK")).unwrap();
    let destination = temporary.path().join("target");
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    let manifest = guard.copy_to(&destination).unwrap();
    assert_eq!(payload(&source), before);
    assert_eq!(fs::read(source.join("LOCK")).unwrap(), lock_before);
    assert_eq!(payload(&destination), before);
    assert!(destination.join("empty").is_dir());
    assert!(!destination.join("LOCK").exists());
    assert_eq!(manifest.files.len(), before.len());
    assert!(
        manifest
            .files
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
    assert!(manifest.files.iter().any(|file| file.path == "extra/LOCK"));
    assert!(!manifest.files.iter().any(|file| file.path == "LOCK"));
    let encoded = fs::read(source_manifest_path(&destination).unwrap()).unwrap();
    let stored: SourceManifest = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(stored, manifest);
    stored.validate().unwrap();
    let mut corrupt = stored.clone();
    corrupt.files[0].size += 1;
    assert!(corrupt.validate().is_err());
    let second = temporary.path().join("target2");
    assert_eq!(
        guard.copy_to(&second).unwrap(),
        manifest,
        "destination does not change snapshot identity"
    );
}

#[test]
fn copy_opens_independently() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    let destination = temporary.path().join("target");
    let before = payload(&source);
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    guard.copy_to(&destination).unwrap();
    let target = engine("open", &destination);
    assert!(
        target.status.success(),
        "{}",
        String::from_utf8_lossy(&target.stderr)
    );
    assert_eq!(payload(&source), before);
    assert!(!engine("open", &source).status.success());
}

#[test]
fn symlink_overlap_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    let alias = temporary.path().join("alias");
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    for destination in [
        &source,
        &alias,
        &source.join("child"),
        &alias.join("missing-parent/child"),
        temporary.path(),
    ] {
        assert!(guard.copy_to(destination).is_err());
    }
    assert!(!source.join("child").exists());
    let outside = temporary.path().join("outside");
    fs::write(&outside, "untouched").unwrap();
    std::os::unix::fs::symlink(&outside, source.join("linked-payload")).unwrap();
    let target = temporary.path().join("target");
    assert!(guard.copy_to(&target).is_err());
    assert!(!target.exists());
    assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
}

#[test]
fn nonempty_destination_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    let target = temporary.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("owned-by-someone-else"), "do not overwrite").unwrap();
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    assert!(guard.copy_to(&target).is_err());
    assert_eq!(
        fs::read_to_string(target.join("owned-by-someone-else")).unwrap(),
        "do not overwrite"
    );
    assert!(!source_manifest_path(&target).unwrap().exists());
}

#[test]
fn mid_copy_failure_does_not_publish_ready_target() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("LOCK"), "old-pid").unwrap();
    fs::write(source.join("a-good"), b"first copied bytes").unwrap();
    let unreadable = source.join("z-unreadable");
    fs::write(&unreadable, b"cannot be copied").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o0)).unwrap();
    let target = temporary.path().join("target");
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    let result = guard.copy_to(&target);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(result.is_err());
    assert!(!target.exists());
    assert!(!source_manifest_path(&target).unwrap().exists());
    let stages: Vec<_> = fs::read_dir(temporary.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".incomplete")
        })
        .collect();
    assert_eq!(stages.len(), 1);
    assert_eq!(
        fs::read(stages[0].join("a-good")).unwrap(),
        b"first copied bytes"
    );
}

#[test]
fn existing_sidecar_is_not_overwritten_and_empty_destination_is_supported() {
    let temporary = tempfile::tempdir().unwrap();
    let source = seeded_source(temporary.path());
    let target = temporary.path().join("target");
    let manifest_path = source_manifest_path(&target).unwrap();
    fs::write(&manifest_path, "another owner's receipt").unwrap();
    let guard = OfflineStoreGuard::acquire(&source).unwrap();
    assert!(guard.copy_to(&target).is_err());
    assert_eq!(
        fs::read_to_string(&manifest_path).unwrap(),
        "another owner's receipt"
    );
    let empty = temporary.path().join("empty-target");
    fs::create_dir(&empty).unwrap();
    guard.copy_to(&empty).unwrap();
    assert_eq!(payload(&empty), payload(&source));
}
