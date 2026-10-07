pub mod assets;
pub mod clusters;
pub mod dashboard;
pub mod graph;
mod guard;
pub mod html;
pub mod maintenance;
pub mod memories;
pub mod query;
pub mod sessions;

#[cfg(test)]
mod test_support;

use axum::Extension;
use axum::Router;
use axum::routing::{get, post};

use crate::AlexandriaServer;

/// Read-only snapshot of resolved configuration, for the debug dashboard only.
///
/// Deliberately *not* fields on [`AlexandriaServer`]: that struct is the MCP tool surface and
/// carries only what the algorithms need (`retrieve_min_similarity`, `activation_top_n`,
/// `cohesion_floor`, …). Everything here is display-only, and some of it — transport, bind
/// address — is meaningless inside a tool handler that is already running. Populated in
/// `main.rs` from the same `Config` that built the server, so it cannot disagree with what the
/// server was given.
#[derive(Debug, Clone)]
pub struct DebugContext {
    /// `[embedding] model` as *asked for*. The dashboard shows this next to
    /// `EmbeddingProvider::model_id()`, which reports what was actually loaded — with the
    /// model locked on first boot, the gap between the two is the whole story.
    pub embedding_model: String,
    pub embedding_device: String,
    pub transport: String,
    pub bind_host: String,
    pub bind_port: u16,
    pub data_dir: String,
    pub cluster_merge_threshold: f32,
    /// The whole `[dreaming]` cadence as one line, built by the binary that read the file.
    ///
    /// A summary string rather than five numeric fields: the dashboard renders one row per key and
    /// the five jobs are only meaningful together, so six `DebugContext` fields to display one
    /// sentence would be plumbing for its own sake. Built in the binary because `DreamingConfig`
    /// lives there and cannot reach this crate — the same reason `ClusterConfig` values arrive
    /// pre-flattened.
    pub dreaming_summary: String,
    /// The scheduler's heartbeat, shared with the loop that writes it. `None` when
    /// `[dreaming] enabled = false`, which the dashboard renders as "not running" rather than as
    /// the no-context `UNAVAILABLE` — "turned off" and "this process cannot tell you" are different
    /// answers and collapsing them would be a lie.
    ///
    /// The type lives in `alexandria-engine` because neither the binary nor this crate can own it:
    /// `alexandria-mcp` cannot name a type defined in a binary crate, which is the same constraint
    /// that forces the cadences above to arrive pre-flattened.
    pub dreaming_liveness: Option<std::sync::Arc<alexandria_engine::dreaming::Liveness>>,
    /// `[server] allowed_hosts`, verbatim.
    ///
    /// The debug UI has no auth, so its only server-side signal that a request is not what it
    /// claims to be is the `Host` header — see `debug/guard.rs`. Empty or wildcarded means the
    /// Host check is off, which keeps the shipped default (`["*"]`) behaving exactly as it did
    /// before.
    pub allowed_hosts: Vec<String>,
}

/// Existing entry point, unchanged: tests and stdio mode use this, and the dashboard's
/// config-dependent rows render their "unavailable" state.
pub fn router(server: AlexandriaServer) -> Router {
    router_with_context(server, None)
}

/// HTTP mode passes the resolved configuration so the dashboard can show it.
///
/// `ctx` travels as a request `Extension` layered on the dashboard route alone. The
/// alternative — a wider `State` struct — would rewrite the extractor signature of every
/// handler on the strength of one page needing one extra value.
///
/// **Every debug route must be added here**, below the `guard` layer: the layer is applied to
/// the routes registered above it, so a route bolted on after `.layer()` — or served by a
/// different router that merges this one — silently escapes both checks. `guard.rs`'s
/// `test_every_debug_route_is_guarded` is the tripwire for exactly that.
pub fn router_with_context(server: AlexandriaServer, ctx: Option<DebugContext>) -> Router {
    // The Host half is configured; the CSRF half is not, on purpose. Deriving it from `ctx` being
    // `Some` would mean every caller of `router()` — stdio mode, and all 60+ tests — got the
    // weaker build without anyone having to decide that.
    let allowed_hosts = ctx
        .as_ref()
        .map(|ctx| guard::Guard {
            allowed_hosts: ctx.allowed_hosts.clone(),
        })
        .unwrap_or_default();

    Router::new()
        .route("/debug", get(dashboard::handler).layer(Extension(ctx)))
        .route("/debug/assets/{name}", get(assets::asset))
        .route("/debug/memories", get(memories::list))
        .route("/debug/memories/{id}", get(memories::detail))
        .route("/debug/clusters", get(clusters::list))
        .route("/debug/clusters/{id}", get(clusters::detail))
        .route("/debug/sessions", get(sessions::list))
        .route("/debug/sessions/{external_id}", get(sessions::detail))
        .route("/debug/graph/{id}", get(graph::page))
        .route("/debug/api/graph/{id}", get(graph::api_graph))
        .route("/debug/maintenance", get(maintenance::list))
        .route("/debug/query", get(query::form))
        .route("/debug/query/run", post(query::run))
        .with_state(server)
        // Over the whole router, after state is applied, so it covers `/debug/assets/*` and the
        // 403-on-GET case of the rebinding attack just as thoroughly as the CSRF-carrying POST.
        .layer(axum::middleware::from_fn_with_state(
            allowed_hosts,
            guard::enforce,
        ))
}
