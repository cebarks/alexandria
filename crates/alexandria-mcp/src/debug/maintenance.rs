use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;

use super::html::{self, error_page, page};
use crate::AlexandriaServer;

#[derive(serde::Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
}

const PAGE_SIZE: usize = 50;

/// One `maintenance_log` row, flattened for display. The action badge is markup, so it lives
/// in the template and this carries the raw `action` string it switches on.
///
/// The four attribution columns are strings rather than `Option` because this page renders one
/// table, not two shapes of row: a pre-v008 entry has no attribution, and it reads as the shared
/// absent marker (`html::ABSENT`), exactly like an absent timestamp does. `run_id` is the column
/// README describes as the unit a pass is selected or reversed by, so it has to be on the page or
/// the promise is about a column nobody can see.
struct MaintenanceRow {
    action: String,
    source_id: String,
    targets: Vec<TargetLink>,
    members_moved: i64,
    timestamp: String,
    run_id: String,
    job: String,
    disposition: String,
    /// What a value-changing verb overwrote, already formatted. A demotion without this is a number
    /// that moved and no record of where it started.
    previous_value: String,
}

/// A `maintenance_log` target plus the page that can show it.
///
/// The table prefix is the only honest signal, and the two kinds of row genuinely differ: split and
/// merge target clusters, collapse targets a *fact*. This template used to link every target to
/// `/debug/clusters/{id}`, so every collapse row linked to a cluster page that cannot resolve a fact
/// id. An empty `route` renders the id as text instead of as a dead link.
struct TargetLink {
    id: String,
    route: &'static str,
}

fn inspect_route(id: &str) -> &'static str {
    match id.split(':').next().unwrap_or_default() {
        "cluster" => "/debug/clusters/",
        "fact" => "/debug/memories/",
        "session" => "/debug/sessions/",
        _ => "",
    }
}

/// `prev_href` / `next_href` / `summary` feed `templates/_pagination.html`'s `pager` macro,
/// which is presentational: this page paginates by `?page=N`, so the full hrefs are built
/// here and an empty string means no link on that side.
#[derive(Template)]
#[template(path = "maintenance.html")]
struct MaintenanceTemplate {
    nav: &'static str,
    logs: Vec<MaintenanceRow>,
    prev_href: String,
    next_href: String,
    summary: String,
    total_pages: usize,
    total: usize,
}

pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<Pagination>,
) -> Response {
    // `current_page`, not `page`: the render helper `html::page` is in scope in this module.
    let current_page = params.page.unwrap_or(1).max(1);
    let offset = (current_page - 1) * PAGE_SIZE;

    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let total = cluster_repo.count_maintenance_logs().await.unwrap_or(0);
    let logs = match cluster_repo.list_maintenance_logs(PAGE_SIZE, offset).await {
        Ok(l) => l,
        Err(e) => return error_page("maintenance", &e.to_string()),
    };

    let rows = logs
        .into_iter()
        .map(|log| MaintenanceRow {
            action: log.action,
            source_id: log.source_id,
            targets: log
                .target_ids
                .into_iter()
                .map(|id| TargetLink {
                    route: inspect_route(&id),
                    id,
                })
                .collect(),
            members_moved: log.members_moved,
            run_id: log.run_id.unwrap_or_else(|| html::ABSENT.to_string()),
            job: log.job.unwrap_or_else(|| html::ABSENT.to_string()),
            disposition: log.disposition.unwrap_or_else(|| html::ABSENT.to_string()),
            previous_value: match log.previous_value {
                // Formatted here rather than in the template so `0.5` and `0.50` cannot both appear
                // across rows of the same column, which is what makes a demotion readable as a move
                // from a known value.
                Some(value) => format!("{value:.2}"),
                None => html::ABSENT.to_string(),
            },
            // Deliberately via the shared `html::format_dt`, so **no seconds**: this page used to
            // print `%H:%M:%S UTC` and an empty cell for an absent timestamp, which made it the
            // one page in the UI that disagreed with the others. Do not restore the seconds —
            // minute resolution is the convention, and `created_at` still orders the table.
            timestamp: html::format_dt(log.created_at),
        })
        .collect();

    let total_pages = total.div_ceil(PAGE_SIZE);
    let prev_href = if current_page > 1 {
        format!("/debug/maintenance?page={}", current_page - 1)
    } else {
        String::new()
    };
    let next_href = if current_page < total_pages {
        format!("/debug/maintenance?page={}", current_page + 1)
    } else {
        String::new()
    };
    let summary = format!("Page {current_page} of {total_pages} ({total} entries)");

    page(MaintenanceTemplate {
        nav: "maintenance",
        logs: rows,
        prev_href,
        next_href,
        summary,
        total_pages,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::MaintenanceTemplate;
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_maintenance_log_empty() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/maintenance")
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
        assert!(text.contains("Maintenance Log"));
        assert!(text.contains("0 entries"));
    }

    #[tokio::test]
    async fn test_maintenance_log_shows_merge() {
        let server = super::super::test_support::test_server().await;
        let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());

        // Set up two clusters and merge them
        let c1 = cluster_repo
            .create(Some("keep"), &[1.0, 0.0])
            .await
            .unwrap();
        let c2 = cluster_repo
            .create(Some("remove"), &[0.98, 0.02])
            .await
            .unwrap();
        let f1 = memory_repo
            .create_fact("fact1", 0.5, &[1.0, 0.0], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&c2, &f1).await.unwrap();

        cluster_repo
            .execute_merge(&c1, &c2, &[0.99, 0.01])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/maintenance")
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
        assert!(text.contains("merge"));
        assert!(text.contains("1 entries"));
    }

    /// Both integration tests above create at most one log, so `total_pages > 1` is never true
    /// and the `pager` call site is compiled but never rendered — and since both hrefs are
    /// `String`, a swapped prev/next would not be caught by the type checker either.
    #[test]
    fn test_maintenance_template_renders_pager_with_correct_sides() {
        let html = MaintenanceTemplate {
            nav: "maintenance",
            logs: vec![],
            prev_href: "/debug/maintenance?page=1".to_string(),
            next_href: "/debug/maintenance?page=3".to_string(),
            summary: "Page 2 of 3 (101 entries)".to_string(),
            total_pages: 3,
            total: 101,
        }
        .render()
        .unwrap();
        assert!(
            html.contains(r#"href="/debug/maintenance?page=1">← Prev"#),
            "prev must carry the lower page number; got: {html}"
        );
        assert!(
            html.contains(r#"href="/debug/maintenance?page=3">Next →"#),
            "next must carry the higher page number; got: {html}"
        );
        assert!(html.contains("Page 2 of 3 (101 entries)"), "got: {html}");
        assert!(
            !html.contains("<p>101 entries</p>"),
            "multi-page render must not also emit the single-page fallback; got: {html}"
        );
    }

    /// The shared timestamp contract, asserted on this page's markup.
    ///
    /// `/debug/maintenance` was the one page that disagreed: it printed `%H:%M:%S UTC` and left
    /// the cell **empty** when `created_at` was NULL. Both halves are pinned here — a revert to
    /// the seconds form trips `assert_no_seconds_timestamp`, a revert to `unwrap_or_default()`
    /// trips the `<td></td>` assertion.
    #[test]
    fn test_maintenance_template_uses_the_shared_timestamp_format_and_absent_marker() {
        use super::{MaintenanceRow, TargetLink, inspect_route};
        use crate::debug::html;

        let row = |action: &str, source: &str, targets: &[&str]| MaintenanceRow {
            action: action.into(),
            source_id: source.into(),
            targets: targets
                .iter()
                .map(|id| TargetLink {
                    id: (*id).to_string(),
                    route: inspect_route(id),
                })
                .collect(),
            members_moved: 3,
            timestamp: html::format_dt(Some(html::example_dt())),
            run_id: "run-1".into(),
            job: "cluster".into(),
            disposition: html::ABSENT.into(),
            previous_value: html::ABSENT.into(),
        };

        let html = MaintenanceTemplate {
            nav: "maintenance",
            logs: vec![
                row("merge", "cluster:c1", &["cluster:c2"]),
                MaintenanceRow {
                    timestamp: html::format_dt(None),
                    ..row("split", "cluster:c3", &[])
                },
            ],
            prev_href: String::new(),
            next_href: String::new(),
            summary: String::new(),
            total_pages: 1,
            total: 2,
        }
        .render()
        .unwrap();

        assert!(
            html.contains("<td>2026-07-05 11:50 UTC</td>"),
            "the shared minute form must appear verbatim; got: {html}"
        );
        assert!(
            html.contains(&format!("<td>{}</td>", html::ABSENT)),
            "an absent time must render the shared marker; got: {html}"
        );
        html::assert_no_seconds_timestamp(&html, "maintenance.html");
    }

    #[test]
    fn test_maintenance_template_empty_state_keeps_the_header_and_marks_the_gap() {
        let html = MaintenanceTemplate {
            nav: "maintenance",
            logs: vec![],
            prev_href: String::new(),
            next_href: String::new(),
            summary: "0 entries".into(),
            total_pages: 1,
            total: 0,
        }
        .render()
        .unwrap();
        assert!(
            html.contains("<th>Action</th>"),
            "the header row must survive an empty log; got: {html}"
        );
        assert!(
            html.contains("<td colspan=\"9\" class=\"empty\">No maintenance events recorded.</td>"),
            "the shared empty-state row must stand in for the missing rows; got: {html}"
        );
    }

    /// The columns README describes as the reason this page exists — "each carrying the `run_id` of
    /// the tick that wrote it so one pass can be selected, or reversed, as a unit" — asserted on the
    /// markup. Before this the page rendered neither `run_id` nor the two new verbs, and the badge
    /// allowlist covered only merge/split, so every collapse and demote row an agent-facing job wrote
    /// displayed as "unknown".
    ///
    /// Also asserts the target routing, which is the half that used to lie: a collapse row's target
    /// is a survivor *fact*, and the old template linked every target to `/debug/clusters/{id}`.
    #[test]
    fn test_maintenance_template_renders_attribution_and_routes_targets_by_table() {
        use super::{MaintenanceRow, TargetLink, inspect_route};
        use crate::debug::html;

        let link = |id: &str| TargetLink {
            id: id.to_string(),
            route: inspect_route(id),
        };
        let html = MaintenanceTemplate {
            nav: "maintenance",
            logs: vec![
                MaintenanceRow {
                    action: "collapse".into(),
                    source_id: "fact:loser".into(),
                    targets: vec![link("fact:survivor")],
                    members_moved: 0,
                    timestamp: html::format_dt(Some(html::example_dt())),
                    run_id: "run-1728".into(),
                    job: "collapse".into(),
                    disposition: "soft_delete".into(),
                    previous_value: html::ABSENT.into(),
                },
                MaintenanceRow {
                    action: "demote".into(),
                    source_id: "fact:cold".into(),
                    // A target from a table this UI does not render, so the empty-route branch is
                    // exercised: the id must appear as text rather than as a link to a page that
                    // cannot resolve it.
                    targets: vec![link("raw:doc42")],
                    members_moved: 0,
                    timestamp: html::format_dt(Some(html::example_dt())),
                    run_id: "run-1729".into(),
                    job: "appraise".into(),
                    disposition: "demote".into(),
                    previous_value: "0.50".into(),
                },
                MaintenanceRow {
                    action: "split".into(),
                    source_id: "cluster:old".into(),
                    targets: vec![link("cluster:a"), link("cluster:b")],
                    members_moved: 12,
                    timestamp: html::format_dt(Some(html::example_dt())),
                    run_id: html::ABSENT.into(),
                    job: html::ABSENT.into(),
                    disposition: html::ABSENT.into(),
                    previous_value: html::ABSENT.into(),
                },
            ],
            prev_href: String::new(),
            next_href: String::new(),
            summary: String::new(),
            total_pages: 1,
            total: 3,
        }
        .render()
        .unwrap();

        for badge in [">collapse<", ">demote<"] {
            assert!(
                html.contains(badge),
                "the two dreaming verbs must badge as themselves, not as unknown; got: {html}"
            );
        }
        assert!(
            !html.contains(">unknown<"),
            "every verb this branch writes must have a badge; got: {html}"
        );
        assert!(
            html.contains("run-1728") && html.contains("run-1729"),
            "`run_id` must be on the page, or README's unit-of-reversal claim describes a column \
             nobody can see; got: {html}"
        );
        assert!(
            html.contains("appraise") && html.contains("0.50") && html.contains("soft_delete"),
            "job, disposition and the value a demotion overwrote must all render; got: {html}"
        );
        // A fact target goes to the memory page and a cluster target to the cluster page. The colon
        // is percent-encoded because ids in an href go through `|urlencode` — that filter is the
        // load-bearing escape here, not decoration, so the expected bytes carry `%3A`.
        assert!(
            html.contains(r#"href="/debug/memories/fact%3Asurvivor""#),
            "a collapse row's survivor is a fact and must link to the memory page; got: {html}"
        );
        assert!(
            html.contains(r#"href="/debug/clusters/cluster%3Aa""#),
            "a split row's targets are clusters; got: {html}"
        );
        // The absent marker, not a blank cell, for a row written before v008 — the same convention
        // the timestamp column already uses, so one page does not render two kinds of nothing.
        assert!(
            html.contains(&format!("<td>{}</td>", html::ABSENT)),
            "a pre-v008 row must show the shared absent marker; got: {html}"
        );
        assert!(
            html.contains(r#"<td>raw:doc42</td>"#) && !html.contains(r#"href="/debug/raw/doc42""#),
            "a target with no page for its table renders as text, never as a dead link; got: {html}"
        );
    }
}
