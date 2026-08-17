mod config;

use std::sync::Arc;

use alexandria_mcp::AlexandriaServer;
use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};
use alexandria_storage::{Database, schema, system_config};
use config::Config;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Alexandria v0.2 starting...");

    // 1. Load configuration
    let config = Config::load()?;
    tracing::info!(
        "Config: transport={}, data_dir={}, model={}",
        config.server.transport,
        config.database.data_dir.display(),
        config.embedding.model,
    );

    // 2. Connect to SurrealDB (persistent or in-memory based on config)
    let db = Database::connect(&config.database.data_dir).await?;
    schema::migrate(db.inner()).await?;

    // 3. Check embedding model safety, then load
    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let embedding =
        CandleProvider::new(&config.embedding.model, &config.embedding.device).await?;
    let dims = embedding.dimensions();
    system_config::check_embedding_model(db.inner(), &config.embedding.model, dims).await?;
    tracing::info!("Embedding model loaded ({dims} dimensions)");

    // 4. Create MCP server
    let server = AlexandriaServer::new(
        Arc::new(db),
        Arc::new(embedding),
        config.cluster.join_threshold,
        config.heat.spacing_halflife_secs,
    );

    // 5. Serve based on transport config
    match config.server.transport.as_str() {
        "stdio" => {
            tracing::info!("Alexandria ready, serving over stdio");
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        "http" => {
            serve_http(server, &config).await?;
        }
        other => {
            anyhow::bail!("Unknown transport: {other}. Use 'stdio' or 'http'.");
        }
    }

    Ok(())
}

async fn serve_http(server: AlexandriaServer, config: &Config) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    };
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();
    let http_config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(15)))
        .with_cancellation_token(cancel.clone())
        .disable_allowed_hosts()
        .disable_allowed_origins();

    let service: StreamableHttpService<AlexandriaServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Default::default(),
            http_config,
        );

    let router = axum::Router::new().nest_service("/mcp", service);
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!("Alexandria ready, serving HTTP on http://{bind_addr}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            cancel.cancel();
        })
        .await?;

    Ok(())
}
