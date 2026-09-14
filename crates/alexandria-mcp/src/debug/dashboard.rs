use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::response::Response;

use super::DebugContext;
use super::clusters::{Cohesion, cohesion_of, embeddings_of};
use super::html::{error_page, page};
use crate::AlexandriaServer;
use crate::server::record_id_to_string;

/// Shown in place of any row this process cannot answer.
///
/// Wordy and digit-free on purpose. The failure mode this panel exists to avoid is a
/// diagnostic surface rendering a *plausible* wrong number, so "we do not know" has to look
/// nothing like a value — hence no `0`, no empty cell, no `—`.
const UNAVAILABLE: &str = "unavailable outside HTTP mode";

/// Cap for the top-tags rollup. Named, because the section heading has to state it: ten tags
/// from a database with more is a *sample*, and a UI that implies otherwise is wrong.
const TAG_LIMIT: usize = 10;

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
    cluster_health: ClusterHealth,
    sessions: SessionRollup,
    tags: BarSection,
    heat: BarSection,
    /// Echoes [`TAG_LIMIT`] so the heading cannot drift from the cap the query applies.
    tag_limit: usize,
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

/// Shown in place of a rollup whose data could not be read.
///
/// Deliberately different wording from [`UNAVAILABLE`]: that means "this process mode has no
/// answer", this means "the read failed", and an operator triaging a page needs to tell the
/// two apart. The cause travels with it because a dashboard that hides *why* it is blind is
/// only half a diagnostic.
fn rollup_unavailable(what: &str, error: &anyhow::Error) -> String {
    format!("{what} could not be read: {error}")
}

/// Maps one rollup's `Result` to the pair every rollup section is built from: its data, and
/// the reason there is none.
///
/// This is the dashboard's degradation contract, and it is a function rather than four matches
/// so that a fifth rollup cannot forget it. A page that 500s because one stat failed is worse
/// than one that shows that stat as unavailable and the other three normally, so `?` is not
/// available at these call sites. On failure the section's [`Default`] — an empty state — is
/// returned, which the template shows only behind the error sentence, never as a plausible `0`.
fn degrade<T: Default>(what: &'static str, source: Result<T, anyhow::Error>) -> (T, String) {
    match source {
        Ok(data) => (data, String::new()),
        Err(e) => {
            tracing::warn!(rollup = what, error = %e, "dashboard rollup unavailable");
            (T::default(), rollup_unavailable(what, &e))
        }
    }
}

/// Cluster-health counts. The three verdict rows sum to the cluster count in the table above.
#[derive(Default)]
struct ClusterHealth {
    error: String,
    healthy: usize,
    needs_split: usize,
    too_few: usize,
}

/// Session counts, split the only way the data model supports: a summary or not.
#[derive(Default)]
struct SessionRollup {
    error: String,
    total: usize,
    finalized: usize,
    idle: usize,
}

/// One horizontal bar. `label` is rendered through `{{ }}` (escaped); `width` is an integer
/// percentage computed by [`bars`], and is the only thing here that reaches a `style`
/// attribute.
struct BarRow {
    label: String,
    count: usize,
    width: u32,
}

/// A bar-chart section. Shared by top tags and heat distribution, which differ only in where
/// their `(label, count)` pairs come from — the markup, and the degradation, are the same.
#[derive(Default)]
struct BarSection {
    error: String,
    rows: Vec<BarRow>,
}

/// Healthy vs needs-split, judged through [`cohesion_of`] from each cluster's **stored**
/// centroid — the same vector the background maintenance task splits on, so this rollup and
/// `clusters.rs`' detail page cannot render opposite verdicts for one cluster.
async fn cluster_health(server: &AlexandriaServer) -> Result<ClusterHealth, anyhow::Error> {
    let repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let mut section = ClusterHealth::default();

    for (cluster, member_count) in repo.list_with_counts().await? {
        // `list_with_counts` already carries the stored centroid, so the only extra reads here
        // are member embeddings — skipped entirely for a cluster with no members, which has no
        // verdict to compute either way.
        //
        // TODO(debt): O(clusters) round trips per dashboard render — and `list_with_counts`
        // itself walks every cluster to count members, so this is twice over. The repo's own
        // notes name cluster-count growth as the first thing to revisit; when it bites the fix
        // is one grouped query inside `alexandria-storage`, not a cache in this handler.
        if member_count == 0 {
            section.too_few += 1;
            continue;
        }
        let id = cluster
            .id
            .as_ref()
            .map(record_id_to_string)
            .unwrap_or_default();
        let members = repo.get_members(&id).await?;
        match cohesion_of(
            &id,
            &cluster.centroid,
            &embeddings_of(&members),
            server.cohesion_floor,
        ) {
            Cohesion::Healthy => section.healthy += 1,
            Cohesion::NeedsSplit => section.needs_split += 1,
            // Fewer members than the engine will judge. `NoCentroid` is unreachable here —
            // every row came from a real cluster record — but counting it rather than
            // dropping it keeps the three rows summing to the cluster total.
            Cohesion::TooSmall | Cohesion::NoCentroid => section.too_few += 1,
        }
    }
    Ok(section)
}

/// `total` is the already-gathered [`alexandria_storage::stats::Stats::session_count`] rather
/// than a second count query, so this section can never disagree with the counts table above
/// it. Only the finalized read can fail.
async fn session_rollup(
    server: &AlexandriaServer,
    total: usize,
) -> Result<SessionRollup, anyhow::Error> {
    let finalized = alexandria_storage::repos::SessionRepo::new(server.db.inner())
        .count_finalized()
        .await?;
    Ok(SessionRollup {
        error: String::new(),
        total,
        finalized,
        // Idle is the remainder, and is never inferred from `ended_at`: `SessionRepo::touch()`
        // bumps that on every attached memory, so it means last activity, not completion. A
        // non-null `summary` — what `count_finalized` selects on — is the only discriminator.
        idle: total.saturating_sub(finalized),
    })
}

/// The ten most-used tags, as bars. The cap is stated in the section heading, because a list of
/// ten that is presented as complete would be a quiet lie about a database with two thousand.
async fn tag_bars(server: &AlexandriaServer) -> Result<BarSection, anyhow::Error> {
    let pairs = alexandria_storage::repos::MemoryRepo::new(server.db.inner())
        .top_tags(TAG_LIMIT)
        .await?;
    Ok(bars(pairs))
}

async fn heat_bars(server: &AlexandriaServer) -> Result<BarSection, anyhow::Error> {
    let counts = alexandria_storage::repos::HeatRepo::new(server.db.inner())
        .heat_histogram()
        .await?;
    // Positional against `HEAT_BANDS`, which is documented as being in the histogram's order.
    // `zip` stops at the shorter side on purpose: a band rendering without a count (or vice
    // versa) after one of the two changes is a quieter failure than panicking on a diagnostic
    // page whose whole job is to stay legible.
    Ok(bars(
        alexandria_storage::repos::heat_repo::HEAT_BANDS
            .into_iter()
            .zip(counts)
            .map(|(band, count)| (band.to_string(), count))
            .collect(),
    ))
}

/// Turns `(label, count)` pairs into bars.
///
/// The arithmetic lives here rather than in the template for two reasons: `dashboard.html` does
/// no arithmetic, and a `style` attribute assembled from data would be an injection point — tag
/// names are user-supplied. Only an integer crosses into markup.
fn bars(pairs: Vec<(String, usize)>) -> BarSection {
    // Scaled to the section's own largest count, not an absolute ceiling: in a database whose
    // most-used tag appears four times, that tag should read as a full bar rather than a sliver.
    let max = pairs.iter().map(|(_, count)| *count).max().unwrap_or(0);
    let rows = pairs
        .into_iter()
        .map(|(label, count)| BarRow {
            label,
            count,
            width: bar_width(count, max),
        })
        .collect();
    BarSection {
        error: String::new(),
        rows,
    }
}

/// Width as an integer percentage, rounded down.
///
/// `count <= max` by construction, so the result cannot exceed 100. `max == 0` — an empty
/// section, or one where every count is zero — returns 0 instead of dividing by zero. Any
/// non-zero count gets at least 1, so a rare tag renders as a visible sliver rather than as a
/// row that looks empty.
fn bar_width(count: usize, max: usize) -> u32 {
    if max == 0 {
        return 0;
    }
    let percent = ((count as u64 * 100) / max as u64) as u32;
    if count > 0 { percent.max(1) } else { percent }
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

    // Every rollup goes through `degrade`, so one unavailable stat costs the reader one section
    // and nothing else.
    let (data, error) = degrade("Cluster health", cluster_health(&server).await);
    let cluster_health = ClusterHealth { error, ..data };
    let (data, error) = degrade(
        "Sessions",
        session_rollup(&server, stats.session_count).await,
    );
    let sessions = SessionRollup { error, ..data };
    let (data, error) = degrade("Top tags", tag_bars(&server).await);
    let tags = BarSection { error, ..data };
    let (data, error) = degrade("Heat distribution", heat_bars(&server).await);
    let heat = BarSection { error, ..data };

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
        cluster_health,
        sessions,
        tags,
        heat,
        tag_limit: TAG_LIMIT,
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
            // Host checking is off for this test: `render` below issues a bare `Request::builder()`
            // GET with no Host header, which an armed check would (correctly) refuse.
            allowed_hosts: vec![],
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

    // --- rollups ---------------------------------------------------------------

    use super::{
        BarSection, ClusterHealth, DashboardTemplate, SessionRollup, TAG_LIMIT, bars, degrade,
    };
    use askama::Template as _;

    /// The full markup one bar row renders to. Asserting the whole row — rather than a label,
    /// a width and a count separately — is what ties them together, so a bar scaled against the
    /// wrong maximum, or paired with the wrong label, cannot pass.
    fn bar_row(label: &str, width: u32, count: usize) -> String {
        format!(
            r#"<div class="bar-row"><span class="bar-name">{label}</span><span class="bar"><span class="bar-fill" style="width: {width}%"></span></span><span class="bar-count">{count}</span></div>"#
        )
    }

    /// Four members mirrored about the x-axis, at cosine 0.56 from their own **stored**
    /// centroid `[0.56, 0]` — below the 0.6 default floor, so the stored centroid itself says
    /// "split".
    async fn needs_split_cluster(server: &crate::AlexandriaServer, label: &str) {
        use alexandria_storage::repos::{ClusterRepo, MemoryRepo};

        let cluster_repo = ClusterRepo::new(server.db.inner());
        let memory_repo = MemoryRepo::new(server.db.inner());
        let cid = cluster_repo
            .create(Some(label), &[0.56, 0.0])
            .await
            .unwrap();
        for (index, embedding) in [
            [0.56_f32, 0.8285],
            [0.56, 0.8285],
            [0.56, -0.8285],
            [0.56, -0.8285],
        ]
        .iter()
        .enumerate()
        {
            let fact = memory_repo
                .create_fact(&format!("{label} member {index}"), 0.5, embedding, &[])
                .await
                .unwrap();
            cluster_repo.add_member(&cid, &fact).await.unwrap();
        }
    }

    /// 2 healthy / 1 needs split / 1 too few members.
    ///
    /// The healthy two come from `disagreeing_cluster`, whose *member average* centroid reads
    /// Needs split — so this exact 2/1 is also the guard that the rollup judges cohesion the way
    /// the maintenance task does. Roll the rollup onto an averaged centroid and it reports 1/2.
    #[tokio::test]
    async fn test_dashboard_cluster_health_counts_verdicts() {
        let server = super::super::test_support::test_server().await;
        super::super::test_support::disagreeing_cluster(&server).await;
        super::super::test_support::disagreeing_cluster(&server).await;
        needs_split_cluster(&server, "diffuse-cluster").await;
        alexandria_storage::repos::ClusterRepo::new(server.db.inner())
            .create(Some("empty-cluster"), &[0.25, -0.75])
            .await
            .unwrap();

        let html = render(crate::debug::router(server)).await;
        for (label, value) in [
            ("Healthy", 2),
            ("Needs split", 1),
            ("Too few members to judge", 1),
        ] {
            let row = format!("<tr><th>{label}</th><td>{value}</td></tr>");
            assert!(
                html.contains(&row),
                "expected {row:?}; cluster rows on the page: {:?}",
                headings_in(&html)
            );
        }
    }

    /// Five sessions: two finalized with a summary, three merely touched.
    ///
    /// `touch()` writes `ended_at`, so the three idle ones already *look* ended to anything
    /// reading that column. A 2/3 split is only reachable through `summary`, which is the point
    /// of the fixture.
    #[tokio::test]
    async fn test_dashboard_session_rollup_separates_finalized_from_idle() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        for index in 1..=5 {
            repo.create(&format!("sess-roll-{index}"), None, None)
                .await
                .unwrap();
        }
        for index in 1..=3 {
            repo.touch(&format!("sess-roll-{index}")).await.unwrap();
        }
        for index in 4..=5 {
            repo.finalize(&format!("sess-roll-{index}"), Some("rollup summary"), None)
                .await
                .unwrap();
        }

        let html = render(crate::debug::router(server)).await;
        for (label, value) in [("Total", 5), ("Finalized", 2), ("Idle", 3)] {
            let row = format!("<tr><th>{label}</th><td>{value}</td></tr>");
            assert!(
                html.contains(&row),
                "expected {row:?}; touched-but-not-finalized sessions must not count as \
                 finalized. Rows: {:?}",
                headings_in(&html)
            );
        }
    }

    /// Three tags used 3 / 2 / 1 times: bars scale to the section's own maximum, so 100% /
    /// 66% / 33%. One tag name is markup, to prove it cannot reach the page raw.
    #[tokio::test]
    async fn test_dashboard_top_tags_bars_scale_to_the_section_max() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        for (tag, times) in [("alpha-tag-3x", 3), ("beta-tag-2x", 2), ("<evil-tag>", 1)] {
            for index in 0..times {
                memory_repo
                    .create_fact(
                        &format!("tagged fact {tag} {index}"),
                        0.5,
                        &[0.1, 0.2],
                        &[tag.to_string()],
                    )
                    .await
                    .unwrap();
            }
        }

        let html = render(crate::debug::router(server)).await;
        assert!(
            html.contains(&bar_row("alpha-tag-3x", 100, 3)),
            "got: {html}"
        );
        assert!(html.contains(&bar_row("beta-tag-2x", 66, 2)), "got: {html}");
        // The escaped form, which also proves the width came from a number rather than from the
        // label: a `style` attribute built out of tag text would put attacker-controlled CSS
        // (or a second attribute) on the page.
        assert!(
            html.contains(&bar_row("&#60;evil-tag&#62;", 33, 1)),
            "the escaped tag row is missing; raw name in markup: {}",
            html.contains("<evil-tag>")
        );
        assert!(
            html.contains("<h2>Top tags (up to 10)</h2>"),
            "the cap has to be stated where the list is shown"
        );
    }

    /// One heat state in each of three bands and two in the top band, so the counts are
    /// 1/1/1/2 — only `3 and above` can be the full-width bar, and every band must appear.
    #[tokio::test]
    async fn test_dashboard_heat_bars_span_every_band() {
        let server = super::super::test_support::test_server().await;
        let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        for (index, heat) in [0.4_f64, 1.4, 2.4, 3.5, 9.75].iter().enumerate() {
            let fact = memory_repo
                .create_fact(&format!("heat fixture {index}"), 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
            heat_repo.create_for_memory(&fact, *heat).await.unwrap();
        }

        let html = render(crate::debug::router(server)).await;
        for (band, width, count) in [
            ("below 1", 50, 1),
            ("1 to 2", 50, 1),
            ("2 to 3", 50, 1),
            ("3 and above", 100, 2),
        ] {
            assert!(
                html.contains(&bar_row(band, width, count)),
                "missing bar row for band {band:?}: {}",
                headings_in(&html).len()
            );
        }
    }

    /// The empty-database path, through the live router. This is what catches a rollup query
    /// that errors on "no rows" instead of returning zeros, and it checks all four sections
    /// appear on a page whose data is entirely absent.
    #[tokio::test]
    async fn test_dashboard_rollups_render_on_an_empty_database() {
        let server = super::super::test_support::test_server().await;
        let html = render(crate::debug::router(server)).await;
        for heading in [
            "<h2>Cluster health</h2>",
            "<h2>Sessions</h2>",
            "<h2>Top tags (up to 10)</h2>",
            "<h2>Heat distribution</h2>",
        ] {
            assert!(html.contains(heading), "missing section {heading:?}");
        }
        assert!(
            !html.contains("could not be read"),
            "nothing failed on an empty database, so no section may say it did: {html}"
        );
        assert!(html.contains("No tags recorded."), "empty state missing");
    }

    /// `degrade` is the only place a rollup's failure is decided, so both of its shapes get
    /// tested directly — including for a second section type, since the point is that the rule
    /// is written once and applies to any rollup.
    #[test]
    fn test_degrade_turns_a_failed_read_into_a_sentence() {
        let (section, error) =
            degrade::<BarSection>("Top tags", Err(anyhow::anyhow!("tag tally failed-9")));
        assert_eq!(error, "Top tags could not be read: tag tally failed-9");
        assert!(
            section.rows.is_empty(),
            "a failed rollup must fall back to its empty state"
        );
        assert!(
            !error.contains(UNAVAILABLE),
            "a data failure must not borrow the wording that means \"this mode cannot answer\""
        );

        let (health, error) = degrade::<ClusterHealth>(
            "Cluster health",
            Err(anyhow::anyhow!("cluster read failed-4")),
        );
        assert!(error.contains("cluster read failed-4"));
        assert!(
            health.error.is_empty(),
            "the sentence travels back separately, so `degrade` must not half-fill the section"
        );

        let (section, error) =
            degrade::<BarSection>("Top tags", Ok(bars(vec![("tag-a".to_string(), 4)])));
        assert_eq!(error, "", "a successful read has nothing to apologise for");
        assert_eq!(section.rows.len(), 1);
    }

    /// The property, at page level: one rollup's read fails and the rest of the dashboard still
    /// renders — headings, counts, bars.
    ///
    /// Driven through [`degrade`] rather than by hand-writing an error string, so the page sees
    /// exactly the pair the handler sees. A router-level version would need a genuinely failing
    /// query, and the only way to cause one from a test is raw SQL inside `src/debug/` — which
    /// the storage boundary forbids; [`test_dashboard_rollups_render_on_an_empty_database`]
    /// covers the live path instead.
    #[test]
    fn test_one_failed_rollup_leaves_the_other_sections_rendering() {
        let (sessions, error) = degrade::<SessionRollup>(
            "Sessions",
            Err(anyhow::anyhow!("session store unreachable-7")),
        );
        let html = DashboardTemplate {
            nav: "dashboard",
            fact_count: 71,
            deleted_fact_count: 72,
            cluster_count: 73,
            edge_count: 74,
            raw_count: 75,
            session_count: 76,
            config_rows: Vec::new(),
            schema_note: String::new(),
            cluster_health: ClusterHealth {
                error: String::new(),
                healthy: 7,
                needs_split: 8,
                too_few: 9,
            },
            sessions: SessionRollup { error, ..sessions },
            tags: bars(vec![("tag-keep-me".to_string(), 5)]),
            heat: bars(vec![("below 1".to_string(), 3)]),
            tag_limit: TAG_LIMIT,
        }
        .render()
        .expect("the page must render even with a broken rollup");

        // The failed section admits it, in its own place in the page.
        assert!(
            html.contains("session store unreachable-7"),
            "the broken section must say why: {html}"
        );
        assert!(html.contains("<h2>Sessions</h2>"));
        // ...and shows no numbers a reader could take for real ones: its `Default` is zeros, and
        // zeros are exactly the plausible wrong values this page exists to avoid.
        assert!(
            !html.contains("<tr><th>Total</th>") && !html.contains("<tr><th>Idle</th>"),
            "a degraded section must render its sentence, not its default zeros: {html}"
        );
        // The other three are untouched.
        assert!(html.contains("<tr><th>Healthy</th><td>7</td></tr>"));
        assert!(html.contains("<tr><th>Needs split</th><td>8</td></tr>"));
        assert!(html.contains(&bar_row("tag-keep-me", 100, 5)));
        assert!(html.contains(&bar_row("below 1", 100, 3)));
        assert!(html.contains("<h2>Top tags (up to 10)</h2>"));
        assert!(html.contains("Facts (active)"));
    }
}
