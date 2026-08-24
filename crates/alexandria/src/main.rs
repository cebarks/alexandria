mod config;

use std::sync::Arc;

use alexandria_mcp::{AlexandriaServer, server::record_id_to_string};
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

    // Check for legacy data dir and advise migration
    let legacy_data = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".alexandria")
        .join("data");
    if legacy_data.exists() && config.database.data_dir != legacy_data {
        tracing::warn!(
            "Legacy data directory found at {}. To migrate, run:\n  \
             mv {} {}",
            legacy_data.display(),
            legacy_data.display(),
            config.database.data_dir.display(),
        );
    }

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
    let activation_config = alexandria_engine::heat::ActivationConfig {
        propagation_factor: config.activation.propagation_factor,
        max_hops: config.activation.max_hops,
    };
    let server = AlexandriaServer::new(
        Arc::new(db),
        Arc::new(embedding),
        config.cluster.join_threshold,
        config.heat.spacing_halflife_secs,
    )
    .with_activation_config(activation_config)
    .with_activation_top_n(config.activation.top_n);

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
    let mut http_config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(config.server.sse_keep_alive_secs)))
        .with_cancellation_token(cancel.clone());

    // Configure host/origin validation from config
    if config.server.allowed_hosts.iter().any(|h| h == "*") {
        http_config = http_config.disable_allowed_hosts();
    } else if !config.server.allowed_hosts.is_empty() {
        http_config = http_config.with_allowed_hosts(config.server.allowed_hosts.clone());
    }
    if config.server.allowed_origins.iter().any(|o| o == "*") {
        http_config = http_config.disable_allowed_origins();
    } else if !config.server.allowed_origins.is_empty() {
        http_config = http_config.with_allowed_origins(config.server.allowed_origins.clone());
    }

    // Spawn cluster maintenance background task
    let maintenance_db = server.db.clone();
    let cohesion_floor = config.cluster.cohesion_floor;
    let merge_threshold = config.cluster.merge_threshold;
    let maintenance_interval_secs = config.cluster.maintenance_interval_secs;
    let maintenance_cancel = cancel.clone();
    tokio::spawn(async move {
        use alexandria_engine::clusters::maintenance::{check_cohesion, check_merge};
        use alexandria_storage::repos::ClusterRepo;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(maintenance_interval_secs));
        loop {
            tokio::select! {
                _ = interval.tick() => {},
                _ = maintenance_cancel.cancelled() => break,
            }
            tracing::debug!("Running cluster maintenance...");
            let cluster_repo = ClusterRepo::new(maintenance_db.inner());

            // Load all clusters with members for cohesion check
            let mut response = match maintenance_db.inner()
                .query("SELECT * FROM cluster")
                .await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("Maintenance: {e}"); continue; }
                };
            let clusters: Vec<alexandria_storage::models::Cluster> = match response.take(0) {
                Ok(c) => c,
                Err(e) => { tracing::warn!("Maintenance: {e}"); continue; }
            };

            for cluster in &clusters {
                let cid = cluster.id.as_ref()
                    .map(record_id_to_string)
                    .unwrap_or_default();
                let members = match cluster_repo.get_members(&cid).await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let member_embeddings: Vec<Vec<f32>> = members.iter()
                    .map(|f| f.embedding.clone()).collect();

                let action = check_cohesion(&cid, &cluster.centroid, &member_embeddings, cohesion_floor);
                if let alexandria_engine::clusters::maintenance::MaintenanceAction::Split {
                    cluster_id, group_a, group_b, centroid_a, centroid_b,
                } = action {
                    tracing::info!("Splitting cluster {cluster_id} into two groups ({} / {} members)",
                        group_a.len(), group_b.len());

                    // Create two new clusters
                    let cid_a = match cluster_repo.create(None, &centroid_a).await {
                        Ok(id) => id,
                        Err(e) => { tracing::warn!("Split: failed to create cluster A: {e}"); continue; }
                    };
                    let cid_b = match cluster_repo.create(None, &centroid_b).await {
                        Ok(id) => id,
                        Err(e) => { tracing::warn!("Split: failed to create cluster B: {e}"); continue; }
                    };

                    // Reassign members — group indices reference `members` vec
                    for &idx in &group_a {
                        if let Some(fact) = members.get(idx) {
                            let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
                            let _ = cluster_repo.remove_member(&cid, &fid).await;
                            let _ = cluster_repo.add_member(&cid_a, &fid).await;
                        }
                    }
                    for &idx in &group_b {
                        if let Some(fact) = members.get(idx) {
                            let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
                            let _ = cluster_repo.remove_member(&cid, &fid).await;
                            let _ = cluster_repo.add_member(&cid_b, &fid).await;
                        }
                    }

                    // Delete the old cluster (now empty)
                    let _ = cluster_repo.delete(&cid).await;
                    tracing::info!("Split complete: {cluster_id} -> {cid_a}, {cid_b}");
                }
            }

            // Check pairs for merge
            let mut infos: Vec<(String, Vec<f32>, usize)> = Vec::new();
            for c in &clusters {
                let id = c.id.as_ref().map(record_id_to_string).unwrap_or_default();
                let count = cluster_repo.get_members(&id).await.map(|m| m.len()).unwrap_or(0);
                infos.push((id, c.centroid.clone(), count));
            }
            for i in 0..infos.len() {
                for j in (i+1)..infos.len() {
                    let result = check_merge(
                        &infos[i].0, &infos[i].1, infos[i].2,
                        &infos[j].0, &infos[j].1, infos[j].2,
                        merge_threshold,
                    );
                    if let alexandria_engine::clusters::maintenance::MergeCheck::Merge {
                        keep_id, remove_id, merged_centroid,
                    } = result {
                        tracing::info!("Merging cluster {remove_id} into {keep_id}");

                        // Move all members from removed cluster to kept cluster
                        let removed_members = match cluster_repo.get_members(&remove_id).await {
                            Ok(m) => m,
                            Err(e) => { tracing::warn!("Merge: {e}"); continue; }
                        };
                        for fact in &removed_members {
                            let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
                            let _ = cluster_repo.remove_member(&remove_id, &fid).await;
                            let _ = cluster_repo.add_member(&keep_id, &fid).await;
                        }

                        // Update kept cluster's centroid to the weighted merge
                        let _ = cluster_repo.update_centroid(&keep_id, &merged_centroid).await;

                        // Delete the now-empty cluster
                        let _ = cluster_repo.delete(&remove_id).await;
                        tracing::info!("Merge complete: {remove_id} -> {keep_id}");

                        // Break out of inner loop — cluster list is stale after mutation
                        break;
                    }
                }
            }
        }
    });

    // Clone `server` for the debug UI router BEFORE it's moved into the MCP service factory
    // closure below — StreamableHttpService::new takes ownership of `server` via `move`.
    let debug_router = alexandria_mcp::debug::router(server.clone());

    let service: StreamableHttpService<AlexandriaServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Default::default(),
            http_config,
        );

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .merge(debug_router);
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!("Alexandria ready, serving HTTP on http://{bind_addr}/mcp (debug UI at http://{bind_addr}/debug)");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            cancel.cancel();
        })
        .await?;

    Ok(())
}
