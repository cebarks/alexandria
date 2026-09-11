//! Minimal HuggingFace Hub file fetcher. Reads and writes the standard
//! `hub/models--owner--name/{refs/main,snapshots/<sha>/...}` cache layout so
//! caches written by hf-hub / huggingface_hub are reused as-is.
//!
//! ponytail: files are written straight into `snapshots/<sha>/`, with no `blobs/`
//! symlink, `.no_exist` marker, or lock files. Every reader we care about
//! only looks at the snapshot path. Add the rest if a second consumer needs it.
//!
//! ponytail: revision `main` only, no `HF_TOKEN`, no `HF_ENDPOINT`. Consequences: a
//! cached revision is served forever (delete the repo dir to re-fetch); a model
//! lacking `1_Pooling/config.json` pays one 404 per online boot and falls to the
//! warn-and-assume-mean path offline (every sentence-transformers repo ships it, so
//! this never fires today); two servers first-booting on the same empty cache both
//! download (rename-into-place keeps the result correct); gated or private models
//! cannot be fetched. Add whichever one actually bites.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HUB_URL: &str = "https://huggingface.co";

fn cache_root() -> PathBuf {
    if let Some(p) = std::env::var_os("HF_HUB_CACHE") {
        return p.into();
    }
    if let Some(p) = std::env::var_os("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    std::env::home_dir()
        .unwrap_or_default()
        .join(".cache/huggingface/hub")
}

/// `snapshots/<sha>/<file>` under `repo_dir` if `refs/main` exists there.
fn cached_path(repo_dir: &Path, file: &str) -> Option<PathBuf> {
    let sha = std::fs::read_to_string(repo_dir.join("refs/main")).ok()?;
    Some(repo_dir.join("snapshots").join(sha.trim()).join(file))
}

/// Resolve `file` of `model_id` (`owner/name`) at revision `main` to a local path.
/// Cache first; only a miss touches the network. `Ok(None)` means the Hub has no
/// such file (HTTP 404).
pub async fn fetch(model_id: &str, file: &str) -> Result<Option<PathBuf>> {
    let repo_dir = cache_root().join(format!("models--{}", model_id.replace('/', "--")));

    if let Some(p) = cached_path(&repo_dir, file)
        && p.exists()
    {
        return Ok(Some(p));
    }

    let client = reqwest::Client::new();
    let path = match cached_path(&repo_dir, file) {
        Some(p) => p,
        None => {
            let info = client
                .get(format!("{HUB_URL}/api/models/{model_id}"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let info: serde_json::Value = serde_json::from_str(&info)
                .with_context(|| format!("bad model info for {model_id}"))?;
            let sha = info["sha"]
                .as_str()
                .with_context(|| format!("no sha in model info for {model_id}"))?;
            std::fs::create_dir_all(repo_dir.join("refs"))?;
            std::fs::write(repo_dir.join("refs/main"), sha)?;
            repo_dir.join("snapshots").join(sha).join(file)
        }
    };

    let url = format!("{HUB_URL}/{model_id}/resolve/main/{file}");
    tracing::info!("Downloading {url}");
    let resp = client.get(&url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body = resp.error_for_status()?.bytes().await?;

    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::cached_path;

    #[test]
    fn cached_path_follows_refs_main() {
        let dir = std::env::temp_dir().join(format!("alexandria-hub-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("refs")).unwrap();
        std::fs::write(dir.join("refs/main"), "abc123\n").unwrap();
        assert_eq!(
            cached_path(&dir, "1_Pooling/config.json").unwrap(),
            dir.join("snapshots/abc123/1_Pooling/config.json")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_path_none_without_refs() {
        assert!(cached_path(std::path::Path::new("/nonexistent/repo"), "config.json").is_none());
    }
}
