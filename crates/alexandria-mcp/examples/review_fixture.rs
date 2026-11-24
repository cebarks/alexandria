//! Disposable localhost fixture for manual refinement review. No application config is loaded.

use std::sync::Arc;

use alexandria_mcp::AlexandriaServer;
use alexandria_mcp::auth::{
    Admission, AuthEvents, AuthMiddleware, AuthService, McpSessionRegistry,
};
use alexandria_mcp::debug::{WebBoundary, WebPolicy, router_secured};
use alexandria_mcp::server::HeatSettings;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::access::{AccessRepo, OwnerContext, SystemContext};
use alexandria_storage::models::OperationId;
use alexandria_storage::repos::{AuthRepo, IdentityRepo};
use alexandria_storage::{Database, schema};
use anyhow::Result;
use axum::Router;
use tokio::net::TcpListener;

#[path = "review_fixture/seed.rs"]
mod seed;

struct FixtureEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for FixtureEmbedding {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let offset = (text.len() % 7) as f32 / 1000.0;
                vec![1.0, offset]
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "review-fixture-embedding"
    }
}

struct ReviewFixture {
    origin: String,
    listener: TcpListener,
    router: Router,
    auth: Arc<AuthService>,
    login: String,
    password: String,
    opinion_attempt: String,
    regenerate_attempt: String,
}

impl ReviewFixture {
    async fn build() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let policy = WebPolicy::hosted(
            &origin,
            &["127.0.0.1".to_owned()],
            std::slice::from_ref(&origin),
            true,
        )
        .map_err(|err| anyhow::anyhow!("fixture web policy: {err:?}"))?;
        let mut db = Database::connect_embedded().await?;
        schema::migrate(db.inner()).await?;
        let system = db.claim_system_authority()?;
        let db = Arc::new(db);
        let login_id = uuid::Uuid::new_v4().simple().to_string();
        let login = format!("fixture-{}", &login_id[..12]);
        let password = format!("fixture-{}", uuid::Uuid::new_v4());
        let digest = AuthRepo::create_password_digest(&password).await?;
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                &login,
                digest.as_str(),
                &OperationId::new("review-fixture-bootstrap"),
            )
            .await?;
        let access = AccessRepo::new(&db);
        let principal = access.local_principal(&system, &ids.account).await?;
        let owner = access.authorize(&principal).await?;
        let seed::Seeded {
            config,
            opinion_attempt,
            regenerate_attempt,
        } = seed::review_scenarios(&db, &system, &owner, &origin).await?;

        let events = AuthEvents::without_sessions();
        let auth = Arc::new(AuthService::admitting(Arc::clone(&db), events.clone()));
        let gate = AuthMiddleware::admitting(
            Arc::clone(&auth),
            Admission::bound_to(events.clone()),
            McpSessionRegistry::over(&events),
        );
        let server = AlexandriaServer::new(
            Arc::clone(&db),
            Arc::new(FixtureEmbedding),
            0.75,
            HeatSettings::default(),
        )
        .with_refinement_runtime(Some(Arc::new(config)));
        let router = router_secured(server, None, WebBoundary::new(gate, db, policy));
        Ok(Self {
            origin,
            listener,
            router,
            auth,
            login,
            password,
            opinion_attempt,
            regenerate_attempt,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let fixture = ReviewFixture::build().await?;
    // Authentication is still real; the only revealed secret is scoped to this ephemeral process.
    println!("REVIEW_FIXTURE_URL={}/debug/login", fixture.origin);
    println!("REVIEW_FIXTURE_LOGIN={}", fixture.login);
    println!("REVIEW_FIXTURE_PASSWORD={}", fixture.password);
    println!(
        "REVIEW_FIXTURE_OPINION_URL={}/debug/refinement/attempts/{}",
        fixture.origin, fixture.opinion_attempt
    );
    println!(
        "REVIEW_FIXTURE_REGENERATE_URL={}/debug/refinement/attempts/{}",
        fixture.origin, fixture.regenerate_attempt
    );
    use std::io::Write as _;
    std::io::stdout().flush()?;
    let _auth = fixture.auth;
    axum::serve(fixture.listener, fixture.router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alexandria_mcp::debug::SESSION_COOKIE;
    use axum::{body, http};
    use tower::ServiceExt;

    #[tokio::test]
    async fn owner_queue_has_review_scenarios() -> Result<()> {
        let fixture = ReviewFixture::build().await?;
        let grant = fixture
            .auth
            .login(&fixture.login, &fixture.password, chrono::Utc::now())
            .await?;
        let request = http::Request::builder()
            .uri("/debug/refinement")
            .header("host", fixture.listener.local_addr()?.to_string())
            .header(
                "cookie",
                format!("{SESSION_COOKIE}={}", grant.expose_cookie()),
            )
            .body(body::Body::empty())?;
        let response = fixture.router.oneshot(request).await?;
        assert_eq!(
            response.status(),
            http::StatusCode::OK,
            "fixture owner can load secured queue"
        );
        let bytes = body::to_bytes(response.into_body(), 512 * 1024).await?;
        let page = std::str::from_utf8(&bytes)?;
        assert!(page.contains("Quality refinement"));
        assert!(
            page.contains("Fixture review: approve and rollback"),
            "Task 23 RED: seed browser review scenarios in the real owner queue"
        );
        Ok(())
    }
}
