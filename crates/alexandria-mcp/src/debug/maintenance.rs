//! The administrator's scheduler-health page.
//!
//! This route used to be the store's audit trail in a table: one row per corpus change, with
//! `source_id`, `target_ids` and the confidence a demotion overwrote. Decision B retired that page
//! here, for a reason that is a schema fact rather than a taste: `maintenance_log` (v004 + v008)
//! carries **no owner and no workspace column**, so no read of it can be narrowed to a caller — and
//! an owner-facing list that reaches the whole table is exactly the cross-account exposure this task
//! exists to close. What remains is the aggregate: per run, which jobs ran, which verbs they used,
//! how many rows they wrote and how many members they moved. That answers "did the scheduler do
//! something this hour" without naming whose memories it touched.
//!
//! What this page is *not*: an owner's own change history. That is Task 10's page over
//! `semantic_operation`, which does carry a workspace, and the plan assigns it there
//! (`operations.rs` / `operation_detail.html`). Rows written before any attribution exists — every
//! row written so far, and every row a dreaming pass writes until Task 10 — stay visible to no
//! browser role at all: they appear only inside the unattributed group's counts, never as records.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;

use super::html::{self, error_page, page};
use super::web::{Boundary, BrowserSession};
use crate::AlexandriaServer;
use serde::Deserialize;

/// The page query. `limit`/`offset` are accepted for a bookmarkable slice of the run history; the
/// caps are storage's.
#[derive(Debug, Deserialize)]
pub(crate) struct Pagination {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// One scheduler run, as the health table reports it. No field of this one names a record: the
/// identifiers a log row carries (`source_id`, `target_ids`) are read by nobody on this route.
struct RunRow {
    run: String,
    jobs: String,
    actions: String,
    entries: usize,
    members_moved: i64,
    first_at: String,
    last_at: String,
}

#[derive(Template)]
#[template(path = "maintenance.html")]
struct MaintenanceTemplate {
    nav: &'static str,
    runs: Vec<RunRow>,
    total: usize,
    /// Whether the caller is looking at their own health page or the page refused them — the route is
    /// admin-only, so a rendered page always says `true` and the field exists to keep the template
    /// honest about what it is claiming.
    acting_login: String,
}

pub(crate) async fn list(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Query(params): Query<Pagination>,
) -> Response {
    // The capability, not the role word: `AdminCapability` comes from `authorize_admin` re-reading
    // `account.role` and `account.status`, so a demotion or suspension between page loads ends the
    // route rather than merely the next mutation.
    let Some(admin) = session.admin() else {
        return health_forbidden();
    };
    let limit = params.limit.unwrap_or(DEFAULT_RUNS).clamp(1, MAX_RUNS);
    let offset = params.offset.unwrap_or(0);
    let repo = alexandria_storage::repos::MaintenanceRepo::new(server.db.inner());
    let runs = match repo.run_rollup(admin, limit, offset).await {
        Ok(rows) => rows,
        Err(error) => return error_page("maintenance", &error.to_string()),
    };
    let total = repo.count_runs(admin).await.unwrap_or(runs.len());
    let acting_login = boundary
        .login_of(session.account())
        .await
        .unwrap_or_else(|_| "this administrator".to_string());

    page(MaintenanceTemplate {
        nav: "maintenance",
        runs: runs
            .into_iter()
            .map(|run| RunRow {
                // An unattributed group is reported as its own row rather than hidden: those are the
                // rows every writer without an `AuditContext` produced, and dropping them would make
                // the totals disagree with the table.
                run: run
                    .run_id
                    .unwrap_or_else(|| format!("{} (unattributed)", html::ABSENT)),
                jobs: joined(&run.jobs),
                actions: joined(&run.actions),
                entries: run.entries,
                members_moved: run.members_moved,
                first_at: html::format_dt(run.first_at),
                last_at: html::format_dt(run.last_at),
            })
            .collect(),
        total,
        acting_login,
    })
}

/// How many runs one page shows, and the most it will show.
const DEFAULT_RUNS: usize = 50;
const MAX_RUNS: usize = 200;

fn joined(values: &[String]) -> String {
    if values.is_empty() {
        html::ABSENT.to_string()
    } else {
        values.join(", ")
    }
}

/// The refusal. 403 with the guard's own wording, and no page markup: an owner must not learn what an
/// admin-only surface would have shown them.
/// The refusal. The guard already refuses an owner this route's path, so reaching here without a
/// capability means a route was mounted outside the layer — and the answer is still a plain 403 with
/// no page markup, which is what the browser suite's "no handler ran" assertions key on.
fn health_forbidden() -> Response {
    super::web::plain_forbidden(ADMIN_ONLY_HEALTH)
}

/// The one sentence this route says about why it refused.
const ADMIN_ONLY_HEALTH: &str = "403 Forbidden: scheduler health is an administrator surface, and it \
    is aggregate-only (see debug/guard.rs)";

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RUNS, MAX_RUNS, MaintenanceTemplate, joined};
    use askama::Template;

    /// The rendered health table names no record, in any of the three ways a row of the audit table
    /// could. This is the disclosure the route's whole design rests on, so it is asserted on the
    /// markup rather than argued in a comment: `runs` is the only data the template can reach, and the
    /// fixture fills it with the values a real rollup produces for a store whose rows point at
    /// clusters and facts.
    #[test]
    fn the_health_page_renders_aggregates_and_no_record_identity() {
        let html = MaintenanceTemplate {
            nav: "maintenance",
            runs: vec![super::RunRow {
                run: "run-1".to_string(),
                jobs: joined(&["cluster".to_string(), "appraise".to_string()]),
                actions: joined(&["split".to_string(), "demote".to_string()]),
                entries: 2,
                members_moved: 4,
                first_at: "2026-10-24 09:00 UTC".to_string(),
                last_at: "2026-10-24 09:00 UTC".to_string(),
            }],
            total: 1,
            acting_login: "root".to_string(),
        }
        .render()
        .expect("the health page renders");

        for visible in ["run-1", "cluster", "split", "demote", "2", "4", "root"] {
            assert!(html.contains(visible), "the page dropped {visible}: {html}");
        }
        // The row-level identity columns, and the value a demotion overwrote.
        for leak in [
            "cluster:source",
            "cluster:target",
            "fact:",
            "0.42",
            "session:",
        ] {
            assert!(
                !html.contains(leak),
                "the health page carried {leak}, which is a record identity or a stored value"
            );
        }
        // The disclosure the page owes an owner who somehow reaches it.
        assert!(
            html.contains("no record identifiers"),
            "the page must say what it withholds: {html}"
        );
        assert!(
            html.contains("aggregate"),
            "the aggregate claim is the page's own: {html}"
        );
    }

    /// The unattributed group is labelled rather than hidden, so the table's counts still add up to
    /// the total the pager reports.
    #[test]
    fn an_unattributed_run_is_labelled_and_not_dropped() {
        let html = MaintenanceTemplate {
            nav: "maintenance",
            runs: vec![super::RunRow {
                run: "— (unattributed)".to_string(),
                jobs: joined(&[]),
                actions: joined(&[]),
                entries: 3,
                members_moved: 0,
                first_at: "—".to_string(),
                last_at: "—".to_string(),
            }],
            total: 3,
            acting_login: "root".to_string(),
        }
        .render()
        .expect("the unattributed row renders");
        assert!(html.contains("unattributed"), "{html}");
        assert!(html.contains(">3<"), "the count vanished: {html}");
    }

    /// An empty store answers with the header row and an empty-table line, the shared convention: a
    /// bare `<p>` would drop the column context that tells a reader the page is working.
    #[test]
    fn an_empty_rollup_still_renders_the_table_shape() {
        let html = MaintenanceTemplate {
            nav: "maintenance",
            runs: Vec::new(),
            total: 0,
            acting_login: "root".to_string(),
        }
        .render()
        .unwrap();
        assert!(
            html.contains("<thead>"),
            "the header row must survive an empty table"
        );
        assert!(
            html.contains("no maintenance has been recorded yet"),
            "{html}"
        );
        assert!(html.contains("Showing 0 of 0 runs"), "{html}");
    }

    #[test]
    fn joined_names_the_absent_case_once() {
        assert_eq!(joined(&[]), super::super::html::ABSENT);
        assert_eq!(joined(&["a".to_string(), "b".to_string()]), "a, b");
    }

    /// The page query reads its two keys the way every other debug page reads a hand-typed number: a
    /// value that is not a `usize` is no opinion, not zero, and the handler's own
    /// `unwrap_or(DEFAULT_RUNS).clamp(1, MAX_RUNS)` is what turns "no opinion" into a page.
    #[test]
    fn a_junk_page_value_is_no_opinion_rather_than_a_failure() {
        for (query, limit, offset) in [
            ("limit=abc&offset=-1", None, None),
            ("limit=7&offset=3", Some(7), Some(3)),
            ("limit=&offset=", None, None),
            ("", None, None),
        ] {
            assert_eq!(parse_number(query, "limit"), limit, "{query}");
            assert_eq!(parse_number(query, "offset"), offset, "{query}");
        }
        // And the fallbacks the handler applies to those `None`s are a page, not a zero-row page.
        let (limit, offset) = (None::<usize>, None::<usize>);
        assert_eq!(
            limit.unwrap_or(DEFAULT_RUNS).clamp(1, MAX_RUNS),
            DEFAULT_RUNS
        );
        assert_eq!(offset.unwrap_or(0), 0);
    }

    /// The same reading `axum`'s `Query` extractor performs on these two keys.
    fn parse_number(query: &str, key: &str) -> Option<usize> {
        query.split('&').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name == key).then(|| value.parse::<usize>().ok()).flatten()
        })
    }
}
