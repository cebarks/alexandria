pub mod clusters;
pub mod dashboard;
pub mod graph;
pub mod html;
pub mod maintenance;
pub mod memories;
pub mod query;

#[cfg(test)]
mod test_support;

use axum::routing::{get, post};
use axum::Router;

use crate::AlexandriaServer;

pub fn router(server: AlexandriaServer) -> Router {
    Router::new()
        .route("/debug", get(dashboard::handler))
        .route("/debug/memories", get(memories::list))
        .route("/debug/memories/{id}", get(memories::detail))
        .route("/debug/clusters", get(clusters::list))
        .route("/debug/clusters/{id}", get(clusters::detail))
        .route("/debug/graph/{id}", get(graph::page))
        .route("/debug/api/graph/{id}", get(graph::api_graph))
        .route("/debug/maintenance", get(maintenance::list))
        .route("/debug/query", get(query::form))
        .route("/debug/query/run", post(query::run))
        .with_state(server)
}
