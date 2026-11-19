use std::sync::Arc;

use alexandria_mcp::auth::admission::Admission;
use alexandria_mcp::auth::http::AuthMiddleware;
use alexandria_mcp::auth::mcp_transport::{AuthEvents, McpSessionRegistry};
use alexandria_mcp::auth::service::AuthService;
use alexandria_mcp::debug::{self, WebBoundary, WebPolicy};
use alexandria_mcp::server::{AlexandriaServer, HeatSettings};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::models::identity::{AccountId, OperationId, ProjectId, WorkspaceId};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, NewReminder, ReminderRepo};
use alexandria_storage::{Database, parse_record_id, schema};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_TYPE, COOKIE, HOST};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use chrono::{DateTime, Duration, Utc};
use surrealdb::types::{RecordId, SurrealValue};
use tower::ServiceExt;

const HOST_VALUE: &str = "mapping.example.test";
const ORIGIN: &str = "https://mapping.example.test";
const PASSWORD: &str = "reminder-mapping-test-password";

struct Fixture {
    router: Router,
    db: Arc<Database>,
    account: AccountId,
    workspace: WorkspaceId,
    owner: alexandria_storage::access::OwnerContext,
    cookie: String,
    csrf: String,
}

struct StubEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for StubEmbedding {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "reminder-mapping-test"
    }
}

async fn fixture(login: &str) -> Fixture {
    let mut db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let _system = db.claim_system_authority().unwrap();
    let db = Arc::new(db);
    let password_digest = AuthRepo::create_password_digest(PASSWORD).await.unwrap();
    let ids = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            login,
            password_digest.as_str(),
            &OperationId::new(format!("mapping-bootstrap-{login}")),
        )
        .await
        .unwrap();

    let events = AuthEvents::without_sessions();
    let service = Arc::new(AuthService::admitting(Arc::clone(&db), events.clone()));
    let middleware = AuthMiddleware::admitting(
        Arc::clone(&service),
        Admission::bound_to(events),
        McpSessionRegistry::over(&service.events()),
    );
    let policy = WebPolicy::hosted(
        ORIGIN,
        &[HOST_VALUE.to_string()],
        &[ORIGIN.to_string()],
        false,
    )
    .unwrap();
    let boundary = WebBoundary::new(middleware, Arc::clone(&db), policy);
    let server = AlexandriaServer::new(
        Arc::clone(&db),
        Arc::new(StubEmbedding),
        0.75,
        HeatSettings::default(),
    );
    let router = debug::router_secured(server, None, boundary);
    let grant = service.login(login, PASSWORD, Utc::now()).await.unwrap();
    let principal = service
        .authenticate_browser(grant.expose_cookie(), Utc::now())
        .await
        .unwrap();
    let owner = alexandria_storage::access::AccessRepo::new(&db)
        .authorize(&principal)
        .await
        .unwrap();

    Fixture {
        router,
        db,
        account: ids.account,
        workspace: ids.workspace,
        owner,
        cookie: grant.expose_cookie().to_string(),
        csrf: grant.expose_csrf().to_string(),
    }
}

async fn seed_project(fx: &Fixture, alias: &str) -> String {
    #[derive(SurrealValue)]
    struct ProjectRow {
        id: RecordId,
    }
    let mut response = fx
        .db
        .inner()
        .query("CREATE project SET workspace = $workspace, normalized_alias = $alias RETURN AFTER;")
        .bind(("workspace", fx.workspace.as_record_id().clone()))
        .bind(("alias", alias.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<ProjectRow> = response.take(0).unwrap();
    let project = ProjectId::try_from(rows.into_iter().next().unwrap().id).unwrap();
    alexandria_storage::project_handle::format_project_handle(project.as_record_id())
}

async fn seed_hold(
    fx: &Fixture,
    message: &str,
    legacy_target: &str,
    next_due_at: DateTime<Utc>,
) -> String {
    let id = ReminderRepo::new(fx.db.inner())
        .create(&NewReminder {
            message: message.to_string(),
            target_project: Some(legacy_target.to_string()),
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: "cron".into(),
            due_at: None,
            freq: None,
            time_of_day: None,
            weekdays: Vec::new(),
            day_of_month: None,
            cron_expr: Some("0 9 * * MON-FRI".into()),
            next_due_at: Some(next_due_at),
        })
        .await
        .unwrap();
    fx.db
        .inner()
        .query(
            "UPDATE type::record($id) SET account = $account, migration_hold = true, \
             legacy_target_project = $legacy, semantic_revision = 0 RETURN AFTER;",
        )
        .bind(("id", parse_record_id(&id).unwrap()))
        .bind(("account", fx.account.as_record_id().clone()))
        .bind(("legacy", legacy_target.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    id
}

async fn get(fx: &Fixture, path: &str, authenticated: bool) -> Response {
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header(HOST, HOST_VALUE);
    if authenticated {
        request = request.header(COOKIE, cookie_header(fx));
    }
    fx.router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn cookie_header(fx: &Fixture) -> String {
    format!(
        "{}={}; {}={}",
        debug::SESSION_COOKIE,
        fx.cookie,
        debug::CSRF_COOKIE,
        fx.csrf
    )
}

async fn body(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn owner_page_lists_every_held_reminder_and_preserves_legacy_target_text() {
    let fx = fixture("mapping-list-owner").await;
    let project_handle = seed_project(&fx, "mapped-project").await;
    let legacy = [
        "team/repo-one",
        "team/repo-two",
        "remote/<ambiguous>&untrusted",
    ];
    for (index, target) in legacy.iter().enumerate() {
        seed_hold(
            &fx,
            &format!("mapping message {index}"),
            target,
            Utc::now() - Duration::days(3),
        )
        .await;
    }

    let (status, html) = body(get(&fx, "/debug/reminder-mapping", true).await).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "authenticated owner can open the mapping page"
    );
    assert!(
        html.contains("Outstanding migration holds: 3"),
        "the page shows an exact count, not a sample count"
    );
    assert!(html.contains(legacy[0]) && html.contains(legacy[1]));
    assert!(
        html.contains("remote/&#60;ambiguous&#62;&#38;untrusted"),
        "original target text is preserved and HTML-escaped: {html}"
    );
    assert!(
        html.contains("value=\"personal\""),
        "Personal is a selectable destination"
    );
    assert!(
        html.contains(&project_handle),
        "the owner's typed project is a selectable destination"
    );
}

#[tokio::test]
async fn mapping_page_requires_authentication_and_release_requires_csrf_and_same_origin() {
    let fx = fixture("mapping-guard-owner").await;
    let (status, _) = body(get(&fx, "/debug/reminder-mapping", false).await).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "anonymous GET is refused before rendering"
    );

    let (status, _) = body(get(&fx, "/debug/reminder-mapping", true).await).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: the owner can read the page"
    );

    let missing_csrf = fx
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/debug/reminder-mapping/release")
                .header(HOST, HOST_VALUE)
                .header("origin", ORIGIN)
                .header(COOKIE, cookie_header(&fx))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("id=reminder%3Aheld&revision=0&target=personal"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        missing_csrf.status(),
        StatusCode::FORBIDDEN,
        "session cookies alone do not authorize release"
    );

    let cross_origin = fx
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/debug/reminder-mapping/release")
                .header(HOST, HOST_VALUE)
                .header("origin", "https://attacker.example")
                .header(COOKIE, cookie_header(&fx))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "csrf={}&id=reminder%3Aheld&revision=0&target=personal",
                    fx.csrf
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        cross_origin.status(),
        StatusCode::FORBIDDEN,
        "a valid token from another origin is refused"
    );
}

#[tokio::test]
async fn released_overdue_recurring_reminder_coalesces_missed_fires_at_query_time() {
    let fx = fixture("mapping-coalesce-owner").await;
    let due = DateTime::parse_from_rfc3339("2026-06-01T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let id = seed_hold(&fx, "weekday reminder", "team/repo-before-mapping", due).await;
    let parsed_id = parse_record_id(&id).unwrap();
    let before: serde_json::Value = {
        let mut response = fx
            .db
            .inner()
            .query("SELECT * FROM type::record($id);")
            .bind(("id", parsed_id.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        response
            .take::<Vec<serde_json::Value>>(0)
            .unwrap()
            .remove(0)
    };

    // This fixture's overdue time is intentionally left untouched by release; the same query-time
    // coalescer used by check_reminders then counts every weekday fire in the elapsed interval.
    let now = DateTime::parse_from_rfc3339("2026-06-03T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let outcome = ReminderRepo::new(fx.db.inner())
        .release_migration_hold(
            &fx.owner,
            &id,
            alexandria_storage::models::identity::Version::ZERO,
            None,
            &OperationId::new("coalesce-after-reminder-map"),
        )
        .await;
    assert!(
        outcome.is_ok(),
        "owner mapping should release the overdue reminder: {outcome:?}"
    );

    let mut response = fx
        .db
        .inner()
        .query("SELECT * FROM type::record($id);")
        .bind(("id", parsed_id))
        .await
        .unwrap()
        .check()
        .unwrap();
    let after: serde_json::Value = response
        .take::<Vec<serde_json::Value>>(0)
        .unwrap()
        .remove(0);
    assert_eq!(
        after["migration_hold"], false,
        "release must open the held reminder"
    );
    assert_eq!(
        after["next_due_at"], before["next_due_at"],
        "release cannot reset missed fires"
    );

    let spec = alexandria_engine::reminders::ScheduleSpec::Cron {
        expr: alexandria_engine::reminders::normalize_cron("0 9 * * MON-FRI").unwrap(),
    };
    let missed =
        alexandria_engine::reminders::occurrences_between(&spec, due, now, chrono_tz::UTC).unwrap();
    assert_eq!(
        (missed.count, missed.saturated),
        (2, false),
        "Tuesday and Wednesday coalesce once"
    );
}
