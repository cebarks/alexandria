//! One model-owning OS thread, one active job and one queued job.

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};

struct Job {
    text: String,
    reply: oneshot::Sender<Result<Vec<f32>>>,
}

#[derive(Clone)]
pub struct Worker {
    sender: mpsc::Sender<Job>,
}

/// Dropping a queued result cancels its job. An already active forward finishes.
pub struct PendingEmbedding {
    receiver: oneshot::Receiver<Result<Vec<f32>>>,
}

impl Worker {
    /// Initialize and validate on the model's thread before reporting readiness.
    /// The encoder need not be Send: it is constructed and retained on that thread.
    pub async fn start<F, E>(initialize: F) -> Result<Self>
    where
        F: FnOnce() -> Result<E> + Send + 'static,
        E: FnMut(&str) -> Result<Vec<f32>> + 'static,
    {
        let (sender, mut jobs) = mpsc::channel::<Job>(1);
        let (ready, loaded) = oneshot::channel();
        std::thread::Builder::new()
            .name("alexandria-granite".into())
            .spawn(move || {
                let mut encode = match initialize() {
                    Ok(encode) => encode,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = jobs.blocking_recv() {
                    if job.reply.is_closed() {
                        continue;
                    }
                    let result = encode(&job.text);
                    let _ = job.reply.send(result);
                }
            })
            .context("spawn Granite embedding worker")?;
        loaded
            .await
            .context("Granite worker stopped during initialization")??;
        Ok(Self { sender })
    }

    /// Await admission with bounded backpressure, then own the cancellable result.
    pub async fn submit(&self, text: &str) -> Result<PendingEmbedding> {
        let permit = self
            .sender
            .reserve()
            .await
            .map_err(|_| anyhow::anyhow!("Granite embedding worker stopped"))?;
        let (reply, receiver) = oneshot::channel();
        permit.send(Job {
            text: text.to_owned(),
            reply,
        });
        Ok(PendingEmbedding { receiver })
    }

    pub async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut embeddings = Vec::with_capacity(texts.len());
        for text in texts {
            embeddings.push(self.submit(text).await?.receive().await?);
        }
        Ok(embeddings)
    }
}

impl PendingEmbedding {
    pub async fn receive(self) -> Result<Vec<f32>> {
        self.receiver
            .await
            .context("Granite embedding worker stopped")?
    }
}
