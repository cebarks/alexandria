//! Checksum-pinned artifacts, independent of the legacy mutable-main Hub cache.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use alexandria_storage::models::embedding::EmbeddingSpace;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedArtifact {
    pub filename: String,
    /// None until a separately approved artifact publication actually exists.
    pub url: Option<String>,
    pub sha256: String,
}

impl PinnedArtifact {
    fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.filename.as_str(), "model.bpk" | "tokenizer.json"),
            "Unsupported artifact filename"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "Invalid pinned artifact SHA256"
        );
        Ok(())
    }
}

pub const MODEL_REVISION: &str = "44399559930365213510b1ee2eb15ded83374f0e";
pub const ONNX_SHA256: &str = "75f9f258bf5013f5fe8a4dad61dd0fd16ac0cbaa7a106e3d3f41c2d04a42d541";
const RELEASE_BASE: &str =
    "https://github.com/cebarks/alexandria/releases/download/granite-311m-r2-burn-0.22.0-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub format_version: u32,
    pub space: EmbeddingSpace,
    pub weights: PinnedArtifact,
    pub tokenizer: PinnedArtifact,
    pub generated_model_sha256: String,
    pub importer_version: String,
}

impl ArtifactManifest {
    /// Immutable descriptor for admission/reporting without reading model bytes.
    pub fn compiled() -> Result<Self> {
        Self::parse_json(include_str!("artifact-manifest.json"))
    }

    pub fn parse_json(json: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(json)?;
        ensure!(
            manifest.format_version == 1,
            "Unsupported artifact manifest version"
        );
        ensure!(
            manifest.importer_version == "0.22.0",
            "Unsupported artifact importer version"
        );
        manifest.space.validate()?;
        let EmbeddingSpace::GraniteV1(space) = &manifest.space else {
            anyhow::bail!("Artifact manifest needs complete Granite provenance");
        };
        ensure!(
            space.revision == MODEL_REVISION && space.onnx_sha256 == ONNX_SHA256,
            "Artifact manifest is not the pinned official Granite source"
        );
        for (artifact, filename, digest) in [
            (&manifest.weights, "model.bpk", &space.burnpack_sha256),
            (
                &manifest.tokenizer,
                "tokenizer.json",
                &space.tokenizer_sha256,
            ),
        ] {
            artifact.validate()?;
            ensure!(
                artifact.filename == filename && &artifact.sha256 == digest,
                "Artifact manifest and embedding identity disagree"
            );
            if let Some(url) = &artifact.url {
                ensure!(
                    url == &format!("{RELEASE_BASE}/{filename}"),
                    "Artifact URL is not the pinned release asset"
                );
            }
        }
        ensure!(
            manifest.generated_model_sha256.len() == 64
                && manifest
                    .generated_model_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "Invalid generated model SHA256"
        );
        Ok(manifest)
    }
}

#[derive(Debug)]
pub struct ResolvedArtifacts {
    pub weights_path: PathBuf,
    pub tokenizer_path: PathBuf,
    pub space: EmbeddingSpace,
}

/// Resolve only the compiled, checksum-pinned manifest. Local directories supply
/// bytes, not replacement provenance. Unpublished cache misses fail without a URL.
pub async fn resolve_artifacts(local_override: Option<&Path>) -> Result<ResolvedArtifacts> {
    let manifest = ArtifactManifest::compiled()?;
    let (weights_path, tokenizer_path) = if let Some(directory) = local_override {
        ensure!(
            !tokio::fs::try_exists(directory.join("BUILD_INCOMPLETE")).await?,
            "Artifact build is incomplete"
        );
        (
            verified(
                directory.join(&manifest.weights.filename),
                manifest.weights.sha256.clone(),
            )
            .await?,
            verified(
                directory.join(&manifest.tokenizer.filename),
                manifest.tokenizer.sha256.clone(),
            )
            .await?,
        )
    } else {
        let directory = dirs::cache_dir()
            .context("No cache directory; provide a verified local Granite artifact directory")?
            .join("alexandria/granite-r2-v1");
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(900))
            .build()?;
        let cache = ArtifactCache::new(directory, client);
        (
            cache.fetch(&manifest.weights).await?,
            cache.fetch(&manifest.tokenizer).await?,
        )
    };
    Ok(ResolvedArtifacts {
        weights_path,
        tokenizer_path,
        space: manifest.space,
    })
}

pub struct ArtifactCache {
    directory: PathBuf,
    client: reqwest::Client,
}

impl ArtifactCache {
    pub fn new(directory: PathBuf, client: reqwest::Client) -> Self {
        Self { directory, client }
    }

    /// Every hit is rehashed. A corrupt hit is an error, not a silent repair.
    pub async fn fetch(&self, artifact: &PinnedArtifact) -> Result<PathBuf> {
        artifact.validate()?;
        let path = self.directory.join(&artifact.sha256);
        if tokio::fs::try_exists(&path).await? {
            return verified(path, artifact.sha256.clone()).await;
        }
        tokio::fs::create_dir_all(&self.directory).await?;
        let lock_path = self.directory.join(format!("{}.lock", artifact.sha256));
        let _lock = tokio::task::spawn_blocking(move || -> Result<File> {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_path)?;
            file.lock()?;
            Ok(file)
        })
        .await??;
        if tokio::fs::try_exists(&path).await? {
            return verified(path, artifact.sha256.clone()).await;
        }
        self.download(artifact, &path).await?;
        Ok(path)
    }

    /// Local overrides select paths, never new trusted hashes or model metadata.
    pub async fn local(&self, artifact: &PinnedArtifact, directory: &Path) -> Result<PathBuf> {
        artifact.validate()?;
        verified(directory.join(&artifact.filename), artifact.sha256.clone()).await
    }

    async fn download(&self, artifact: &PinnedArtifact, destination: &Path) -> Result<()> {
        let url = artifact.url.as_deref().context(
            "Granite artifact is not published; provide a checksum-verified local artifact directory"
        )?;
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        let directory = self.directory.clone();
        let temporary =
            tokio::task::spawn_blocking(move || tempfile::NamedTempFile::new_in(directory))
                .await??;
        let mut output = tokio::fs::File::from_std(temporary.reopen()?);
        let mut hash = Sha256::new();
        while let Some(chunk) = response.chunk().await? {
            hash.update(&chunk);
            output.write_all(&chunk).await?;
        }
        output.flush().await?;
        output.sync_all().await?;
        drop(output);
        ensure!(
            format!("{:x}", hash.finalize()) == artifact.sha256,
            "Artifact SHA256 mismatch for {}",
            artifact.filename
        );
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || temporary.persist(destination)).await??;
        Ok(())
    }
}

async fn verified(path: PathBuf, expected: String) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || {
        let mut file =
            File::open(&path).with_context(|| format!("open artifact {}", path.display()))?;
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
        ensure!(
            format!("{:x}", hash.finalize()) == expected,
            "Artifact SHA256 mismatch at {}; remove the corrupt entry to fetch again",
            path.display()
        );
        Ok(path)
    })
    .await?
}
