use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::response::Response;

use super::DebugContext;
use super::html::{error_page, page};
use crate::AlexandriaServer;

/// Shown in place of any row this process cannot answer.
///
/// Wordy and digit-free on purpose. The failure mode this panel exists to avoid is a
/// diagnostic surface rendering a *plausible* wrong number, so "we do not know" has to look
/// nothing like a value — hence no `0`, no empty cell, no `—`.
const UNAVAILABLE: &str = "unavailable outside HTTP mode";

/// One row of the effective-configuration table. `label` is the `config.toml` key so an
/// operator can grep the file for what they are looking at; `value` is already a display
/// string, which keeps "unavailable" an ordinary value rather than a template branch.
struct ConfigRow {
    label: &'static str,
    value: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    nav: &'static str,
    fact_count: usize,
    deleted_fact_count: usize,
    cluster_count: usize,
    edge_count: usize,
    raw_count: usize,
    session_count: usize,
    config_rows: Vec<ConfigRow>,
    /// Prominent only when the applied and compiled-in schema versions disagree.
    schema_note: String,
}

/// Values the running server itself holds. Available in every mode, including stdio and
/// tests, because they are read from `AlexandriaServer` and the live embedding provider.
fn server_rows(server: &AlexandriaServer) -> Vec<ConfigRow> {
    vec![
        row("retrieve.min_similarity", &server.retrieve_min_similarity),
        row("activation.top_n", &server.activation_top_n),
        row(
            "activation.propagation_factor",
            &server.activation_config.propagation_factor,
        ),
        row("activation.max_hops", &server.activation_config.max_hops),
        row("cluster.join_threshold", &server.cluster_join_threshold),
        row("cluster.cohesion_floor", &server.cohesion_floor),
        row("heat.spacing_halflife_secs", &server.heat_spacing_halflife),
        // The provider, not the config: `model_id()`/`dimensions()` report what was actually
        // loaded, which is the only answer that explains existing vectors in the database.
        // The config-file pair is rendered next to it, labelled, because "asked for" and
        // "loaded" disagreeing is itself a finding.
        row("embedding.model (loaded)", &server.embedding.model_id()),
        row(
            "embedding.dimensions (loaded)",
            &server.embedding.dimensions(),
        ),
    ]
}

/// Values only HTTP mode can answer, because they live in the binary crate's `Config` and
/// never reach `AlexandriaServer`. With no context every row says so rather than guessing.
///
/// One label list serves both modes, so a row added here is unavailable-in-stdio by
/// construction instead of depending on the author remembering to edit two branches.
fn context_rows(ctx: Option<&DebugContext>) -> Vec<ConfigRow> {
    vec![
        row_str(
            "server.transport",
            or_unavailable(ctx.map(|c| &c.transport)),
        ),
        row_str("server.host", or_unavailable(ctx.map(|c| &c.bind_host))),
        row_str("server.port", or_unavailable(ctx.map(|c| c.bind_port))),
        row_str(
            "database.data_dir",
            or_unavailable(ctx.map(|c| &c.data_dir)),
        ),
        row_str(
            "cluster.merge_threshold",
            or_unavailable(ctx.map(|c| c.cluster_merge_threshold)),
        ),
        row_str(
            "cluster.maintenance_interval_secs",
            or_unavailable(ctx.map(|c| c.maintenance_interval_secs)),
        ),
        row_str(
            "embedding.model (config file)",
            or_unavailable(ctx.map(|c| &c.embedding_model)),
        ),
        row_str(
            "embedding.device (config file)",
            or_unavailable(ctx.map(|c| &c.embedding_device)),
        ),
    ]
}

/// A missing answer renders as [`UNAVAILABLE`], never as a zero or an empty cell.
fn or_unavailable(value: Option<impl std::fmt::Display>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => UNAVAILABLE.to_string(),
    }
}

/// Applied version comes from the database, head from the binary. They differ exactly when
/// migrations are pending — the thing a dashboard should shout about rather than bury.
fn schema_rows(applied: &Result<Option<String>, anyhow::Error>, head: &str) -> Vec<ConfigRow> {
    vec![
        row_str("schema.version (applied)", applied_text(applied)),
        row_str("schema.version (compiled-in head)", head.to_string()),
    ]
}

fn applied_text(applied: &Result<Option<String>, anyhow::Error>) -> String {
    match applied {
        Ok(Some(v)) => v.clone(),
        Ok(None) => "not recorded".to_string(),
        Err(e) => format!("unreadable ({e})"),
    }
}

/// Empty string means "no drift to report", which the template renders as no paragraph. A
/// version that could not be read is *not* drift — claiming otherwise would invent a
/// comparison the data does not support.
fn schema_note(applied: &Result<Option<String>, anyhow::Error>, head: &str) -> String {
    let Some(applied) = applied.as_ref().ok().and_then(|v| v.as_deref()) else {
        return format!("Applied schema version could not be read; this binary's head is v{head}.");
    };
    if applied == head {
        String::new()
    } else {
        format!(
            "SCHEMA DRIFT: the database is at version {applied} but this binary expects \
             version {head}. Migrations are pending — the server may be running a schema it \
             does not fully understand."
        )
    }
}

fn row<T: std::fmt::Display>(label: &'static str, value: &T) -> ConfigRow {
    ConfigRow {
        label,
        value: value.to_string(),
    }
}

fn row_str(label: &'static str, value: String) -> ConfigRow {
    ConfigRow { label, value }
}

pub async fn handler(
    State(server): State<AlexandriaServer>,
    Extension(ctx): Extension<Option<DebugContext>>,
) -> Response {
    let stats = match alexandria_storage::stats::gather(server.db.inner()).await {
        Ok(stats) => stats,
        // The legacy page prefixed the raw error, so the prefix travels with the message.
        Err(e) => return error_page("dashboard", &format!("Failed to load stats: {e}")),
    };

    let head = alexandria_storage::schema::LATEST_VERSION.to_string();
    let applied =
        alexandria_storage::system_config::get_config(server.db.inner(), "schema_version").await;
    let schema_note = schema_note(&applied, &head);

    let mut config_rows = server_rows(&server);
    config_rows.extend(context_rows(ctx.as_ref()));
    config_rows.extend(schema_rows(&applied, &head));

    page(DashboardTemplate {
        nav: "dashboard",
        fact_count: stats.fact_count,
        deleted_fact_count: stats.deleted_fact_count,
        cluster_count: stats.cluster_count,
        edge_count: stats.edge_count,
        raw_count: stats.raw_count,
        session_count: stats.session_count,
        config_rows,
        schema_note,
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::UNAVAILABLE;

    async fn render(app: axum::Router) -> String {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// Reads the `<td>` of a labelled config row. `dashboard.html` emits every row on one
    /// physical line as `<tr><th>label</th><td>value</td></tr>`, so an exact match on the
    /// opening pair cannot be satisfied by the counts table above it.
    fn config_value(html: &str, label: &str) -> String {
        let open = format!("<tr><th>{label}</th><td>");
        let start = html.find(&open).unwrap_or_else(|| {
            panic!(
                "no config row for {label:?}; row headings on the page: {headings:?}",
                headings = headings_in(html)
            )
        });
        let rest = &html[start + open.len()..];
        let end = rest
            .find("</td>")
            .unwrap_or_else(|| panic!("unterminated value for {label:?}"));
        rest[..end].to_string()
    }

    /// Every `<th>` in a `<tr><th>…</th>` pair, for a readable failure message above.
    fn headings_in(html: &str) -> Vec<&str> {
        html.split("<tr><th>")
            .skip(1)
            .filter_map(|rest| rest.split("</th>").next())
            .collect()
    }

    /// The whole point of the panel: the floor the server will actually apply, not a
    /// documented default. `0.42` matches no default in the codebase, so this cannot pass by
    /// accident — it is the regression guard for the 0.10-config / 0.30-constructor split.
    #[tokio::test]
    async fn test_dashboard_shows_effective_min_similarity() {
        let server = super::super::test_support::test_server()
            .await
            .with_retrieve_min_similarity(0.42);
        let html = render(crate::debug::router(server)).await;
        assert_eq!(
            config_value(&html, "retrieve.min_similarity"),
            "0.42",
            "the dashboard must report the floor the server holds"
        );
    }

    #[tokio::test]
    async fn test_dashboard_shows_embedding_model_and_dimensions() {
        let server = super::super::test_support::test_server().await;
        let html = render(crate::debug::router(server)).await;
        // The harness stub reports "stub" / 2 dims; both must appear on the loaded rows.
        assert_eq!(config_value(&html, "embedding.model (loaded)"), "stub");
        assert_eq!(
            config_value(&html, "embedding.dimensions (loaded)"),
            "2",
            "dimensions come from the provider, not from a config constant"
        );
    }

    /// With no context the panel must admit it. Asserts both halves of the promise: the
    /// wording is present, and no context-dependent row leaks a number that was invented
    /// rather than read.
    #[tokio::test]
    async fn test_dashboard_config_panel_unavailable_without_context() {
        let server = super::super::test_support::test_server().await;
        let html = render(crate::debug::router(server)).await;
        assert!(
            html.contains(UNAVAILABLE),
            "no unavailable wording rendered"
        );

        for label in [
            "server.transport",
            "server.host",
            "server.port",
            "database.data_dir",
            "cluster.merge_threshold",
            "cluster.maintenance_interval_secs",
            "embedding.model (config file)",
            "embedding.device (config file)",
        ] {
            let value = config_value(&html, label);
            assert_eq!(
                value, UNAVAILABLE,
                "{label} must not show a value without a DebugContext"
            );
            assert!(
                !value.chars().any(|c| c.is_ascii_digit()),
                "{label} rendered a digit ({value}) that no one supplied"
            );
        }

        // Rows that ARE answerable in this mode still render, so the panel degrades per row
        // rather than wholesale.
        assert_eq!(config_value(&html, "cluster.cohesion_floor"), "0.6");
        assert_eq!(config_value(&html, "activation.top_n"), "3");
    }

    /// The only coverage of the populated path, so it carries the weight of proving the
    /// plumbing exists at all. Values are distinctive: none of them is a default.
    #[tokio::test]
    async fn test_dashboard_with_context_shows_bind_and_transport() {
        let server = super::super::test_support::test_server().await;
        let ctx = super::DebugContext {
            embedding_model: "org/configured-model-v9".to_string(),
            embedding_device: "cuda-device-7".to_string(),
            transport: "http-transport-x".to_string(),
            bind_host: "bind-host-9".to_string(),
            bind_port: 45678,
            data_dir: "/tmp/data-dir-9".to_string(),
            cluster_merge_threshold: 0.875,
            maintenance_interval_secs: 1234,
        };
        let html = render(crate::debug::router_with_context(server, Some(ctx))).await;

        assert_eq!(config_value(&html, "server.transport"), "http-transport-x");
        assert_eq!(config_value(&html, "server.host"), "bind-host-9");
        assert_eq!(config_value(&html, "server.port"), "45678");
        assert_eq!(config_value(&html, "database.data_dir"), "/tmp/data-dir-9");
        assert_eq!(config_value(&html, "cluster.merge_threshold"), "0.875");
        assert_eq!(
            config_value(&html, "cluster.maintenance_interval_secs"),
            "1234"
        );
        assert_eq!(
            config_value(&html, "embedding.model (config file)"),
            "org/configured-model-v9"
        );
        assert_eq!(
            config_value(&html, "embedding.device (config file)"),
            "cuda-device-7"
        );
        // The configured model and the loaded one are different rows on purpose: the stub
        // provider says "stub", the config says something else, and both must show.
        assert_eq!(config_value(&html, "embedding.model (loaded)"), "stub");
        assert!(
            !html.contains(UNAVAILABLE),
            "a row leaked unavailability: {html}"
        );
    }

    #[tokio::test]
    async fn test_dashboard_shows_schema_versions() {
        let server = super::super::test_support::test_server().await;
        let html = render(crate::debug::router(server)).await;
        let head = alexandria_storage::schema::LATEST_VERSION.to_string();
        assert_eq!(
            config_value(&html, "schema.version (compiled-in head)"),
            head
        );
        // `test_server()` migrates, so the applied version is recorded and equal: no drift
        // warning, and the applied row carries the real number rather than a fallback.
        assert_eq!(config_value(&html, "schema.version (applied)"), head);
        assert!(
            !html.contains("SCHEMA DRIFT"),
            "a fully migrated database must not be flagged as drifting"
        );
    }

    #[tokio::test]
    async fn test_dashboard_shows_session_count() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        for id in ["sess-dashboard-7", "sess-dashboard-8"] {
            repo.create(id, None, None).await.unwrap();
        }
        let html = render(crate::debug::router(server)).await;
        assert!(
            html.contains("<tr><th>Sessions</th><td>2</td></tr>"),
            "Stats::session_count was not surfaced"
        );
    }

    #[tokio::test]
    async fn test_dashboard_returns_200_with_stats() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Alexandria Debug Dashboard"));
        assert!(text.contains("Facts (active)"));
    }
}
