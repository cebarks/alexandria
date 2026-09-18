//! Minimal HuggingFace Hub file fetcher. Reads and writes the standard
//! `hub/models--owner--name/{refs/main,snapshots/<sha>/...}` cache layout so
//! caches written by hf-hub / huggingface_hub are reused as-is.
//!
//! ponytail: files are written straight into `snapshots/<sha>/`, with no `blobs/`
//! symlink or `.no_exist` marker. Every reader we care about only looks at the
//! snapshot path. Add the rest if a second consumer needs it.
//!
//! Concurrency: a cache miss takes an advisory lock on `<repo>/.lock` and re-checks
//! the cache under it, so concurrent fetchers (threads or processes) download each
//! file once and the rest wait and then hit the cache. The body is streamed into a
//! uniquely named temp file beside the destination and renamed into place, so a
//! reader never sees a partial file and memory does not track model size.
//!
//! ponytail: one lock per repo, not per file. Fetches are a handful of files at
//! boot; go per-file if that stops being true.
//!
//! ponytail: revision `main` only, no `HF_TOKEN`, no `HF_ENDPOINT`. Consequences: a
//! cached revision is served forever (delete the repo dir to re-fetch); a model
//! lacking `1_Pooling/config.json` pays one 404 per online boot and falls to the
//! warn-and-assume-mean path offline (every sentence-transformers repo ships it, so
//! this never fires today); gated or private models cannot be fetched. Add whichever
//! one actually bites.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HUB_URL: &str = "https://huggingface.co";

/// Same precedence as huggingface_hub: `HF_HUB_CACHE`, `HF_HOME`, `XDG_CACHE_HOME`,
/// `HOME`. Unset and empty are the same. With none of them there is no sane place to
/// put ~90 MB of weights, so this errors instead of caching relative to the CWD.
fn cache_root(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let var = |k| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(p) = var("HF_HUB_CACHE") {
        return Ok(p);
    }
    if let Some(p) = var("HF_HOME") {
        return Ok(p.join("hub"));
    }
    if let Some(p) = var("XDG_CACHE_HOME") {
        return Ok(p.join("huggingface/hub"));
    }
    let home = var("HOME").context(
        "none of HF_HUB_CACHE, HF_HOME, XDG_CACHE_HOME or HOME is set; \
         set one so the embedding model has a cache directory",
    )?;
    Ok(home.join(".cache/huggingface/hub"))
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
    let repo_dir = cache_root(|k| std::env::var_os(k))?
        .join(format!("models--{}", model_id.replace('/', "--")));

    if let Some(p) = cached_path(&repo_dir, file)
        && p.exists()
    {
        return Ok(Some(p));
    }

    // Miss: serialize with every other fetcher of this repo, then look again, since
    // whoever held the lock before us has probably just downloaded the file.
    std::fs::create_dir_all(&repo_dir)?;
    let lock = std::fs::File::create(repo_dir.join(".lock"))?;
    let _lock = tokio::task::spawn_blocking(move || lock.lock().map(|()| lock)).await??;
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
    let mut resp = resp.error_for_status()?;

    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    while let Some(chunk) = resp.chunk().await? {
        tmp.write_all(&chunk)?;
    }
    tmp.persist(&path)?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::{cache_root, cached_path};
    use std::path::PathBuf;

    fn root(vars: &[(&str, &str)]) -> anyhow::Result<PathBuf> {
        cache_root(|k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).into()))
    }

    #[test]
    fn cache_root_precedence() {
        let all = [
            ("HF_HUB_CACHE", "/a"),
            ("HF_HOME", "/b"),
            ("XDG_CACHE_HOME", "/c"),
            ("HOME", "/d"),
        ];
        assert_eq!(root(&all).unwrap(), PathBuf::from("/a"));
        assert_eq!(root(&all[1..]).unwrap(), PathBuf::from("/b/hub"));
        assert_eq!(
            root(&all[2..]).unwrap(),
            PathBuf::from("/c/huggingface/hub")
        );
        assert_eq!(
            root(&all[3..]).unwrap(),
            PathBuf::from("/d/.cache/huggingface/hub")
        );
    }

    #[test]
    fn cache_root_errors_rather_than_going_cwd_relative() {
        assert!(root(&[]).is_err());
        assert!(root(&[("HOME", "")]).is_err());
    }

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
