//! Owner-facing review of legacy reminder targets.
//!
//! Migration holds stay out of delivery, but remain visible here until the owner maps each one to a
//! personal reminder or a project in their workspace. The selected destination is parsed as a typed
//! project handle and storage rechecks ownership inside the release transaction.

use askama::Template;
use axum::extract::Form;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::html::{self, page, unavailable};
use super::web::{Boundary, BrowserSession};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{OperationId, Version};
use alexandria_storage::parse_record_id;
use alexandria_storage::project_handle::{format_project_handle, parse_project_handle};
use alexandria_storage::repos::{ScopedProjectRepo, ScopedReminderRepo};
use surrealdb::types::ToSql;

const NAV: &str = "reminders";

#[derive(Template)]
#[template(path = "reminder_mapping.html")]
struct ReminderMappingTemplate {
    nav: &'static str,
    csrf: String,
    outstanding_holds: i64,
    rows: Vec<MappingRow>,
    projects: Vec<ProjectOption>,
}

struct MappingRow {
    id: String,
    revision: String,
    message: String,
    legacy_target: String,
    schedule: String,
}

struct ProjectOption {
    handle: String,
    label: String,
}

pub(crate) async fn list(boundary: Boundary, session: BrowserSession) -> Response {
    let owner = session.owner();
    let (outstanding_holds, held) =
        match ScopedReminderRepo::new(boundary.db().inner(), owner.clone())
            .migration_holds()
            .await
        {
            Ok(result) => result,
            Err(error) => return unavailable(NAV, "reminders", error),
        };
    let project_page = match ScopedProjectRepo::new(boundary.db().inner(), owner)
        .list(None, 100)
        .await
    {
        Ok(projects) => projects,
        Err(error) => return unavailable(NAV, "reminders", error),
    };
    let projects = project_page
        .entries
        .into_iter()
        .map(|entry| {
            let label = entry
                .project
                .display_name
                .unwrap_or_else(|| entry.project.normalized_alias.clone());
            ProjectOption {
                handle: format_project_handle(entry.project.id.as_record_id()),
                label,
            }
        })
        .collect();
    let rows = held
        .into_iter()
        .map(|row| MappingRow {
            id: row.id.to_sql(),
            revision: row.semantic_revision.unwrap_or(0).to_string(),
            message: row.message,
            legacy_target: row
                .legacy_target_project
                .unwrap_or_else(|| "(no original target text)".to_string()),
            schedule: schedule_label(
                &row.schedule_kind,
                row.freq.as_deref(),
                row.time_of_day.as_deref(),
                &row.weekdays,
                row.day_of_month,
                row.cron_expr.as_deref(),
            ),
        })
        .collect();
    page(ReminderMappingTemplate {
        nav: NAV,
        csrf: session.csrf().to_string(),
        outstanding_holds,
        rows,
        projects,
    })
}

#[derive(Deserialize)]
pub(crate) struct ReleaseForm {
    pub(crate) id: String,
    pub(crate) revision: String,
    /// `personal` or one strict typed project handle; aliases are not accepted here.
    pub(crate) target: String,
}

pub(crate) async fn release(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<ReleaseForm>,
) -> Response {
    let id = match parse_record_id(&form.id) {
        Ok(id) if id.table.as_str() == "reminder" => id,
        _ => return bad_request("invalid reminder id"),
    };
    let revision = match form.revision.parse::<u64>() {
        Ok(revision) => Version::new(revision),
        Err(_) => return bad_request("invalid reminder revision"),
    };
    let target = if form.target == "personal" {
        None
    } else {
        match parse_project_handle(&form.target) {
            Ok(project) => Some(project),
            Err(_) => {
                return bad_request("destination must be Personal or an owned project handle");
            }
        }
    };
    let operation = OperationId::new(format!("reminder-map-{}", uuid::Uuid::new_v4()));
    let reminder_id = id.to_sql();
    match ScopedReminderRepo::new(boundary.db().inner(), session.owner().clone())
        .release_migration_hold(&reminder_id, revision, target, &operation)
        .await
    {
        Ok(_) => Redirect::to("/debug/reminder-mapping").into_response(),
        Err(StorageError::Stale) => {
            let mut response =
                html::error_page(NAV, "This reminder changed; reload before mapping it.");
            *response.status_mut() = StatusCode::CONFLICT;
            response
        }
        Err(StorageError::NotFound) => {
            bad_request("reminder or destination is not available to this account")
        }
        Err(error) => unavailable(NAV, "reminders", error),
    }
}

fn bad_request(message: &str) -> Response {
    let mut response = html::error_page(NAV, message);
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}

fn schedule_label(
    kind: &str,
    freq: Option<&str>,
    time: Option<&str>,
    weekdays: &[String],
    day_of_month: Option<i64>,
    cron: Option<&str>,
) -> String {
    match kind {
        "once" => "One-time".to_string(),
        "pattern" => format!(
            "{}{}{}",
            freq.unwrap_or("Recurring"),
            time.map(|value| format!(" at {value}")).unwrap_or_default(),
            if weekdays.is_empty() {
                day_of_month
                    .map(|day| format!(" on day {day}"))
                    .unwrap_or_default()
            } else {
                format!(" on {}", weekdays.join(", "))
            }
        ),
        "cron" => format!("Cron: {}", cron.unwrap_or("(missing expression)")),
        _ => "Unknown schedule".to_string(),
    }
}
