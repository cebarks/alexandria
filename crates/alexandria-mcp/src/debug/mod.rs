pub mod dashboard;
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
        .with_state(server)
}
