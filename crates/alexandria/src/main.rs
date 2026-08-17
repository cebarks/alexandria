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
    tracing::info!("Alexandria v0.2 starting...");

    // 1. Load configuration
    let config = Config::load()?;
    tracing::info!("Config loaded: data_dir={}, model={}, device={}",
        config.database.data_dir.display(),
        config.embedding.model,
        config.embedding.device,
    );

    // 2. Connect to SurrealDB (persistent or in-memory based on config)
    let db = Database::connect(&config.database.data_dir).await?;
    schema::migrate(db.inner()).await?;

    // 3. Initialize embedding provider
    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let embedding =
        CandleProvider::new(&config.embedding.model, &config.embedding.device).await?;
    tracing::info!("Embedding model loaded ({} dimensions)", embedding.dimensions());

    // 4. Create MCP server
    let server = AlexandriaServer::new(
        Arc::new(db),
        Arc::new(embedding),
        config.cluster.join_threshold,
        config.heat.spacing_halflife_secs,
    );

    // 5. Serve over stdio
    tracing::info!("Alexandria ready, serving over stdio");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
