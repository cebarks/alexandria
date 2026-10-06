mod bench;
mod config;
mod dreaming;

use std::sync::Arc;

use alexandria_mcp::AlexandriaServer;
use alexandria_pipeline::embedding::{CandleProvider, EmbeddingProvider};
use alexandria_storage::{Database, schema, system_config};
use config::Config;
use rmcp::ServiceExt;

/// Hours that still fit in the duration `do_check_reminders` builds (`i64::MAX`
/// seconds). Past it the window is unrepresentable, and the delivery path holds
/// project reminders instead of escalating them.
const MAX_REPRESENTABLE_ESCALATION_HOURS: u64 = (i64::MAX / 3600) as u64;

/// Below the representable bound, still so far out that "escalates after this" is
/// indistinguishable from "never": the `set_reminder` promise that a project
/// reminder is never silently lost stops being meaningful. Warned rather than
/// rejected — someone may genuinely want a decade-long hold.
const MAX_PRACTICAL_ESCALATION_HOURS: u64 = 24 * 365 * 10;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    const USAGE: &str =
        "Usage: alexandria [migrate-embeddings [--force] | bench-retrieval | --help]";
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => {}
        ["migrate-embeddings"] => return migrate_embeddings(false).await,
        ["migrate-embeddings", "--force"] => return migrate_embeddings(true).await,
        ["bench-retrieval"] => return bench::run().await,
        ["--help"] | ["-h"] => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => anyhow::bail!("unexpected arguments {args:?}. {USAGE}"),
    }
    tracing::info!("Alexandria v{} starting...", env!("CARGO_PKG_VERSION"));

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

    // 2b. Record the instant this build started counting retrievals, before any job can run.
    // `Appraise` refuses to demote a memory older than this stamp: on a store that predates access
    // recording every `access_count` is zero because nothing wrote it, so without the stamp the first
    // pass reads the whole legacy corpus as unused. Stored once, never rewritten — same shape as the
    // embedding-model lock beside it.
    let armed_at = system_config::arm_access_recording(db.inner()).await?;
    tracing::info!("Access recording armed at {armed_at}");

    // 3. Check embedding model safety, then load
    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let embedding = CandleProvider::new(
        &config.embedding.model,
        &config.embedding.device,
        config.embedding.max_tokens,
    )
    .await?;
    let dims = embedding.dimensions();
    // The provider's limit, not config's: the lock records what the vectors were made with.
    system_config::check_embedding_model(
        db.inner(),
        &config.embedding.model,
        dims,
        embedding.max_tokens(),
    )
    .await?;
    // A failed define must not stop boot: the brute-force KNN form returns the same rows, and
    // the backfill aborts on any row of another dimension — which is exactly the state a
    // reverted, half-finished `migrate-embeddings` leaves behind.
    let vector_index = match schema::ensure_vector_index(db.inner(), dims).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                "Could not define the HNSW index; retrieval falls back to a full scan: {e:#}"
            );
            false
        }
    };
    tracing::info!("Embedding model loaded ({dims} dimensions)");

    // 4. Create MCP server
    let activation_config = alexandria_engine::heat::ActivationConfig {
        propagation_factor: config.activation.propagation_factor,
        max_hops: config.activation.max_hops,
    };

    // Resolve reminders timezone: config value, else system-local, else UTC
    let tz_name = if config.reminders.timezone.is_empty() {
        iana_time_zone::get_timezone().unwrap_or_else(|e| {
            tracing::warn!("Could not detect system timezone ({e}); using UTC for reminders");
            "UTC".to_string()
        })
    } else {
        config.reminders.timezone.clone()
    };
    let tz: chrono_tz::Tz = tz_name.parse().map_err(|e| {
        anyhow::anyhow!("invalid [reminders].timezone `{tz_name}` (expected IANA name like 'Europe/Stockholm'): {e}")
    })?;
    // `do_check_reminders` turns this window into a duration, and beyond
    // `i64::MAX` seconds it cannot: the delivery path then *holds* project
    // reminders forever, on every check, with only a warning line to show for it.
    // Refuse to start instead — the same posture as `[reminders].timezone` and the
    // embedding-model lock, which both fail fast rather than degrading quietly.
    let escalation_hours = config.reminders.escalation_hours;
    if escalation_hours > MAX_REPRESENTABLE_ESCALATION_HOURS {
        anyhow::bail!(
            "[reminders].escalation_hours = {escalation_hours} exceeds the largest representable              duration ({MAX_REPRESENTABLE_ESCALATION_HOURS}h); project reminders could never escalate"
        );
    }
    if escalation_hours > MAX_PRACTICAL_ESCALATION_HOURS {
        tracing::warn!(
            "[reminders].escalation_hours = {escalation_hours} is over              {MAX_PRACTICAL_ESCALATION_HOURS}h (~10 years): project reminders will effectively              never escalate — is that intended?"
        );
    }
    tracing::info!("Reminders timezone: {tz}, escalation: {escalation_hours}h");

    let server = AlexandriaServer::new(
        Arc::new(db),
        Arc::new(embedding),
        config.cluster.join_threshold,
        alexandria_mcp::server::HeatSettings {
            decay_tau_secs: config.heat.decay_tau_secs,
            spacing_reference_secs: config.heat.spacing_reference_secs,
        },
    )
    .with_activation_config(activation_config)
    .with_activation_top_n(config.activation.top_n)
    .with_retrieve_min_similarity(config.retrieve.min_similarity)
    .with_cohesion_floor(config.cluster.cohesion_floor)
    .with_vector_index(vector_index)
    .with_reminders_config(alexandria_mcp::server::RemindersSettings {
        tz,
        escalation_hours: config.reminders.escalation_hours,
    });

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

/// `alexandria migrate-embeddings`: re-embed everything with the model in config and
/// move the lock. Run with the server stopped; the data dir is single-writer. `--force`
/// re-embeds even when the lock already matches config.
async fn migrate_embeddings(force: bool) -> anyhow::Result<()> {
    use alexandria_mcp::migrate::{ReembedOutcome, reembed};

    tracing::info!(
        "Alexandria v{} migrate-embeddings starting...",
        env!("CARGO_PKG_VERSION")
    );
    let config = Config::load()?;
    let db = Database::connect(&config.database.data_dir).await?;
    schema::migrate(db.inner()).await?;

    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let embedding = CandleProvider::new(
        &config.embedding.model,
        &config.embedding.device,
        config.embedding.max_tokens,
    )
    .await?;

    let max_tokens = embedding.max_tokens();
    match reembed(
        &db,
        &embedding,
        config.embedding.batch_size,
        max_tokens,
        force,
    )
    .await?
    {
        ReembedOutcome::Skipped(why) => println!("Nothing to do: {why}"),
        ReembedOutcome::Done { facts, clusters } => println!(
            "Re-embedded {facts} facts and {clusters} cluster centroids with {} ({} dims, {max_tokens} tokens). Restart the service.",
            embedding.model_id(),
            embedding.dimensions()
        ),
    }
    Ok(())
}

async fn serve_http(server: AlexandriaServer, config: &Config) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();
    let mut http_config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(
            config.server.sse_keep_alive_secs,
        )))
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

    // Spawn the dreaming scheduler: one loop, five independently-due jobs (#43). It replaces the
    // inline cluster-maintenance spawn that used to live here, which had a single interval for two
    // distinct phases and no way to add a third job without retiming the first two.
    if config.dreaming.enabled {
        // The handle is deliberately dropped: the task is detached and exits on `cancel`, which is
        // the HTTP service's own token, so the loop stops when serving stops.
        let _dreaming = dreaming::Jobs::spawn(server.db.clone(), config, cancel.clone());
    } else {
        tracing::info!("dreaming scheduler disabled by [dreaming] enabled = false");
    }

    // Clone `server` for the debug UI router BEFORE it's moved into the MCP service factory
    // closure below — StreamableHttpService::new takes ownership of `server` via `move`.
    // The context is built from this same `Config`, so the dashboard cannot report a value
    // the server was not started with.
    let debug_ctx = alexandria_mcp::debug::DebugContext {
        embedding_model: config.embedding.model.clone(),
        embedding_device: config.embedding.device.clone(),
        transport: config.server.transport.clone(),
        bind_host: config.server.host.clone(),
        bind_port: config.server.port,
        data_dir: config.database.data_dir.display().to_string(),
        cluster_merge_threshold: config.cluster.merge_threshold,
        dreaming_summary: dreaming::summary(&config.dreaming),
        // The same list `/mcp` is configured with above, so the debug UI and the MCP endpoint
        // cannot disagree about what a legitimate Host is.
        allowed_hosts: config.server.allowed_hosts.clone(),
    };
    let debug_router = alexandria_mcp::debug::router_with_context(server.clone(), Some(debug_ctx));

    let service: StreamableHttpService<AlexandriaServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), http_config);

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .merge(debug_router);
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!(
        "Alexandria ready, serving HTTP on http://{bind_addr}/mcp (debug UI at http://{bind_addr}/debug)"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            cancel.cancel();
        })
        .await?;

    Ok(())
}
