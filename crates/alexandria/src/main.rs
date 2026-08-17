mod config;

use std::sync::Arc;

use alexandria_mcp::AlexandriaServer;
use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};
use alexandria_storage::{Database, schema};
use config::Config;
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Alexandria v0.1 starting...");

    let config = Config::default();

    // 1. Connect to embedded SurrealDB
    let db = Database::connect_embedded().await?;
    schema::bootstrap(db.inner()).await?;

    // 2. Initialize embedding provider
    tracing::info!("Loading embedding model: {}", config.embedding_model);
    let embedding =
        CandleProvider::new(&config.embedding_model, &config.embedding_device).await?;
    tracing::info!("Embedding model loaded ({} dimensions)", embedding.dimensions());

    // 3. Create MCP server
    let server = AlexandriaServer::new(
        Arc::new(db),
        Arc::new(embedding),
        config.cluster_join_threshold,
        config.heat_spacing_halflife,
    );

    // 4. Serve over stdio
    tracing::info!("Alexandria ready, serving over stdio");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
