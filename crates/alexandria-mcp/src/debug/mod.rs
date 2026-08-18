pub mod clusters;
pub mod dashboard;
pub mod graph;
pub mod html;
pub mod memories;

#[cfg(test)]
mod test_support;

use axum::routing::get;
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
        .with_state(server)
}
