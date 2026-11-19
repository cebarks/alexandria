mod bench;
mod config;
mod dream;
mod dreaming;
mod embedding_migration;
mod ownership;
#[cfg(test)]
mod refinement_boot_tests;

use std::{collections::BTreeMap, sync::Arc};

use alexandria_engine::refinement::policy::InferencePolicyInputs;
use alexandria_mcp::AlexandriaServer;
use alexandria_mcp::refinement::ReconciledRefinement;
use alexandria_pipeline::embedding::load_provider;
use alexandria_storage::access::SystemContext;
use alexandria_storage::ownership_migration::check_ownership_ready;
use alexandria_storage::{Database, schema, system_config};
use anyhow::Context;
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("migrate-embeddings" | "bootstrap-owner")
    ) {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt::init();
    }

    const USAGE: &str = "Usage: alexandria [bootstrap-owner --help | migrate-embeddings [--force] [--output-data-dir PATH [--resume]] | bench-retrieval | dream [--job NAME]... [--max-rows N] | --help]\nGranite requires --output-data-dir; legacy no-output mode remains in-place. Cutover/rollback is manual: change model and data_dir together, never automatically swap or delete stores.";
    if args.first().is_some_and(|first| first == "dream") {
        let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
        return dream::run(&rest).await;
    }
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => {}
        ["migrate-embeddings"] => return migrate_embeddings(false).await,
        ["migrate-embeddings", "--force"] => return migrate_embeddings(true).await,
        ["migrate-embeddings", ..] => {
            let arguments = embedding_migration::parse_migration_args(&args[1..])?;
            let config = Config::load()?;
            let report = embedding_migration::migrate_to_copy(&config, arguments).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            eprintln!(
                "Cutover is manual: set model={}, max_tokens={} and data_dir={} together. Preserve the original model/directory together for rollback; no service was restarted or source deleted.",
                report.target_space.model(),
                report.target_space.max_tokens(),
                report.output_path.display()
            );
            return Ok(());
        }
        ["bootstrap-owner", ..] => return ownership::run(&args[1..]).await,
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
    validate_auth_config(&config)?;
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
    let mut db = Database::connect(&config.database.data_dir).await?;
    system_config::check_boot_embedding_metadata(
        db.inner(),
        &config.embedding.model,
        config.embedding.max_tokens,
    )
    .await
    .with_context(|| {
        format!(
            "Refused startup for data_dir={}. To resume an interrupted owned embedding target, stop the service and run `alexandria migrate-embeddings --output-data-dir PATH --resume` with the original source configured and PATH set to that target",
            config.database.data_dir.display()
        )
    })?;
    check_ownership_ready(db.inner()).await.with_context(|| {
        format!(
            "Refused startup for data_dir={}. For offline legacy ownership migration, stop the service and run `alexandria bootstrap-owner --login LOGIN --backup-data-dir PATH` with the nominated login and verified backup directory (and `--password-file FILE` for a new account)",
            config.database.data_dir.display()
        )
    })?;
    schema::migrate(db.inner()).await?;

    // 2b. Record the instant this build started counting retrievals, before any job can run.
    // `Appraise` refuses to demote a memory older than this stamp: on a store that predates access
    // recording every `access_count` is zero because nothing wrote it, so without the stamp the first
    // pass reads the whole legacy corpus as unused. Stored once, never rewritten — same shape as the
    // embedding-model lock beside it.
    // Arm only after the provider's full identity passes, so a refused boot
    // cannot invalidate an incomplete target's original metadata baseline.

    // 3. Load a ready provider and check its space before index creation/serving.
    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let loaded = load_provider((&config.embedding).into()).await?;
    system_config::check_embedding_space(db.inner(), &loaded.space).await?;
    let armed_at = system_config::arm_access_recording(db.inner()).await?;
    tracing::info!("Access recording armed at {armed_at}");
    let embedding = loaded.provider;
    let dims = embedding.dimensions();
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

    // 3b. Claim this instance's single system capability, in the one order that is safe: connect →
    // embedding admission → migrate → claim → admit requests. Claiming before the embedding admission
    // would put the capability in the hands of a boot that is about to refuse; claiming never at all
    // would leave every background writer to invent its own authority. The value is borrowed by
    // `trusted_local_identity` below, which is the only request-facing use it has, and it stays
    // unreachable from the MCP server itself: `AlexandriaServer` holds `Arc<Database>`, which cannot
    // produce the `&mut` borrow a claim needs.
    //
    // **Carry for Task 12:** `system` is *claimed*, not re-claimable — issuance is once per
    // `Database` and the second claim is refused. Task 12's partitioned maintenance
    // (`maintenance_partitions` / `load_progress`) runs inside the HTTP-mode dreaming scheduler, and it
    // must take this same value by move or by borrow. It must never try to claim again, and it must not
    // hand the capability to anything request-facing: `AlexandriaServer` holds `Arc<Database>`, which
    // structurally cannot produce the `&mut` borrow a claim needs, and that is the boundary the claim
    // exists to hold. Today the only consumer is `trusted_local_identity`, on the stdio arm.
    let system = db.claim_system_authority().map_err(|error| {
        anyhow::anyhow!(
            "could not claim this store's startup authority for data_dir={}: {error:#}",
            config.database.data_dir.display()
        )
    })?;
    let _refinement = reconcile_refinement_at_boot(&db, &system, &config.refinement).await?;
    let db = Arc::new(db);

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
        Arc::clone(&db),
        embedding,
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

    // 4b. The stdio path has no HTTP request to authenticate, so its identity has to be *named* by the
    // operator and issued by storage. An absent `[auth] stdio_account` is not a bypass: nothing is issued,
    // and the transport keeps working as it did before scoped services exist (Task 6 is where an
    // unresolved identity starts refusing work rather than simply not being used).
    let server = match config.server.transport.as_str() {
        "stdio" => match trusted_local_identity(&db, &system, &config).await? {
            Some(identity) => {
                tracing::info!(
                    "stdio transport is serving as the configured local account `[auth] stdio_account`"
                );
                server.with_stdio_identity(identity)
            }
            None => server,
        },
        _ => server,
    };

    // 5. Serve based on transport config
    match config.server.transport.as_str() {
        "stdio" => {
            tracing::info!("Alexandria ready, serving over stdio");
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        "http" => {
            serve_http(server, &config, system).await?;
        }
        other => {
            anyhow::bail!("Unknown transport: {other}. Use 'stdio' or 'http'.");
        }
    }

    Ok(())
}

/// Legacy-only `alexandria migrate-embeddings`: re-embed with the model in config and
/// move the lock. Run with the server stopped; the data dir is single-writer. `--force`
/// re-embeds even when the lock already matches config.
async fn migrate_embeddings(force: bool) -> anyhow::Result<()> {
    use alexandria_mcp::migrate::{ReembedOutcome, reembed};

    tracing::info!(
        "Alexandria v{} migrate-embeddings starting...",
        env!("CARGO_PKG_VERSION")
    );
    let config = Config::load()?;
    anyhow::ensure!(
        config.embedding.model != alexandria_storage::models::embedding::GRANITE_MODEL_ID,
        "Granite requires a separate offline migrated copy: pass --output-data-dir PATH [--resume]; --force alone does not enable the legacy in-place CLI"
    );
    let db = Database::connect(&config.database.data_dir).await?;
    system_config::legacy_reembed_source(db.inner()).await?;
    tracing::info!("Loading embedding model: {}", config.embedding.model);
    let loaded = load_provider((&config.embedding).into()).await?;
    let max_tokens = loaded.effective_max_tokens;
    let embedding = loaded.provider;
    schema::migrate(db.inner()).await?;
    match reembed(
        &db,
        embedding.as_ref(),
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

/**
 * Rejects an `[auth]` setting that cannot mean what the operator meant by it, before anything is opened.
 *
 * The one case is `stdio_account` on the hosted transport. It has no effect there — every hosted request
 * presents its own credential, and reading a configured local identity for a request that presented none
 * is the bypass D2 forbids — and a key that silently does nothing is how a deployment comes to be believed
 * scoped when it is not. Refusing is also the forward-compatible answer: Task 9's hosted configuration
 * surface will want these keys to mean something specific, and a startup that had quietly ignored one
 * would have to be re-explained to whoever set it.
 */
async fn reconcile_refinement_at_boot(
    db: &Database,
    system: &SystemContext,
    config: &config::RefinementConfig,
) -> anyhow::Result<Option<ReconciledRefinement>> {
    // This build has no inference prompt or operation schema. A build that can execute inference
    // must fingerprint the concrete inputs instead, which fences this pre-inference policy ID.
    let settings = BTreeMap::new();
    let inputs = InferencePolicyInputs {
        prompt_template: b"",
        prompt_template_version: "not-installed",
        operation_schema_version: "not-installed",
        settings: &settings,
    };
    ReconciledRefinement::reconcile(
        db,
        system,
        config.resolve()?,
        &inputs,
        config.approval_mode_ceiling,
    )
    .await
    .context("could not reconcile refinement policy at startup")
}

fn validate_auth_config(config: &Config) -> anyhow::Result<()> {
    // An unknown transport is refused first, before either arm below reads the config as one of the two
    // known kinds — and before a store is opened or a model is loaded.
    if !matches!(config.server.transport.as_str(), "stdio" | "http") {
        anyhow::bail!(
            "unknown server.transport \"{}\"; expected \"stdio\" or \"http\"",
            config.server.transport
        );
    }
    if config.server.transport != "stdio" && !config.auth.stdio_account.trim().is_empty() {
        anyhow::bail!(
            "[auth] stdio_account is set but server.transport = \"{}\"; a trusted-local identity is \
            issued only on the stdio transport, where no request can present a credential. Unset it, or \
            run the transport it belongs to.",
            config.server.transport
        );
    }
    // The hosted origin, checked before anything is served and before any store is
    // opened: a wildcard or empty origin list is not a weaker debug UI, it is every account's corpus
    // behind a `Host` header an attacker chooses, and booting to discover that at request time is the
    // wrong order. It sits **after** the two arms above rather than before them because each of those is
    // a config contradiction with its own established refusal, and an operator who set
    // `[auth] stdio_account` on an HTTP transport needs to hear that first — one message per mistake is
    // the difference between a fix and a guessing game.
    let problems = config.server.hosted_origin_problems();
    if !problems.is_empty() {
        anyhow::bail!(
            "hosted HTTP configuration is not safe to serve:\n{}\n{}",
            problems
                .iter()
                .map(|problem| format!("  {problem}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "fix the keys above in [server]; `transport = \"stdio\"` needs none of them"
        );
    }
    Ok(())
}

/**
 * Issues the trusted-local principal the stdio transport acts as, from `[auth] stdio_account`.
 *
 * Two refusals, both deliberate. An unknown login is an error rather than a silent unscoped start: an
 * operator who named an account and got a different regime is worse off than one who is told to fix the
 * config. And the startup capability is *required*, because D2's local identity is exactly
 * "this instance's authority plus an account that exists here" — never "nobody presented a credential".
 * Bootstrapping the named account would be the other way this seam could be wrong, so a fresh store
 * refuses and says what to do instead.
 */
async fn trusted_local_identity(
    db: &Arc<Database>,
    system: &alexandria_storage::access::SystemContext,
    config: &Config,
) -> anyhow::Result<Option<alexandria_mcp::auth::StdioIdentity>> {
    let login = config.auth.stdio_account.trim();
    if login.is_empty() {
        return Ok(None);
    }
    let account = alexandria_storage::repos::AuthRepo::new(db.inner())
        .account_by_login(login)
        .await
        .map_err(|error| anyhow::anyhow!("could not read `[auth] stdio_account`: {error:#}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`[auth] stdio_account` names an account this store does not have; enrol it first. \
                 To enrol an owner offline, use `alexandria bootstrap-owner --help`; startup does not create accounts"
            )
        })?;
    let principal = alexandria_storage::access::AccessRepo::new(db)
        .local_principal(system, &account.account_id()?)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "could not issue the trusted-local identity for `[auth] stdio_account`: {error:#}"
            )
        })?;
    Ok(Some(alexandria_mcp::auth::StdioIdentity::issued(principal)))
}

async fn serve_http(
    server: AlexandriaServer,
    config: &Config,
    system: alexandria_storage::access::SystemContext,
) -> anyhow::Result<()> {
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
    //
    // `liveness` is shared with the dashboard below because a scheduler that quietly stopped is
    // otherwise indistinguishable from one that is idle: `summary()` renders the *configured*
    // cadences, which say the same thing about a healthy loop and about one that panicked itself out.
    let liveness = dreaming::shared_liveness(&config.dreaming);
    if config.dreaming.enabled {
        // The supervisor's own handle is what gets dropped here, deliberately: it exits on `cancel`,
        // which is the HTTP service's own token, so the loop stops when serving stops. Watching it
        // would mean awaiting the server's lifetime from inside the server.
        let _dreaming = dreaming::Jobs::spawn(
            server.db.clone(),
            system.clone(),
            config,
            cancel.clone(),
            liveness.clone(),
        );
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
        // Only when the scheduler is enabled: `None` renders the dashboard's existing
        // "not running" state rather than inventing a fourth one, and a disabled loop has no
        // heartbeat to report.
        dreaming_liveness: config.dreaming.enabled.then(|| Arc::clone(&liveness)),
        // The same list `/mcp` is configured with above, so the debug UI and the MCP endpoint
        // cannot disagree about what a legitimate Host is.
        allowed_hosts: config.server.allowed_hosts.clone(),
    };

    // The body limit the gate enforces is also the limit the SDK enforces while it streams, so one
    // number is written down once and the transport cannot be configured past it by accident.
    http_config = http_config.with_max_request_body_bytes(alexandria_mcp::auth::MAX_BODY_BYTES);
    // Cross-instance session restore stays off until the ownership binding is persisted with it: an
    // external store replays a handshake for a session id another process created, and that id alone is
    // data, not authority. See `AuthMiddleware::refuses_unbound_session_restore`.
    alexandria_mcp::auth::AuthMiddleware::refuses_unbound_session_restore(&http_config)?;

    // The admission stack, in the one order that keeps a single truth about revocation: the bus, then the
    // service that mints leases into it, then the queue and the registry that read the same bus, then the
    // gate over them. `sessions` is shared with the SDK's own session machinery so a revocation can close
    // a session instead of merely marking an entry dead.
    let sessions = Arc::new(LocalSessionManager::default());
    let events = alexandria_mcp::auth::AuthEvents::closing_sessions_in(Arc::clone(&sessions));
    let admission = alexandria_mcp::auth::Admission::bound_to(events.clone());
    let auth_service = Arc::new(alexandria_mcp::auth::AuthService::admitting(
        Arc::clone(&server.db),
        events,
    ));
    let gate = alexandria_mcp::auth::AuthMiddleware::admitting(
        Arc::clone(&auth_service),
        admission,
        alexandria_mcp::auth::McpSessionRegistry::over(&auth_service.events()),
    );

    // The browser boundary, built over the *same* gate, auth service and revocation bus as `/mcp`, so
    // a login is throttled by the same budget, an Argon2 job is bounded by the same semaphore, and a
    // revocation reaches both transports from one publish. Two services would be two answers to "is
    // this session still live", and the one the browser checks would be the one that never learns.
    let policy = alexandria_mcp::debug::WebPolicy::hosted(
        &config.server.public_origin,
        &config.server.allowed_hosts,
        &config.server.allowed_origins,
        config.server.loopback_development,
    )
    .map_err(|problems| {
        anyhow::anyhow!(
            "hosted HTTP configuration is not safe to serve:\n{}",
            problems
                .iter()
                .map(|problem| format!("  {problem}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let web_boundary =
        alexandria_mcp::debug::WebBoundary::new(gate.clone(), Arc::clone(&server.db), policy);
    let debug_router =
        alexandria_mcp::debug::router_secured(server.clone(), Some(debug_ctx), web_boundary);

    let service: StreamableHttpService<AlexandriaServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), sessions, http_config);

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            gate,
            alexandria_mcp::auth::admit,
        ))
        .merge(debug_router);
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!(
        "Alexandria ready, serving HTTP on http://{bind_addr}/mcp (debug UI at http://{bind_addr}/debug)"
    );

    // `ConnectInfo` is what makes the login/invite/recovery attempt budget key on the *peer address*.
    // Without it the handlers would see no peer and fall back to one shared bucket, which is honest but
    // useless against a scripted attacker; and the alternative — reading `X-Forwarded-For` — is a
    // limiter a client sets.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Shutting down...");
        cancel.cancel();
    })
    .await?;

    Ok(())
}

#[cfg(test)]
mod auth_wiring_tests {
    use super::*;

    /// A config for the *identity* arms of `validate_auth_config`.
    ///
    /// An HTTP build here carries a complete hosted-origin trio on purpose: Task 9 made
    /// `server.public_origin` mandatory for `transport = "http"`, so a fixture left at the default would
    /// be refused for a reason this module is not about, and the test would pass or fail on the wrong
    /// key. `config::tests` covers the origin rules themselves.
    fn config(transport: &str, stdio_account: &str) -> Config {
        Config {
            server: config::ServerConfig {
                transport: transport.to_string(),
                public_origin: "https://memory.example.com".to_string(),
                allowed_hosts: vec!["memory.example.com".to_string()],
                allowed_origins: vec!["https://memory.example.com".to_string()],
                ..config::ServerConfig::default()
            },
            auth: config::AuthConfig {
                stdio_account: stdio_account.to_string(),
            },
            ..Config::default()
        }
    }

    /// The default configuration is the stdio transport with no trusted-local identity issued, and it must
    /// start: a guard that refused the shipped default would be a way to force a local operator to name an
    /// account they do not have yet.
    #[test]
    fn the_default_config_needs_no_stdio_identity() {
        validate_auth_config(&config("stdio", ""))
            .expect("the default stdio configuration must start");
    }

    /// Naming a local identity on the hosted transport is refused rather than ignored.
    ///
    /// The refusal is the whole point: on the hosted path every request presents its own credential, so a
    /// key that does nothing there is how a deployment comes to be believed scoped when it is not. The
    /// message names both the key and the setting it conflicts with, which is what an operator greps for.
    #[test]
    fn a_stdio_identity_on_the_hosted_transport_is_refused() {
        let error = validate_auth_config(&config("http", "alice"))
            .expect_err("a hosted stdio_account must not start");
        let message = error.to_string();
        assert!(
            message.contains("[auth] stdio_account") && message.contains("server.transport"),
            "the refusal did not name the keys: {message}"
        );
        // Whitespace-only is the same as unset, because that is how it will be typed.
        validate_auth_config(&config("http", "   ")).expect("blank is unset");
    }

    /// An unknown transport is refused here, before a store is opened or a model is loaded.
    #[test]
    fn an_unknown_transport_is_refused_before_anything_is_opened() {
        let error = validate_auth_config(&config("grpc", ""))
            .expect_err("an unknown transport must not start");
        assert!(
            error.to_string().contains("unknown server.transport"),
            "unexpected refusal: {error}"
        );
    }

    /// The trusted-local resolver is reached only from the stdio arm.
    ///
    /// There is no path from `serve_http` to it, and the type it returns cannot be built by hand: the only
    /// producer is storage's local-principal call, which needs the startup capability this function takes.
    #[test]
    fn the_http_transport_never_calls_the_identity_resolver() {
        let source = include_str!("main.rs");
        let http_arm = source
            .split("fn serve_http(")
            .nth(1)
            .expect("serve_http exists")
            .split("\n}\n")
            .next()
            .unwrap_or_default();
        assert!(
            !http_arm.contains("trusted_local_identity"),
            "serve_http must not issue a trusted-local identity"
        );
        assert!(
            source.contains("\"stdio\" => match trusted_local_identity("),
            "the stdio arm is the only caller"
        );
    }
}
