pub mod assets;
pub mod clusters;
pub mod dashboard;
pub mod graph;
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
    pub maintenance_interval_secs: u64,
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
pub fn router_with_context(server: AlexandriaServer, ctx: Option<DebugContext>) -> Router {
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
}
