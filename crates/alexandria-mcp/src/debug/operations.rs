//! The owner's private operation history — the read half of Task 10's journal.
//!
//! Every semantic change an owner makes is described by one `semantic_operation` row carrying the exact
//! before- and after-state, the actor, and the credential that presented itself. This page is what makes
//! that record answer the question it exists for: *"what did this memory say before?"*
//!
//! # Why this is a page and not a wider audit
//!
//! There is no operator-facing corpus log. `maintenance_log` (Task 9's `/debug/maintenance`) reports
//! **content-free aggregates** by run id, and nothing here reads it. The journal is the owner's, and the
//! only thing that makes that sentence true is the read path:
//!
//! [`MutationRepo::history`] lists the caller's own operations by workspace, and
//! [`MutationRepo::operation_by_id`] resolves an operation *and then each snapshot it names* under that
//! same workspace predicate. The second half is not decoration. `semantic_operation.preimages` is an array
//! of record links a page could resolve by handle alone — and a page that did would answer a stale or
//! forged id with **another owner's memory content**, turning the private journal into exactly the probe it
//! was built to prevent. A foreign key and an absent one are one answer (`None` → 404), so a link cannot be
//! used to test whether some other account holds an operation.
//!
//! # What the page shows and withholds
//!
//! Memory content appears here because it is the owner's own content, in the owner's own history — that is
//! the feature. Actor is rendered as the owner's account, never as a foreign id, and the *initiator*
//! credential is described in kind ("browser session" / "agent token") rather than as a row handle, because
//! naming a credential id on a page is a way to make an opaque-looking identifier look like a secret.
//! Effects are shown as the closed label set they are: expected beside actual, since the difference between
//! the two is how a batch reports the entries it did not apply.

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;

use super::html::{self, page, unavailable};
use super::web::{Boundary, BrowserSession};
use crate::AlexandriaServer;
use alexandria_storage::models::{
    SemanticSnapshot,
    mutation::{FactSnapshot, OperationSummary},
};
use alexandria_storage::repos::{MAX_HISTORY_PAGE, MutationRepo};
use surrealdb::types::Value;

/// The nav key this page highlights, shared by both its routes.
const NAV: &str = "history";

pub(crate) async fn list(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
) -> Response {
    // A refusal renders 403, like every other private page in this module (`memories.rs`, `clusters.rs`) —
    // **not** an empty history. An owner whose account was just suspended, demoted or reassigned resolves to
    // nothing here, and showing them "you have no operations" would tell them their corpus is empty when the
    // true fact is that they may no longer read it. `corpus` re-reads the live rows on every call for exactly
    // this race.
    let Ok((owner, _read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let repo = MutationRepo::new(server.db.inner());
    let operations = match repo.history(&owner, MAX_HISTORY_PAGE).await {
        Ok(operations) => operations,
        Err(error) => return unavailable(NAV, "operation history", error),
    };
    page(OperationsTemplate {
        nav: NAV,
        rows: operations.into_iter().map(SummaryView::from).collect(),
        // A page bound, stated as one. `history` reads the newest `MAX_HISTORY_PAGE` rows of this owner's
        // own operations, so once an account has more than that, the older ones are not reachable from this
        // page — and a listing that says nothing about its own ceiling invites the reader to believe the
        // list is the whole history. Naming the number is the honest shape; a pager is a later decision.
        capped: MAX_HISTORY_PAGE,
    })
}

/// One journal row as the list renders it: navigation only, no content.
///
/// The list deliberately carries no memory text. A page that showed a snippet per row would be the widest
/// content surface in the debug UI for an attacker who had a stolen session cookie and wanted to skim, and
/// `history` does not return content anyway — reaching it would mean a second read per row.
struct SummaryView {
    id: String,
    kind: String,
    at: String,
    effects: String,
    touched: usize,
}

impl From<OperationSummary> for SummaryView {
    fn from(row: OperationSummary) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            at: html::format_dt(row.committed_at),
            effects: row.effects.join(", "),
            touched: row.touched,
        }
    }
}

#[derive(Template)]
#[template(path = "operations.html")]
struct OperationsTemplate {
    nav: &'static str,
    rows: Vec<SummaryView>,
    /// How many rows this page reads at most. Rendered, not just remembered.
    capped: usize,
}

pub(crate) async fn detail(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Path(id): Path<String>,
) -> Response {
    // 403 here for the same reason as `list`: this is "you may not be served", which must not be rendered
    // as "no such operation". The 404 equivalence below is deliberately narrower — it is for a handle that
    // is foreign, malformed or gone, which the caller *is* entitled to be unable to distinguish.
    let Ok((owner, _read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    // A malformed handle and a foreign one and an absent one are the same 404. The strict record-id
    // parse failing is not a 400: a caller who cannot name the row is not distinguished from one who
    // may not read it.
    let Ok(handle) = alexandria_storage::parse_record_id(&id) else {
        return not_found();
    };
    let repo = MutationRepo::new(server.db.inner());
    let operation = match repo.operation_by_id(&owner, &handle).await {
        Ok(Some(operation)) => operation,
        Ok(None) => return not_found(),
        Err(error) => return unavailable(NAV, "operation", error),
    };
    page(OperationTemplate {
        nav: NAV,
        id: operation.id.clone(),
        kind: operation.kind.clone(),
        actor: operation.actor.clone(),
        at: html::format_dt(operation.committed_at),
        expected: operation.expected_effects.join(", "),
        actual: operation.actual_effects.join(", "),
        before: operation
            .preimages
            .iter()
            .zip(&operation.preimage_records)
            .map(|(image, record)| snapshot_view(image, record))
            .collect::<Vec<_>>(),
        after: operation
            .postimages
            .iter()
            .zip(&operation.postimage_records)
            .map(|(image, record)| snapshot_view(image, record))
            .collect::<Vec<_>>(),
    })
}

/// One snapshot, rendered as the text it describes when it describes a memory.
///
/// Only the four v009 variants exist, and a non-memory snapshot renders an empty `content` with
/// `has_content = false`: a cluster or reminder snapshot has no `content` column, and inventing a string for
/// one would put a number this page did not read in front of the owner as if it were their history. The bool
/// rather than an `Option<String>` is askama's constraint — it renders neither an `Option` nor a borrow of one
/// — and it is the same shape every other debug view in this module uses.
struct SnapshotView {
    kind: String,
    subject: String,
    content: String,
    has_content: bool,
    /// What a non-memory snapshot has instead of text: the relation's endpoints, a cluster's member count and
    /// vector width, a reminder mapping's message.
    detail: String,
    /// The original journal record, not a live read or a reconstruction from summary fields.
    exact: String,
}

fn snapshot_view(snapshot: &SemanticSnapshot, record: &Value) -> SnapshotView {
    let exact = format!("{record:#?}");
    match snapshot {
        SemanticSnapshot::Fact(fact) => {
            let mut view = fact_view(fact, exact);
            view.has_content = true;
            view
        }
        SemanticSnapshot::Edge(edge) => SnapshotView {
            kind: "edge".to_string(),
            exact,
            subject: alexandria_storage::record_id_to_string(&edge.edge),
            content: String::new(),
            has_content: false,
            detail: format!(
                "{}: {} → {} (strength {:.2})",
                edge.kind.edge_type().unwrap_or(edge.kind.table()),
                alexandria_storage::record_id_to_string(&edge.from),
                alexandria_storage::record_id_to_string(&edge.to),
                edge.strength
            ),
        },
        SemanticSnapshot::Cluster(cluster) => SnapshotView {
            kind: "cluster".to_string(),
            exact,
            subject: alexandria_storage::record_id_to_string(&cluster.id),
            content: String::new(),
            has_content: false,
            // Keep the summary compact; the disclosure retains members, centroids and guards verbatim.
            detail: format!(
                "{} members; centroid over {} dimensions",
                cluster.members.len(),
                cluster.centroid.len()
            ),
        },
        SemanticSnapshot::Reminder(reminder) => SnapshotView {
            kind: "reminder mapping".to_string(),
            exact,
            subject: alexandria_storage::record_id_to_string(&reminder.id),
            content: String::new(),
            has_content: false,
            // The reminder snapshot variant carries the *mapping* and never a delivery clock, which is why a
            // history page cannot put an already-delivered reminder back on the queue.
            detail: format!(
                "message {:?}; project {}",
                reminder.message,
                if reminder.project.is_some() {
                    "set"
                } else {
                    "none"
                }
            ),
        },
    }
}

fn fact_view(fact: &FactSnapshot, exact: String) -> SnapshotView {
    SnapshotView {
        kind: "memory".to_string(),
        exact,
        subject: alexandria_storage::record_id_to_string(fact.id.as_record_id()),
        content: fact.content.clone(),
        has_content: false,
        detail: format!(
            "{}; tags {}; confidence {:.2}; {}",
            scope_label(&fact.scope),
            if fact.tags.is_empty() {
                "none".to_string()
            } else {
                fact.tags.join(", ")
            },
            fact.confidence,
            if fact.visibility.deleted {
                "hidden"
            } else if fact.visibility.quarantined_at.is_some() {
                "quarantined"
            } else {
                "live"
            }
        ),
    }
}

fn scope_label(scope: &alexandria_storage::models::MemoryScope) -> String {
    use alexandria_storage::models::MemoryScope;
    match scope {
        MemoryScope::Personal => "personal".to_string(),
        MemoryScope::Unassigned => "unassigned".to_string(),
        MemoryScope::Project(project) => format!(
            "project ({})",
            alexandria_storage::record_id_to_string(project.as_record_id())
        ),
    }
}

#[derive(Template)]
#[template(path = "operation_detail.html")]
struct OperationTemplate {
    nav: &'static str,
    id: String,
    kind: String,
    actor: String,
    at: String,
    expected: String,
    actual: String,
    before: Vec<SnapshotView>,
    after: Vec<SnapshotView>,
}

/// A page that cannot resolve its caller at all, matching `memories.rs` and `clusters.rs`.
fn forbidden() -> Response {
    let mut response = html::error_page(NAV, "this page requires an authenticated owner session");
    *response.status_mut() = axum::http::StatusCode::FORBIDDEN;
    response
}

/// A row this caller may not resolve renders exactly like a row that does not exist.
fn not_found() -> Response {
    let mut response = html::error_page(NAV, "no such operation for this account");
    *response.status_mut() = axum::http::StatusCode::NOT_FOUND;
    response
}
