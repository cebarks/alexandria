//! Offline physical copying; never connect the source storage engine here.
//!
//! The read-only LOCK handle protects against cooperating SurrealKV writers, not
//! arbitrary filesystem changes or unverified network-filesystem lock semantics.
//! Native std/fs2 interoperability is verified on Linux only. Failed copies retain
//! their private `.incomplete` staging directory; no automatic cleanup is implied.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Receipt of the original copy, not checksums of a target after engine recovery
/// or re-embedding. The digest covers canonical JSON `[format_version, files]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub format_version: u32,
    pub files: Vec<SourceFile>,
    pub sha256: String,
}

impl SourceManifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "Unsupported source manifest version"
        );
        ensure!(
            self.files
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path),
            "Source manifest paths must be unique and sorted"
        );
        for file in &self.files {
            ensure!(
                file.path != "LOCK"
                    && file
                        .path
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != ".."),
                "Invalid source manifest payload path"
            );
            ensure!(valid_digest(&file.sha256), "Invalid source file SHA256");
        }
        ensure!(
            valid_digest(&self.sha256) && self.sha256 == self.canonical_digest()?,
            "Source manifest SHA256 mismatch"
        );
        Ok(())
    }

    fn canonical_digest(&self) -> Result<String> {
        let mut hasher = Sha256::new();
        serde_json::to_writer(&mut hasher, &(self.format_version, &self.files))?;
        Ok(format!("{:x}", hasher.finalize()))
    }
}

pub struct OfflineStoreGuard {
    source: PathBuf,
    _lock: File,
}

impl OfflineStoreGuard {
    /// Acquire an existing regular LOCK without creation, truncation or PID writes.
    /// Keep this guard alive until the copy and manifest publication finish.
    pub fn acquire(source: &Path) -> Result<Self> {
        ensure!(
            cfg!(target_os = "linux"),
            "Offline source-lock interoperability is verified only on Linux"
        );
        ensure!(
            source != Path::new(":memory:"),
            "An in-memory database cannot be copied offline"
        );
        let source = source
            .canonicalize()
            .context("Resolve existing source store")?;
        ensure!(source.is_dir(), "Source store must be a directory");
        let path = source.join("LOCK");
        ensure!(
            fs::symlink_metadata(&path)
                .context("Source must have an existing LOCK")?
                .file_type()
                .is_file(),
            "Source LOCK must be a regular, non-symlink file"
        );
        let lock = File::open(&path).context("Open source LOCK read-only")?;
        match lock.try_lock() {
            Ok(()) => Ok(Self {
                source,
                _lock: lock,
            }),
            Err(TryLockError::WouldBlock) => {
                bail!("Source store is locked; stop its server before copying")
            }
            Err(TryLockError::Error(error)) => Err(error)
                .context("Cannot acquire source lock; unsupported locking refuses migration"),
        }
    }

    /// Copy regular payload files through a 64 KiB buffer; the returned manifest
    /// takes O(file count) space. Retain every failed staging directory for inspection.
    /// The sidecar is durable before the new target directory becomes visible.
    pub fn copy_to(&self, destination: &Path) -> Result<SourceManifest> {
        let destination = resolve_destination(destination)?;
        ensure!(
            !destination.starts_with(&self.source) && !self.source.starts_with(&destination),
            "Source and output directories must not overlap"
        );
        require_empty_or_missing(&destination)?;
        let sidecar = source_manifest_path(&destination)?;
        require_missing(&sidecar)?;
        let parent = destination
            .parent()
            .context("Output directory must have a parent")?;
        create_private_parents(parent)?;
        let name = destination
            .file_name()
            .context("Output directory name")?
            .to_string_lossy();
        let stage = parent.join(format!(
            ".{name}.alexandria-copy-{}.incomplete",
            uuid::Uuid::new_v4()
        ));
        create_private_directory(&stage)?;
        let mut manifest = SourceManifest {
            format_version: 1,
            files: Vec::new(),
            sha256: String::new(),
        };
        catalog(&self.source, &self.source, &stage, &mut manifest.files)?;
        manifest
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        let mut buffer = [0u8; 64 * 1024];
        for file in &mut manifest.files {
            file.sha256 = copy_file(
                &self.source.join(&file.path),
                &stage.join(&file.path),
                file.size,
                &mut buffer,
            )?;
        }
        sync_directory_tree(&stage)?;
        manifest.sha256 = manifest.canonical_digest()?;
        manifest.validate()?;
        let mut receipt = BufWriter::new(create_private_file(&sidecar)?);
        serde_json::to_writer(&mut receipt, &manifest)?;
        receipt.flush().context("Write source manifest")?;
        receipt
            .get_ref()
            .sync_all()
            .context("Flush source manifest")?;
        sync_directory(parent)?;
        require_empty_or_missing(&destination)?;
        fs::rename(&stage, &destination).context("Publish complete migration-owned copy")?;
        sync_directory(parent)?;
        Ok(manifest)
    }
}

/// Sidecar next to the data directory; retain it with unfinished targets for resume.
pub fn source_manifest_path(destination: &Path) -> Result<PathBuf> {
    let destination = resolve_destination(destination)?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("Output must have a UTF-8 directory name")?;
    Ok(destination.with_file_name(format!("{name}.alexandria-source-manifest.json")))
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn resolve_destination(path: &Path) -> Result<PathBuf> {
    ensure!(
        path != Path::new(":memory:"),
        "Output must be a persistent directory"
    );
    let mut existing = std::path::absolute(path)?;
    let mut tail = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(metadata) => {
                ensure!(
                    !tail.is_empty() || !metadata.file_type().is_symlink(),
                    "Output directory must not be a symlink"
                );
                let mut resolved = existing.canonicalize()?;
                for component in tail.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(
                    existing
                        .file_name()
                        .context("Cannot resolve output path")?
                        .to_owned(),
                );
                ensure!(existing.pop(), "Cannot resolve output parent");
            }
            Err(error) => return Err(error).context("Inspect output path"),
        }
    }
}

fn require_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("Output source manifest already exists; never overwrite it"),
        Err(error) => Err(error).context("Inspect output source manifest"),
    }
}

fn require_empty_or_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && fs::read_dir(path)?.next().is_none(),
            "Output must be missing or empty; never overwrite a nonempty target"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Inspect output directory"),
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .context("Create private copy directory")
}

fn create_private_parents(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    create_private_parents(path.parent().context("Output parent")?)?;
    create_private_directory(path)?;
    sync_directory(path.parent().context("Output parent")?)
}

fn create_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .context("Create private copy file without overwriting")
}

fn catalog(root: &Path, directory: &Path, stage: &Path, files: &mut Vec<SourceFile>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path == root.join("LOCK") {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            create_private_directory(&stage.join(relative))?;
            catalog(root, &path, stage, files)?;
        } else {
            ensure!(
                metadata.file_type().is_file(),
                "Source payload must contain only regular files and directories: {}",
                path.display()
            );
            files.push(SourceFile {
                path: relative
                    .to_str()
                    .context("Source payload paths must be UTF-8")?
                    .to_string(),
                size: metadata.len(),
                sha256: String::new(),
            });
        }
    }
    Ok(())
}

fn copy_file(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    buffer: &mut [u8],
) -> Result<String> {
    let mut input =
        File::open(source).with_context(|| format!("Read source payload {}", source.display()))?;
    let before = input.metadata()?;
    ensure!(
        before.is_file() && before.len() == expected_size,
        "Source payload changed before copying"
    );
    let mut output = create_private_file(destination)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    loop {
        let count = input.read(buffer)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .context("Source file size overflow")?;
        ensure!(size <= expected_size, "Source payload grew while copying");
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    let after = input.metadata()?;
    ensure!(
        size == expected_size
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "Source payload changed while copying"
    );
    output.sync_all().context("Flush copied payload")?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().context("Flush copy directory")
}

fn sync_directory_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            sync_directory_tree(&path)?;
        }
    }
    sync_directory(path)
}
