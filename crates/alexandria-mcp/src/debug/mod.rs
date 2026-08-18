pub mod dashboard;
pub mod html;

#[cfg(test)]
mod test_support;

use axum::routing::get;
use axum::Router;

use crate::AlexandriaServer;

pub fn router(server: AlexandriaServer) -> Router {
    Router::new()
        .route("/debug", get(dashboard::handler))
        .with_state(server)
}
