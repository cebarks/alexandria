//! Task 11's owner-facing surface: the collections views, the edit controls, and human classification.
//!
//! Three partitions of one corpus (**Personal**, **project**, **Unassigned**), the controls to correct a
//! memory, and the queue of scope proposals an agent has formed — with the human decision that is the only
//! thing allowed to act on one.
//!
//! # An agent's judgement is not an edit
//!
//! [`propose`](alexandria_storage::repos::ClassificationRepo::propose) writes a pending row; nothing in
//! this module can turn that row into a moved memory without a person pressing a button here. Application
//! runs through
//! [`apply_batch`](alexandria_storage::repos::ClassificationRepo::apply_batch), which moves **each** memory
//! through Task 10's journal in its own transaction and reports per-entry conflicts, so a stale entry
//! refuses without unwinding the ones that were lawful. There is deliberately no automatic approval path
//! and no quality-auto flag anywhere in these handlers: a proposal about *where* a memory belongs and a
//! judgement about *how good* it is are different consents, and collapsing them would let a quality run
//! move somebody's corpus.
//!
//! # Every write here is the tool's own write
//!
//! The edit and delete controls call [`AlexandriaServer::update_within`] /
//! [`AlexandriaServer::delete_within`] — the same bodies `update_memory` and `delete_memory` reach through
//! the MCP transport — built from a [`RequestScope`] the browser session resolved. That is not
//! convenience: a page and an agent with two notions of "edit this memory" is two notions of atomicity, of
//! what gets journaled, and of who may write. There is one.
//!
//! # The review partition is reached by name
//!
//! Unassigned rows come from [`Boundary::corpus_selection`] with
//! [`ReadSelection::UnassignedReview`](alexandria_storage::models::ReadSelection) spelled at the call site,
//! never from a widened default. **Classification preserves quarantine**: `move_scope` changes scope and
//! nothing else, so approving a proposal about a quarantined memory files it without un-hiding it, and the
//! queue shows the state ([`list_pending`](alexandria_storage::repos::ClassificationRepo::list_pending)) so
//! the owner is not surprised by it later.
//!
//! # CSRF and admin authority are not this module's business
//!
//! `guard` is the single CSRF and session authority (see its module documentation): a mutation reaching a
//! handler here has already presented a token equal to its own cookie and matched the session's. A handler
//! that re-checked would be a second rule to forget, which is why none of these functions mention a token.
//! An administrator sees no page in this module at all — `corpus_selection` resolves an owner population
//! from a browser session, and the admin surface reads content-free aggregates.

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Form, Query, State};
use axum::response::Response;
use serde::Deserialize;
use std::collections::HashMap;

use super::html::{self, error_page, page, unavailable};
use super::web::{Boundary, BrowserSession};
use crate::AlexandriaServer;
use crate::scope_service::ScopeService;
use crate::server::RequestScope;
use alexandria_storage::models::identity::OperationId;
use alexandria_storage::models::mutation::BatchResult;
use alexandria_storage::models::{MemoryScope, ReadSelection, Version};
use alexandria_storage::record_id_to_string;
use alexandria_storage::repos::{
    ClassificationCandidates, ClassificationDestination, ClassificationRepo,
    PreparedClassificationBatch, ScopeProposalRef,
};
use alexandria_storage::repos::{
    FactListQuery, FactSort, PendingProposal, ScopedMemoryRepo, ScopedSessionRepo, SortDir,
};
use surrealdb::types::RecordId;

/// The nav key all three pages share, so one highlight follows the owner across partitions.
const NAV: &str = "collections";

/// The page size the owner's collections listing reads its memories at.
///
/// Not a queue cap: [`list_pending`](alexandria_storage::repos::ClassificationRepo::list_pending) is
/// deliberately uncapped, so the queue carries no truncation flag at all — a note claiming "may not show
/// every entry" would describe a dropped row that cannot happen, and removing the field is cheaper to be
/// true than a flag that has to be kept false.
const LIST_PAGE: usize = 50;

/// Which partition this request asked to look at, from a closed allowlist.
///
/// The selection is this module's decision, not the caller's: the string `?partition=` chooses between
/// three `ReadSelection` values defined here, and no input can name a population the session was not
/// granted — `AccessRepo::select` re-reads the account and workspace behind every one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Partition {
    Personal,
    Project,
    Unassigned,
}

impl Partition {
    fn parse(raw: Option<&String>) -> Self {
        match raw.map(String::as_str) {
            Some("project") => Self::Project,
            Some("unassigned") => Self::Unassigned,
            _ => Self::Personal,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Project => "project",
            Self::Unassigned => "unassigned",
        }
    }

    fn selection(self) -> ReadSelection {
        match self {
            Self::Personal => ReadSelection::One(MemoryScope::Personal),
            // `Assigned` rather than a specific project: the collections page lists what the owner may
            // file into and what they own, and a project-named selection would need a resolver here that
            // `scope_service` already owns for the tools. Cross-project listing is `Memories`' job.
            Self::Project => ReadSelection::Assigned,
            Self::Unassigned => ReadSelection::UnassignedReview,
        }
    }
}

pub(crate) async fn list(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let partition = Partition::parse(params.get("partition"));
    let Ok((owner, read)) = boundary
        .corpus_selection(&session, partition.selection())
        .await
    else {
        return forbidden();
    };
    // The listing pages by offset, like `memories.rs`, and says which page it is showing. The earlier
    // shape read `LIST_PAGE` rows at offset zero and stayed silent about it, which made an owner with more
    // memories than one page hold a "collections" view that quietly was not one.
    let offset = params
        .get("offset")
        .and_then(|text| text.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let query = FactListQuery {
        search: None,
        tag: None,
        // The collections view shows what the owner has, including what they hid from search: this is
        // the page they come to in order to change their mind about a memory, and a hidden row that
        // cannot be un-hidden is a row the owner has lost.
        include_deleted: true,
        include_quarantined: true,
        sort: FactSort::CreatedAt,
        dir: SortDir::Desc,
        limit: LIST_PAGE,
        offset,
    };
    let total = ScopedMemoryRepo::new(server.db.inner(), &owner, &read)
        .count(&query)
        .await
        .unwrap_or(0);
    let rows = ScopedMemoryRepo::new(server.db.inner(), &owner, &read)
        .owner_page(&query)
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return unavailable(NAV, "collections", error),
    };
    // The owner's own project inventory, for the destination selectors below, through the same read the
    // queue page uses. `list_projects` resolves nothing and creates nothing, which is the point: a page
    // that could name an arbitrary project would need its own notion of a project name, and two notions of
    // a name is the drift this slice was reviewed for. A listing that will not read yields an empty
    // selector, and an empty selector is refused at apply time rather than guessed at.
    let projects = destination_inventory(&server, &owner).await;
    let queue = ClassificationRepo::new(server.db.inner(), &owner)
        .list_pending()
        .await;
    let (queue, queue_error) = match queue {
        Ok(queue) => (queue, None),
        // A queue that will not read must not take the corpus listing down with it: the owner can still
        // see and correct their memories, and the page says the other half is unavailable.
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    // The queue views are built before the template struct, because the template *moves* `projects` into
    // itself and the queue rows still need to read it for their destination labels.
    let queue_views: Vec<ProposalView> = queue
        .into_iter()
        .map(|row| ProposalView::from_pending(row, &projects))
        .collect();
    page(CollectionTemplate {
        nav: NAV,
        partition: partition.as_str(),
        csrf: session.csrf().to_string(),
        rows: rows.into_iter().map(MemoryView::from).collect(),
        projects,
        queue: queue_views,
        // No truncation flag on the queue: `list_pending` is uncapped, so any such note would claim a
        // dropped entry that cannot happen. The memories *above* it are paged (`LIST_PAGE`) and say so.
        queue_error,
        note: params.get("note").cloned(),
        error: params.get("error").cloned(),
        // The pager's contract: complete hrefs from the caller, an empty string for a side with no link.
        prev_href: if offset == 0 {
            String::new()
        } else {
            collections_page_url(partition, offset.saturating_sub(LIST_PAGE))
        },
        next_href: if offset + LIST_PAGE < total {
            collections_page_url(partition, offset + LIST_PAGE)
        } else {
            String::new()
        },
        page_summary: format!(
            "Showing {}\u{2013}{} of {total} memories",
            if total == 0 { 0 } else { offset + 1 },
            (offset + LIST_PAGE).min(total),
        ),
    })
}

/// A memory, as the collections row renders it. Text only, plus the two states that change what the
/// controls may honestly promise.
struct MemoryView {
    id: String,
    content: String,
    tags: String,
    confidence: String,
    created: String,
    hidden: bool,
    quarantined: bool,
    /// The partition this row is in *now*, spelled for a person.
    partition: String,
    /// The same partition as the *word* a destination selector posts, for the row's own controls.
    partition_word: String,
    /// The revision the page renders into every control that writes this row.
    ///
    /// This is the value a stale conflict is measured against. A form that re-read the row at POST time
    /// would always agree with itself, the advertised "reload and try again" branch would be unreachable
    /// by construction, and the guard the owner was promised would exist only in the sentence.
    revision: String,
}

impl From<alexandria_storage::repos::OwnedRow> for MemoryView {
    fn from(row: alexandria_storage::repos::OwnedRow) -> Self {
        Self {
            id: row.id.as_ref().map(record_id_to_string).unwrap_or_default(),
            content: row.content,
            tags: row.tags.join(", "),
            confidence: format!("{:.2}", row.confidence),
            created: html::format_dt(row.created_at),
            hidden: row.deleted,
            quarantined: row.quarantined_at.is_some(),
            partition: partition_word(row.scope.as_deref(), row.project.as_ref()),
            partition_word: partition_selector_value(row.scope.as_deref()),
            revision: row
                .semantic_revision
                .map(|value| value.max(0).to_string())
                .unwrap_or_else(|| "0".to_string()),
        }
    }
}

/// The partition a stored `scope`/`project` pair spells, as one word plus the owner's own name for it.
///
/// The handle is shown, because the selector below posts what the page rendered: an owner choosing between
/// two projects by opaque id is choosing by name, and the name is the owner's own `display_name` or the
/// canonical alias the resolver already printed. Never a foreign handle — `owner_page` selected under the
/// caller's population, so every row here is the caller's, and a label that could name another account's
/// project would be a leak in the one view whose whole job is naming partitions.
fn partition_word(scope: Option<&str>, project: Option<&RecordId>) -> String {
    match (scope, project) {
        (Some("project"), Some(handle)) => {
            format!("project {}", record_id_to_string(handle))
        }
        (Some("unassigned"), _) => "unassigned (review)".to_string(),
        _ => "personal".to_string(),
    }
}

/// How many destinations one page offers a human. The tool surface caps a *page* of projects at
/// `MAX_SCOPE_PAGE_LIMIT`; a selector that outgrew this would be a page nobody can read, and the resolver
/// still accepts a name typed into the text field beside it.
const MAX_DESTINATIONS: u32 = 100;

/// The destination value a row's own selector should show as current.
///
/// The *project* handle is deliberately not echoed here: a prefilled `<select>` cannot know which of the
/// owner's projects the row is in without posting the handle alongside, and the form beside each row offers
/// the whole owned inventory for exactly that reason. `project` is the truthful current word, and the
/// project column already names the row's partition in full.
fn partition_selector_value(scope: Option<&str>) -> String {
    match scope {
        Some("project") => "project".to_string(),
        Some("unassigned") => "unassigned".to_string(),
        _ => "personal".to_string(),
    }
}

/// One destination a human may choose from: the handle to post and the label to read.
struct DestinationChoice {
    handle: String,
    label: String,
}

/// One pending proposal, as the queue renders it.
struct ProposalView {
    id: String,
    revision: String,
    memory: String,
    destination: String,
    reason: String,
    quarantined: bool,
}

impl ProposalView {
    /// One pending row as the queue renders it.
    ///
    /// `projects` is the owner's own inventory, matched by handle: a destination that reads `project` with
    /// no name next to it is a decision the owner cannot actually review, and the handle is already in the
    /// row, so labelling it costs nothing but the lookup the page is showing anyway.
    fn from_pending(row: PendingProposal, projects: &[DestinationChoice]) -> Self {
        Self {
            id: row.reference.id,
            revision: row.reference.revision.get().to_string(),
            memory: record_id_to_string(&row.fact),
            destination: destination_spelled(&row.destination, projects),
            reason: row.reason.unwrap_or_default(),
            quarantined: row.quarantined,
        }
    }
}

/// The destination as a word an owner reads, never a row handle.
///
/// A project destination is reported as `project` without its key: the key is an opaque identifier that
/// means nothing on this page, and printing it next to a name the owner typed invites the two being
/// confused for one thing.
/// The owner's own project inventory, for a destination selector.
///
/// Both owner-facing pages read it through this one call, so a project that is selectable on one page and
/// unlabelled on the other cannot happen: the same read, the same cap, the same fallback. An empty answer
/// is a selector with no project options, and a form that posts a project name the resolver cannot find is
/// refused — not guessed.
async fn destination_inventory(
    server: &AlexandriaServer,
    owner: &alexandria_storage::access::OwnerContext,
) -> Vec<DestinationChoice> {
    ScopeService::new(&server.db)
        .list_projects(owner, None, MAX_DESTINATIONS)
        .await
        .map(|page| {
            page.entries
                .into_iter()
                .map(|entry| DestinationChoice {
                    handle: alexandria_storage::project_handle::format_project_handle(
                        entry.project.id.as_record_id(),
                    ),
                    label: entry
                        .project
                        .display_name
                        .clone()
                        .or_else(|| Some(entry.project.normalized_alias.clone()))
                        .unwrap_or_else(|| "project".to_string()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn destination_spelled(destination: &MemoryScope, projects: &[DestinationChoice]) -> String {
    match destination {
        MemoryScope::Personal => "personal".to_string(),
        MemoryScope::Unassigned => "unassigned (review)".to_string(),
        MemoryScope::Project(project) => {
            let handle =
                alexandria_storage::project_handle::format_project_handle(project.as_record_id());
            // The label is the owner's own string for their own project, and the handle stays visible
            // beside it because the handle is what the form posts back and what a conflict names.
            match projects.iter().find(|choice| choice.handle == handle) {
                Some(choice) => format!("project {} ({})", choice.label, handle),
                None => format!("project {handle}"),
            }
        }
    }
}

#[derive(Template)]
#[template(path = "collections.html")]
struct CollectionTemplate {
    nav: &'static str,
    partition: &'static str,
    /// The owner's own projects, for the two destination selectors on this page.
    projects: Vec<DestinationChoice>,
    /// The token the guard reads out of each form body. Handlers never check it themselves — `guard` is
    /// the single CSRF authority — but a form without it is a form the guard refuses, so the page must
    /// carry it.
    csrf: String,
    rows: Vec<MemoryView>,
    queue: Vec<ProposalView>,
    /// Rendered with `{% if let Some(..) %}`, which is this crate's existing convention for an optional
    /// message (`account.html`). A bare `{% if %}` over an `Option<String>` is not: askama's `as_bool` has
    /// no `String` impl, so `{% if note %}` on a `String` is a compile error rather than a truthiness test.
    queue_error: Option<String>,
    note: Option<String>,
    error: Option<String>,
    prev_href: String,
    next_href: String,
    page_summary: String,
}

/// One page of the owner's collections, with the partition kept across the hop.
///
/// The partition is part of the page's identity, so a pager that dropped `?partition=` would walk an owner
/// from the project view into the Personal one without saying so.
fn collections_page_url(partition: Partition, offset: usize) -> String {
    format!(
        "/debug/collections?partition={}&offset={}",
        partition.as_str(),
        offset
    )
}

/// The edit form: text, tags, a tag clear, or a soft delete.
///
/// Every field a plain HTML form posts is `Option`: a browser sends an *empty* value for a text input
/// nobody typed in, so a `String` field that is always present cannot be told apart from one the owner
/// actually filled in. The distinction is the whole of H2 — see [`edit`].
#[derive(Debug, Deserialize)]
pub(crate) struct EditForm {
    id: String,
    /// The revision the page rendered for this row, so a stale edit conflicts instead of overwriting.
    #[serde(default)]
    revision: Option<String>,
    content: Option<String>,
    tags: Option<String>,
    /// The explicit clear-tags control. A checkbox that a browser sends as `clear_tags=on` **only when it
    /// is checked** and not at all when it is not, which is what makes "wipe the tags" a thing an owner
    /// asked for rather than a thing that happens when they edited the text next to it.
    #[serde(rename = "clear_tags")]
    clear_tags: Option<String>,
    /// Which button was pressed. `delete` hides the memory; anything else saves.
    action: Option<String>,
}

/// One form posts both verbs because a plain HTML form has no other way to say which submit button fired.
///
/// `destination` is the caller's *word*, and `project` is a name or opaque id from the owner's own
/// inventory: the page never mints a project handle, and resolution happens in
/// [`ScopeService::resolve_destination`], the same door the tools use. A second normalizer in a page
/// handler is precisely the drift two notions of a project name cause, so there is one.
#[derive(Debug, Deserialize)]
pub(crate) struct MoveForm {
    id: String,
    destination: String,
    project: Option<String>,
    /// The revision the row carried when this page rendered. Bound to the *rendered* value on purpose:
    /// re-reading the current version at POST time would make the advertised stale-conflict unreachable
    /// by construction, and a control that cannot conflict is a control that cannot protect anything.
    #[serde(default)]
    revision: Option<String>,
}

/// The queue decision, read from the **whole guard-restored body**, not from `Form<T>`.
///
/// A real browser posts one `selected` pair per checked box, and two of them is a body serde_urlencoded
/// 0.7 hands to serde as a map with a repeated field — which serde_derive refuses with
/// `duplicate field \`selected\``, giving a 422 before any handler sees the request. `Form<DecideForm>`
/// therefore worked for exactly one checkbox and for a comma-joined string no browser ever produces from
/// separate boxes; the previous shape of this type is the bug H1 names. The values come from
/// [`web::form_fields`], the same dependency-free decoder `guard` already trusts with the CSRF token, and
/// the body it reads is the one the guard handed back whole after spending the token.
pub(crate) struct DecideForm {
    selected: Vec<String>,
    decision: String,
}

impl DecideForm {
    fn from_body(body: &Bytes) -> Self {
        Self {
            selected: super::web::form_fields(body, "selected"),
            decision: super::web::form_field(body, "decision").unwrap_or_default(),
        }
    }
}

/// Change one pending proposal's destination, under its owner's hand.
///
/// The revision the page rendered is the guard: `revise_destination` bumps the proposal's own revision, so
/// every reference minted before the change stops resolving and a decision somebody clicked against the old
/// destination cannot land afterwards. That invalidation is the point — an approval and a changed opinion
/// about the same row are two judgements, and the store must say which one it honoured.
#[derive(Debug, Deserialize)]
pub(crate) struct ReviseForm {
    id: String,
    revision: String,
    destination: String,
    project: Option<String>,
}

pub(crate) async fn edit(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<EditForm>,
) -> Response {
    let Ok((owner, read)) = boundary
        .corpus_selection(&session, Partition::Project.selection())
        .await
    else {
        return forbidden();
    };
    let scope = RequestScope::issued_for_debug(owner, read);
    // A foreign id and an absent one are refused by the same predicate the tool uses, before any embed.
    if form.action.as_deref() == Some("delete") {
        // The review partition's rows are the owner's to clean up and nobody else's: the browser path is
        // the human path the owner decision named, and it reaches the *same* journaled delete body with a
        // reach the tools cannot ask for. See [`AlexandriaServer::delete_reviewed_within`].
        return match server.delete_reviewed_within(&scope, &form.id).await {
            Ok(_) => redirect("hidden — the change is in your history"),
            Err(error) => redirect_failed(error.to_string()),
        };
    }
    // H2, in one line: **an empty tags field is no change.** A browser posts `tags=` for a text input the
    // owner did not touch, and mapping that to `Some(vec![])` wipes every tag on the memory the moment
    // somebody corrects its text. Preservation is the default; clearing is a control the owner presses.
    let tags = if presents_clear(form.clear_tags.as_deref()) {
        Some(vec![])
    } else {
        form.tags
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(split_tags)
    };
    let params = crate::tools::UpdateMemoryParams {
        id: form.id.clone(),
        content: form.content.filter(|text| !text.trim().is_empty()),
        tags,
        confidence: None,
    };
    // The revision the listing rendered for this row, **required**. A form the page never rendered cannot
    // name one, and an edit without a conflict guard is an edit that overwrites whatever it raced with;
    // refusing is the only answer that keeps the page's own promise true.
    let expected = match form.revision.as_deref().map(str::trim) {
        Some(text) => match text.parse::<u64>() {
            Ok(value) => Version::new(value),
            Err(_) => {
                return redirect_failed("reload the page and edit again".to_string());
            }
        },
        None => return redirect_failed("reload the page and edit again".to_string()),
    };
    match server
        .update_within_at(&scope, params, Some(expected))
        .await
    {
        Ok(_) => redirect("saved — your history shows the exact before-state"),
        Err(error) if is_stale(&error) => redirect_failed(
            "that memory changed since this page loaded; reload and edit again".to_string(),
        ),
        Err(error) => redirect_failed(error.to_string()),
    }
}

/// Whether a refusal is the staleness class.
///
/// Read by downcast, not by string matching, because the answer decides what the owner is told to do —
/// reload, or stop and report — and the service boundary sanitises its own chain, so a rendered message is
/// a fixed tag that merely happens to name the class today.
fn is_stale(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<alexandria_storage::error::StorageError>()
        .is_some_and(|storage| matches!(storage, alexandria_storage::error::StorageError::Stale))
        || error
            .downcast_ref::<crate::error::ServiceError>()
            .is_some_and(|service| matches!(service, crate::error::ServiceError::Stale))
}

/// Whether the form's clear-tags control was actually pressed.
///
/// Checked-and-absent are distinguishable here because a browser sends the name only when the box is
/// checked; a stray `clear_tags=` (empty value) is *not* a press, because that is what a text input left
/// empty looks like, and the whole rule this function guards is that an empty field means nothing happened.
fn presents_clear(raw: Option<&str>) -> bool {
    matches!(raw, Some(value) if !value.trim().is_empty())
}

/// Moves one memory between partitions under the owner's own authority.
///
/// Personal, a project of the owner's own inventory, and Unassigned are all reachable: filing into the
/// review partition is a *placement* act, and the earlier claim that it was inherently an operator's
/// quarantine act is removed — quarantine is an access state no move writes, and the partition is what a
/// memory's aboutness opinion says it belongs to. `move_scope` changes scope and visibility not at all,
/// so a move neither hides a live memory nor un-hides a quarantined one.
///
/// The cluster consequence is real and not deferred: the cluster the memory leaves gets the centre of its
/// survivors, inside this transaction, under that cluster's own guards; the cluster it joins (or the one
/// it forms) is chosen by the server's ranking against the destination partition, because the join
/// threshold is configured policy and the arithmetic is the engine's, and a page handler that guessed at
/// either would be a second notion of "the nearest cluster".
pub(crate) async fn move_one(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<MoveForm>,
) -> Response {
    let Ok((owner, read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let scope = RequestScope::issued_for_debug(owner, read);
    // The rendered revision, or the honest refusal. A form the page never rendered (a hand-built post with
    // no revision) cannot be given a conflict guard, so this asks the caller to reload rather than silently
    // falling back to "whatever the row says now" — which is the reading that makes the guard unobservable.
    let Some(expected) = form
        .revision
        .as_deref()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(Version::new)
    else {
        return redirect_failed("reload the page and try that move again".to_string());
    };
    match server
        .move_within(
            &scope,
            &form.id,
            &form.destination,
            form.project.as_deref(),
            expected,
        )
        .await
    {
        Ok(answer) => {
            let status = answer["status"].as_str().unwrap_or_default();
            if status == "already_there" {
                redirect_failed("that memory is already in the partition you chose".to_string())
            } else {
                redirect("moved — the id is unchanged and its history stayed where it was")
            }
        }
        // `Stale` is the interesting answer and is kept distinct from a probe: the memory moved or was
        // edited since this page was rendered, which is the owner's cue to reload rather than to retry.
        Err(crate::error::ServiceError::Stale) => redirect_failed(
            "that memory changed since this page loaded; reload and try again".to_string(),
        ),
        Err(crate::error::ServiceError::NotFound) => not_found("no memory of yours has that id"),
        Err(error) => redirect_failed(owner_failure(&error)),
    }
}

/// What to tell an owner whose write was refused, in terms of what they can do next.
///
/// `ServiceError`'s own `Display` is a fixed tag written for a log and for the JSON-RPC error path, where
/// naming the class is the point. A redirect is read by a person, and "retryable bookkeeping conflict"
/// names no act they can take: it is the internal variant spelled out, and an owner who cannot tell whether
/// to reload, wait, or stop and report is likelier to click the same button again than to do any of those.
/// The retryable class therefore gets its own arm here — reload, because the cluster bookkeeping the move
/// was ranked against has moved, and a fresh page re-ranks it — while the classes the handlers already
/// render distinctly (`Stale`, `NotFound`) keep their existing wording at the call sites above this one.
///
/// Every other class falls through to storage's sanitized text, which names no table, id or bound value.
fn owner_failure(error: &crate::error::ServiceError) -> String {
    match error {
        crate::error::ServiceError::RetryableBookkeepingConflict => {
            "the cluster bookkeeping this move was planned against changed while it was being written; \
             reload the page and make the move again"
                .to_string()
        }
        other => other.to_string(),
    }
}

/// Applies or dismisses the proposals the owner selected.
///
/// Approving runs one transaction per entry through the journal and reports **per-entry** conflicts, which
/// is the whole reason a batch can be offered at all: a stale entry cannot overwrite a later move, and the
/// entries that were lawful still landed. Dismissing writes no journal row, because it moves no memory.
pub(crate) async fn decide(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    body: Bytes,
) -> Response {
    let form = DecideForm::from_body(&body);
    let Ok((owner, read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let scope = RequestScope::issued_for_debug(owner.clone(), read);
    let selected: Vec<ScopeProposalRef> = form
        .selected
        .iter()
        .filter_map(|pair| parse_reference(pair.as_str()))
        .collect();
    if selected.is_empty() {
        return redirect_failed("select at least one proposal".to_string());
    }
    // Everything a decision is about has to be a decision the page rendered: a body that named three boxes
    // and yielded one parseable reference is a form somebody tampered with or a page that went stale under
    // them, and quietly applying the one that survived would be a different act from the one they took.
    if selected.len() != form.selected.len() {
        return redirect_failed(
            "some of what you selected is no longer a proposal; reload the page".to_string(),
        );
    }
    let classify = ClassificationRepo::new(server.db.inner(), &owner);
    match form.decision.as_str() {
        "approve" => {
            // UUID, like every other writer: `apply-browser:<seconds>` was collision-shaped for two
            // approvals in one second and needed nothing to be a problem only because the per-entry key
            // already carries the proposal. No id in this server should be a timestamp in disguise.
            let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
            // Capture first, then rank the frozen candidates once, then seal before anything executes:
            // the ranking is the engine's arithmetic and the join threshold is this server's policy, so
            // it happens here — but on the frozen fact, never on a re-read that could disagree with what
            // the owner approved.
            match classify.prepare_batch(&selected).await {
                Ok(candidates) => {
                    match seal_classification_batch(&server, &scope, candidates).await {
                        Ok(batch) => {
                            match classify.apply_prepared_batch(&batch, &operation).await {
                                Ok(result) => redirect(&approval_note(&result)),
                                Err(error) => redirect_failed(owner_failure(&error.into())),
                            }
                        }
                        Err(error) => redirect_failed(owner_failure(&error.into())),
                    }
                }
                Err(error) => redirect_failed(owner_failure(&error.into())),
            }
        }
        "dismiss" => {
            use alexandria_storage::error::StorageError;
            let mut dismissed = 0;
            let mut refused = 0;
            for (index, reference) in selected.iter().enumerate() {
                match classify.dismiss(reference).await {
                    Ok(()) => dismissed += 1,
                    // A definite refusal — decided elsewhere, or a citation that no longer resolves.
                    Err(StorageError::Stale)
                    | Err(StorageError::NotFound)
                    | Err(StorageError::InvalidInput(_)) => refused += 1,
                    // An unexpected failure stops the loop: what already dismissed stands, the failing
                    // entry is unconfirmed, and the rest are not attempted. A dismissal loop that kept
                    // going would report a count for entries it never tried.
                    Err(_) => {
                        return redirect(&format!(
                            "{dismissed} dismissed, {refused} already decided, could not confirm one \
                             entry; {} not attempted — reload before retrying",
                            selected.len() - index - 1
                        ));
                    }
                }
            }
            redirect(&format!("{dismissed} dismissed, {refused} already decided"))
        }
        other => redirect_failed(format!("unknown decision {other:?}")),
    }
}

/// Change one proposal's destination, from the queue.
pub(crate) async fn revise(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<ReviseForm>,
) -> Response {
    let Ok((owner, _read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let Some(revision) = form.revision.trim().parse::<u64>().ok().map(Version::new) else {
        return redirect_failed("reload the queue and choose again".to_string());
    };
    // Resolved through the one door the tools use, so a name nobody owns is a refusal and not a new
    // partition: `establish_project` is the only path that creates, and this is not it.
    let (destination, _) = match ScopeService::new(&server.db)
        .resolve_destination(&owner, &form.destination, form.project.as_deref())
        .await
    {
        Ok(resolved) => resolved,
        Err(error) => return redirect_failed(error.to_string()),
    };
    let reference = ScopeProposalRef {
        id: form.id.clone(),
        revision,
    };
    match ClassificationRepo::new(server.db.inner(), &owner)
        .revise_destination(reference, destination)
        .await
    {
        Ok(changed) => redirect(&format!(
            "destination changed — this proposal is now revision {}",
            changed.revision.get()
        )),
        Err(alexandria_storage::error::StorageError::Stale) => redirect_failed(
            "that proposal changed since this page loaded; reload and choose again".to_string(),
        ),
        Err(error) => redirect_failed(error.to_string()),
    }
}

/// Rank each frozen candidate's destination once, in input order, and seal the batch.
///
/// The ranking happens on the candidate's **frozen** fact — never a re-read — so what is ranked is what
/// the owner approved. A typed, definite failure (`Stale`, `NotFound`, an invalid request) is a safe
/// refusal the owner can act on; an unexpected failure is logged privately and leaves the entry
/// **unconfirmed**, which stops the seal: nothing after it is attempted, and nothing before it executes
/// either. Execution happens only after the whole batch is sealed.
async fn seal_classification_batch(
    server: &AlexandriaServer,
    scope: &RequestScope,
    candidates: ClassificationCandidates,
) -> Result<PreparedClassificationBatch, alexandria_storage::error::StorageError> {
    use alexandria_storage::error::StorageError;
    use alexandria_storage::models::BatchConflictReason as Reason;
    let mut destinations: Vec<ClassificationDestination> = Vec::new();
    for candidate in candidates.candidates() {
        // An `Unassigned` destination ranks as an explicit ungrouped filing; the ranking call itself
        // decides that before any read.
        match server
            .prepare_arrival(scope, candidate.fact(), candidate.destination())
            .await
        {
            Ok(reconciliation) => destinations.push(ClassificationDestination::Chosen {
                reference: candidate.reference().clone(),
                arriving: reconciliation.arriving,
            }),
            // A definite, typed failure is a safe refusal — the owner is told which entry and why.
            Err(StorageError::Stale) => destinations.push(ClassificationDestination::Refused {
                reference: candidate.reference().clone(),
                reason: Reason::Stale,
            }),
            Err(StorageError::NotFound) => destinations.push(ClassificationDestination::Refused {
                reference: candidate.reference().clone(),
                reason: Reason::NotFound,
            }),
            Err(StorageError::InvalidInput(_)) => {
                destinations.push(ClassificationDestination::Refused {
                    reference: candidate.reference().clone(),
                    reason: Reason::InvalidRequest,
                })
            }
            // Everything else could not be *established*, and guessing is not ranking: the entry is
            // unconfirmed, which stops the seal before anything executes.
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    proposal = %candidate.reference().id,
                    "destination ranking failed; leaving the entry unconfirmed"
                );
                destinations.push(ClassificationDestination::Unconfirmed {
                    reference: candidate.reference().clone(),
                });
                return candidates.seal(&destinations);
            }
        }
    }
    candidates.seal(&destinations)
}

/// What the decide page tells the owner a batch did.
///
/// Refusals are definite and labeled; a stop is never called a refusal and never called a rollback —
/// entries that already moved keep their result, which is why the honest phrasing is "could not
/// confirm", not "failed". The not-attempted count tells the owner the safe next action: reload before
/// retrying, because the page's citations are stale the moment anything decided.
fn approval_note(result: &BatchResult) -> String {
    let mut note = format!("{} moved", result.applied_count());
    if !result.conflicts.is_empty() {
        note.push_str(&format!(
            ", {} refused: {}",
            result.conflicts.len(),
            result
                .conflicts
                .iter()
                .map(conflict_label)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if let Some(stop) = &result.stopped {
        note.push_str(&format!(
            ", could not confirm one entry; {} not attempted — reload before retrying",
            stop.unattempted.len()
        ));
    }
    note
}

/// The owner's **explicit whole-session archive**: the one page in this module that shows a session's own
/// summary and tags.
///
/// Task 11's rule is a pair, and both halves are enforced here. A session is allowed to be *mixed* —
/// memories stored during one conversation can belong to several partitions, and a scope move deliberately
/// leaves the session link where it was — so the prose that describes the whole session cannot honestly be
/// served into one partition's view, and a partition view that showed it would be saying something about
/// memories it is not entitled to read. The two ways to get that wrong are a route that widens a scoped
/// list, and a scoped list that renders a field the storage read was supposed to withhold. This is the
/// first shape, chosen: a separate private route, on the storage read that was built for it
/// ([`ScopedSessionRepo::archive`]), which projects the prose and **no memory body at all** — the one number
/// it carries is how many memories are still live.
///
/// The mixed-scope label is the second half of the disclosure. `scopes` comes from the same live traversal as
/// `live_memory_count`, so the badge and the count cannot disagree about what is in the session, and an owner
/// reading "personal + project" is being told *why* this page is not the project page.
pub(crate) async fn archive(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Ok((owner, _read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    // One row more than the page, and the extra row decides what the page admits about itself. The alternative
    // — read `LIST_PAGE` and claim `truncated: true` unconditionally — told an owner with three sessions that
    // older ones were waiting when there were none, which is the false statement this fix's review counted
    // against L7 rather than with it. Reading `LIST_PAGE + 1` is not a pagination framework: it is the same
    // single read with one more row, and it is exactly what the claim needs to be either true or false.
    let rows = ScopedSessionRepo::new(server.db.inner(), owner)
        .archive(LIST_PAGE + 1, 0)
        .await;
    match rows {
        Ok(rows) => {
            let truncated = rows.len() > LIST_PAGE;
            let rows: Vec<_> = rows.into_iter().take(LIST_PAGE).collect();
            page(ArchiveTemplate {
                nav: NAV,
                sessions: rows
                    .into_iter()
                    .map(|row| {
                        let mixed = row.scopes.len() > 1;
                        ArchiveSessionView {
                            external_id: row.external_id,
                            agent: row.agent_id.unwrap_or_else(|| "unknown agent".to_string()),
                            ended: html::format_dt(row.ended_at),
                            summary: row.summary.unwrap_or_default(),
                            tags: row.tags.join(", "),
                            live_memory_count: row.live_memory_count,
                            scopes: row.scopes.join(" · "),
                            mixed,
                        }
                    })
                    .collect(),
                note: params.get("note").cloned(),
                error: params.get("error").cloned(),
                // Now derived, not asserted: the page says it is truncated only when a further row exists
                // behind it. `list_pending`'s queue stays uncapped and says nothing, which remains the truth of
                // that read (L7).
                truncated,
            })
        }
        Err(error) => unavailable(NAV, "session archive", error),
    }
}

struct ArchiveSessionView {
    external_id: String,
    agent: String,
    ended: String,
    summary: String,
    tags: String,
    live_memory_count: i64,
    scopes: String,
    mixed: bool,
}

#[derive(Template)]
#[template(path = "archive.html")]
struct ArchiveTemplate {
    nav: &'static str,
    sessions: Vec<ArchiveSessionView>,
    note: Option<String>,
    error: Option<String>,
    truncated: bool,
}

// -- helpers -----------------------------------------------------------------------------------------

/// A `fact` handle only. A well-formed id in another table is refused the same way an absent one is, so
/// the form cannot be used to aim a write at a `raw` or `cluster` row.
/// How a record handle crosses a browser boundary.
///
/// **The round trip is display-form in, typed parse back** — `record_id_to_string` renders the handle the
/// page shows, and `RecordId::parse_simple` reads it back. `AGENTS.md` warns that the display form is *not*
/// a parser-safe SQL round trip (numeric and quoted opaque keys render ambiguously), and that warning is
/// about splicing the rendering into a statement, which nothing here does: the parsed value is only ever
/// **bound** (`type::record('table', $key)` with a typed key), never interpolated, so a key whose display
/// form is ambiguous degrades into a refusal at the row predicate, not into a write against the wrong row.
/// A *typed* id would be strictly better on these boundaries, and the reason it is not used is that the
/// `<input>`/`<select>` value a browser posts is text by construction; what this comment pins is that the
/// text is parsed to a typed handle before it is bound, and that no SQL string is ever built from it.
///
/// One `id:revision` pair from the form, back into the reference the repo cites.
///
/// The revision rides with the id because a stale approval must refuse rather than adopt: it is the number
/// `revise_destination` moved, and applying an old revision over a newer one is the failure
/// `stale_batch_entry_cannot_overwrite_a_later_move` measures at the storage layer.
fn parse_reference(pair: &str) -> Option<ScopeProposalRef> {
    // Split from the **end**. A record handle is itself `table:key`, so `split_once(':')` on
    // `classification_proposal:abc123:0` yields the id `classification_proposal` and the revision
    // `abc123:0` — which never parses, so every reference was silently dropped and a lawful decision
    // answered `select at least one proposal`. The first version of this function was caught by
    // `dismissing_a_proposal_decides_it_without_moving_anything`, not by reading it.
    let (id, revision) = pair.rsplit_once(':')?;
    let revision = revision.parse::<u64>().ok()?;
    Some(ScopeProposalRef {
        id: id.to_string(),
        revision: Version::new(revision),
    })
}

fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// Post/redirect/get, so a refreshed page never re-posts an edit.
///
/// The message travels as a query parameter because the listing renders it verbatim into a status line and
/// nothing else; askama escapes it at render, which is why an error string that carries a stored value
/// cannot become markup here.
fn redirect(note: &str) -> Response {
    see_other(format!("/debug/collections?note={}", urlencoded(note)))
}

fn redirect_failed(error: String) -> Response {
    see_other(format!("/debug/collections?error={}", urlencoded(&error)))
}

fn see_other(location: String) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = axum::http::StatusCode::SEE_OTHER;
    response.headers_mut().insert(
        axum::http::header::LOCATION,
        location
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("/debug/collections")),
    );
    response
}

fn urlencoded(value: &str) -> String {
    // Percent-encode by hand rather than pull a URL crate into this crate for one query value: the set
    // that can appear in these messages is ASCII prose plus a few reserved bytes.
    //
    // A space is **not** a legal byte in a query component, so it is encoded like everything else outside
    // the unreserved set — the previous version let `b' '` through, which produced a `Location` header
    // whose value contained a raw space. Browsers tolerate it and so did every test that only looked for a
    // substring, which is why the deviation was invisible; a proxy or a strict URL parser is the consumer
    // that would have broken first.
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A refused batch entry, spelled for a person rather than as a debug enum.
///
/// The reason label is generated here instead of via `Display` is that `BatchConflictReason` is a closed
/// enum with no display impl on purpose — the storage layer reports the class, and only this surface has
/// to say what an owner should do about it.
fn conflict_label(conflict: &alexandria_storage::models::BatchConflict) -> String {
    use alexandria_storage::models::BatchConflictReason as Reason;
    let why = match conflict.reason {
        Reason::Stale => "the memory changed since this was suggested",
        Reason::NotFound => "no memory of yours has that id",
        Reason::InvalidDestination => "the destination is not yours",
        Reason::Decided => "already approved or dismissed",
        Reason::RetryExhausted => "another change kept racing this one",
        Reason::InvalidRequest => "this entry cannot be filed as asked",
    };
    format!("{why} ({})", conflict.proposal)
}

fn forbidden() -> Response {
    let mut response = error_page(NAV, "this page requires an authenticated owner session");
    *response.status_mut() = axum::http::StatusCode::FORBIDDEN;
    response
}

fn not_found(message: &str) -> Response {
    let mut response = error_page(NAV, message);
    *response.status_mut() = axum::http::StatusCode::NOT_FOUND;
    response
}

/// The queue on its own page, for an owner who wants to work through decisions rather than the corpus.
pub(crate) async fn classification(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
) -> Response {
    let Ok((owner, _read)) = boundary.corpus(&session).await else {
        return forbidden();
    };
    let queue = ClassificationRepo::new(server.db.inner(), &owner)
        .list_pending()
        .await;
    let projects = destination_inventory(&server, &owner).await;
    match queue {
        Ok(queue) => {
            let proposals: Vec<ProposalView> = queue
                .into_iter()
                .map(|row| ProposalView::from_pending(row, &projects))
                .collect();
            page(QueueTemplate {
                nav: NAV,
                csrf: session.csrf().to_string(),
                projects,
                proposals,
            })
        }
        Err(error) => unavailable(NAV, "classification queue", error),
    }
}

#[derive(Template)]
#[template(path = "classification.html")]
struct QueueTemplate {
    nav: &'static str,
    csrf: String,
    /// The owner's own projects, offered both as the label under each destination and as the selector that
    /// changes it. A queue page without them would ask an owner to approve a move to a bare row handle.
    projects: Vec<DestinationChoice>,
    proposals: Vec<ProposalView>,
}

#[cfg(test)]
#[path = "collections_pinned_move_tests.rs"]
mod pinned_move_tests;

#[cfg(test)]
mod tests {
    //! The owner workflow, end to end, as a signed-in browser actually performs it.
    //!
    //! Task 11's acceptance is not "the routes exist" — it is that a person can correct a memory, move it,
    //! decide what an agent suggested, and then **read back exactly what changed**. Every case here goes
    //! through the real `router_secured`, through the guard, with a cookie a real sign-in produced, and
    //! asserts against the **store** rather than against a rendered string wherever the store can answer: a
    //! response that describes a journal row is not evidence that one exists.
    //!
    //! What this is **not**: a substitute for a real browser. These are HTTP requests with form bodies. A
    //! visual check that the pages are usable and that a button does what it is labelled is a different act,
    //! and the parent's native-browser pass over a disposable store is the one that can claim it
    //! (`slice-1-report.md` §17.3). Every "nothing happened" assertion is paired with a control that *does*
    //! happen on the same fixture, because an empty page is also what a broken fixture reports.

    use super::*;
    use alexandria_storage::models::{FactId, Version};
    use alexandria_storage::repos::{ClassificationRepo, MutationRepo};
    use axum::http::StatusCode;
    use serde_json::Value as Json;

    use super::super::test_support::{AuthenticatedTestApp, authenticated_on_server, test_server};

    async fn numbers(app: &AuthenticatedTestApp, sql: &str) -> Vec<i64> {
        let mut response = app.db.inner().query(sql).await.unwrap().check().unwrap();
        let rows: Vec<Json> = response.take(0).unwrap();
        rows.iter().filter_map(|row| row["n"].as_i64()).collect()
    }

    /// The store's answer to "how many rows match", where an empty set is a refusal to answer rather than
    /// a zero. `GROUP ALL` over nothing returns **no row**, which is the same *number* as zero and a
    /// different piece of evidence, so a case that means "there are none" says so and one that means "the
    /// query found nothing at all" fails loudly here.
    async fn none(app: &AuthenticatedTestApp, sql: &str) {
        match numbers(app, sql).await.as_slice() {
            [] => {}
            [only] => assert_eq!(*only, 0, "expected no matching rows: {sql}"),
            other => panic!("expected an empty count, got {other:?} for {sql}"),
        }
    }

    /// The store's own answer to "how many rows match", as a single number, failing loudly when the query
    /// returned nothing at all (`GROUP ALL` on an empty set returns no row, which is the same *number* as
    /// zero but not the same evidence — so the call site says which it means).
    async fn one(app: &AuthenticatedTestApp, sql: &str) -> i64 {
        match numbers(app, sql).await.as_slice() {
            [only] => *only,
            other => panic!("expected exactly one count row, got {other:?} for {sql}"),
        }
    }

    /// One pending proposal for `fact`, minted at the repository.
    ///
    /// `propose` is Task 8's tool and already has transport coverage; what is under test here is the
    /// *decision*, so the proposal is fixture rather than subject.
    async fn propose(app: &AuthenticatedTestApp, fact: &str) -> String {
        let handle = RecordId::parse_simple(fact).expect("a fixture fact id");
        let reference = ClassificationRepo::new(app.db.inner(), &app.owner)
            .propose(
                &FactId::try_from(handle).expect("a fact id"),
                Version::new(0),
                MemoryScope::Personal,
                Some("a fixture suggestion"),
            )
            .await
            .expect("a proposal");
        format!("{}:{}", reference.id, reference.revision.get())
    }

    #[tokio::test]
    async fn exact_history_disclosures_show_stored_values_without_widening_access() {
        let app = authenticated_on_server(test_server().await).await;
        let a = app.own_project("history-a").await;
        let b = app.own_project("history-b").await;
        let project = |id: &str| {
            MemoryScope::Project(RecordId::parse_simple(id).unwrap().try_into().unwrap())
        };
        let mover = app
            .seed_personal("history <script>history-danger</script> & exact values")
            .await;
        let stayer = app.seed_personal("history source survivor").await;
        let hidden = app
            .seed_quarantined_unassigned("retained hidden history member")
            .await;
        let destination_member = app.seed_personal("history destination survivor").await;
        for (id, destination, key) in [
            (&mover, project(&a), "history-mover-a"),
            (&stayer, project(&a), "history-survivor-a"),
            (&hidden, project(&a), "history-hidden-a"),
            (&destination_member, project(&b), "history-survivor-b"),
        ] {
            assert!(app.move_row(id, destination, key).await);
        }
        let source = app.seed_cluster(Some("history source"), &[0.1, 0.2]).await;
        for id in [&mover, &stayer, &hidden] {
            app.add_cluster_member(&source, id).await;
        }
        let target = app
            .seed_cluster(Some("history destination"), &[0.1, 0.2])
            .await;
        app.add_cluster_member(&target, &destination_member).await;
        let edges = alexandria_storage::repos::EdgeRepo::new(app.db.inner());
        edges
            .create_edge(&mover, &hidden, "derived_from", 0.75)
            .await
            .unwrap();
        let links = edges.get_edges_for(&mover).await.unwrap();
        let edge_id = alexandria_storage::record_id_to_string(links[0].id.as_ref().unwrap());
        app.db
            .inner()
            .query("UPDATE $fact SET confidence = 0.512345;")
            .bind(("fact", RecordId::parse_simple(&mover).unwrap()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let (_, revision) =
            rendered_form_on(&app, &mover, "/debug/collections?partition=project").await;
        let (_, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", &mover),
                    ("revision", &revision),
                    ("destination", "project"),
                    ("project", &b),
                ],
            )
            .await;
        assert!(
            !location.contains("error="),
            "the fixture move must commit: {location}"
        );
        let (status, history) = app.get("/debug/operations").await;
        assert_eq!(status, StatusCode::OK);
        let href = history
            .split("href=\"")
            .find(|s| s.starts_with("/debug/operations/"))
            .and_then(|s| s.split('"').next())
            .expect("emitted history detail link");
        let (status, body) = app.get(href).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.matches("<details").count(),
            6,
            "every before/after snapshot needs exact-state disclosure"
        );
        for value in [&a, &b, &source, &target] {
            assert!(
                body.contains(value),
                "history omitted stored identity {value}"
            );
        }
        let hidden_key = hidden.rsplit_once(':').unwrap().1;
        let edge_key = edge_id.rsplit_once(':').unwrap().1;
        for value in [
            hidden_key,
            edge_key,
            "fact_relations",
            "membership_revision",
            "member_embedding_revision",
            "centroid_revision",
            "0.512345",
        ] {
            assert!(
                body.contains(value),
                "history omitted exact stored value {value}"
            );
        }
        assert!(body.contains("Exact stored state"));
        assert!(body.contains("&#60;script&#62;history-danger&#60;/script&#62;"));
        assert!(
            !body.contains("<script>history-danger</script>"),
            "raw snapshot content must stay escaped"
        );
        let other = app.second_owner("history-other-owner").await;
        let (foreign_status, foreign_body) = other.get(href).await;
        let (absent_status, absent_body) = other
            .get("/debug/operations/semantic_operation:missing-private-history")
            .await;
        assert_eq!(foreign_status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_status, absent_status);
        assert_eq!(foreign_body, absent_body);
        assert!(!foreign_body.contains(hidden_key));
        assert!(!foreign_body.contains(edge_key));
        let (anonymous_status, anonymous_body) = app.get_anonymous(href).await;
        assert_eq!(anonymous_status, StatusCode::FORBIDDEN);
        assert!(!anonymous_body.contains(hidden_key));
        assert!(!anonymous_body.contains(edge_key));
    }

    #[tokio::test]
    async fn an_owner_edit_pages_the_journal_and_history_shows_the_before_state() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app
            .seed_personal("the text before the owner corrected it")
            .await;
        // Control that the journal is empty *before*: the seed went in through a scoped writer, not a page.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "a fixture seed should not already be journaled"
        );

        let edit_revision = rendered_revision(&app, &id).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/edit",
                &[
                    ("id", id.as_str()),
                    ("revision", edit_revision.as_str()),
                    ("content", "the text after the owner corrected it"),
                    ("action", "save"),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "the edit was not accepted: {location}"
        );
        assert!(
            !location.contains("error="),
            "the browser edit reported a failure: {location}"
        );

        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            1,
            "the browser edit did not write exactly one operation"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'update' GROUP ALL;",
            )
            .await,
            1,
            "the edit did not journal an `update`"
        );
        // The superseded text left a hidden before-state, in the same transaction as the new text.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE deleted = true AND content = \
                 'the text before the owner corrected it';",
            )
            .await,
            1,
            "the superseded text left no hidden before-state"
        );

        // The history list links, and the detail page shows both sides verbatim.
        let (status, body) = app.get("/debug/operations").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("/debug/operations/semantic_operation"),
            "the history list linked no operation: {body}"
        );
        // Follow the link the page actually emitted, rather than reconstructing one: a hand-built handle
        // is how this case first failed *for the wrong reason* (it used `meta::id`, the bare key, where the
        // page links the full `table:key`), and a test that builds its own URL cannot catch a page whose
        // links are wrong.
        let href = body
            .split("href=\"")
            .find(|chunk| chunk.starts_with("/debug/operations/semantic_operation"))
            .and_then(|chunk| chunk.split('"').next())
            .expect("the history list linked no operation")
            .to_string();
        let (status, detail) = app.get(&href).await;
        assert_eq!(status, StatusCode::OK, "{href} -> {status}");
        assert!(
            detail.contains("the text before the owner corrected it"),
            "the history page did not show the exact before-state"
        );
        assert!(
            detail.contains("the text after the owner corrected it"),
            "the history page did not show the exact after-state"
        );
    }

    #[tokio::test]
    async fn an_owner_delete_journals_the_visibility_change() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app
            .seed_personal("a memory the owner decided to hide")
            .await;
        let (status, _) = app
            .post(
                "/debug/collections/edit",
                &[("id", id.as_str()), ("action", "delete")],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'delete' GROUP ALL;",
            )
            .await,
            1,
            "the delete was not journaled"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE deleted = true AND content = \
                 'a memory the owner decided to hide';",
            )
            .await,
            1,
            "the journaled delete did not hide the row"
        );
    }

    #[tokio::test]
    async fn an_owner_move_keeps_the_id_and_journals_the_move() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory about to be filed").await;
        let project = app.own_project("the-filing-destination").await;
        let revision = rendered_revision(&app, &id).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("destination", "project"),
                    ("project", project.as_str()),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a lawful move was refused: {location}"
        );
        assert!(
            !location.contains("error="),
            "a lawful move reported a failure: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
            )
            .await,
            1,
            "the move did not journal exactly one operation"
        );
        // The id survived — no new row was minted to hold the same content.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a memory about to be filed';",
            )
            .await,
            1,
            "the move duplicated the row instead of keeping the id"
        );
    }

    #[tokio::test]
    async fn rendered_row_project_default_allows_nonproject_moves() {
        for destination in ["personal", "unassigned"] {
            let app = authenticated_on_server(test_server().await).await;
            let id = app.seed_personal("rendered default move control").await;
            let project = app.own_project("first-project").await;
            app.own_project("second-project").await;
            let revision = rendered_revision(&app, &id).await;
            let (status, location, _) = app
                .post_form_located(
                    "/debug/collections/move",
                    &[
                        ("id", id.as_str()),
                        ("revision", revision.as_str()),
                        ("destination", "project"),
                        ("project", project.as_str()),
                    ],
                )
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
            assert!(
                !location.contains("error="),
                "project control failed: {location}"
            );

            let path = "/debug/collections?partition=project";
            let (posted_id, revision) = rendered_form_on(&app, &id, path).await;
            let (status, page) = app.get(path).await;
            assert_eq!(status, StatusCode::OK);
            let project_select = page
                .split("<select name=\"project\">")
                .nth(1)
                .expect("the actual row offers its Project select")
                .split("</select>")
                .next()
                .unwrap();
            assert!(project_select.contains(project.as_str()));
            let options = project_select.split("<option ").skip(1);
            let default_option = options
                .clone()
                .find(|option| option.split('>').next().unwrap().contains(" selected"))
                .or_else(|| options.clone().next())
                .expect("the actual select has a default option");
            let default_project = default_option
                .split("value=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            // Submit the rendered default, not a hand-crafted valid empty project.
            let (status, location, _) = app
                .post_form_located(
                    "/debug/collections/move",
                    &[
                        ("id", posted_id.as_str()),
                        ("revision", revision.as_str()),
                        ("destination", destination),
                        ("project", default_project),
                    ],
                )
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
            assert!(
                !location.contains("error="),
                "rendered project default {default_project:?} cannot express {destination}: {location}"
            );
            let destination_path = format!("/debug/collections?partition={destination}");
            let (same_id, _) = rendered_form_on(&app, &id, &destination_path).await;
            assert_eq!(
                same_id, id,
                "the destination page must retain the same fact"
            );
            assert_eq!(
                one(
                    &app,
                    "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;"
                )
                .await,
                2,
                "the project control and non-project move each journal exactly once"
            );
            assert_eq!(one(&app,
                "SELECT count() AS n FROM fact WHERE content = 'rendered default move control' GROUP ALL;").await, 1);
        }
    }

    /// A move that would change nothing is refused, not journaled.
    ///
    /// The row is already in the partition the form names, so an operation row saying "moved" would be the
    /// audit noise the journal exists to avoid — and the earlier shape of this control silently wrote one.
    #[tokio::test]
    async fn a_move_to_where_the_memory_already_is_writes_nothing() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory already where it is").await;
        let revision = rendered_revision(&app, &id).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("destination", "personal"),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("already%20in%20the%20partition"),
            "the no-op move was not named as one: {location}"
        );
        none(
            &app,
            "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
        )
        .await;
    }

    #[tokio::test]
    async fn dismissing_a_proposal_decides_it_without_moving_anything() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory an agent guessed at").await;
        let proposal = propose(&app, &id).await;

        let (status, _) = app
            .post(
                "/debug/classification/decide",
                &[("selected", proposal.as_str()), ("decision", "dismiss")],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM classification_proposal WHERE state = 'dismissed' GROUP ALL;",
            )
            .await,
            1,
            "the dismissal did not decide the proposal"
        );
        // A dismissal is not a corpus write: nothing in the journal, nothing moved.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "a dismissal wrote a journal row"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a memory an agent guessed at' \
                 AND deleted = false;",
            )
            .await,
            1,
            "the dismissed proposal still touched the memory"
        );
        let (status, body) = app.get("/debug/classification").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Nothing pending"), "{body}");
    }

    #[tokio::test]
    async fn approving_a_proposal_files_the_memory_and_keeps_quarantine() {
        let app = authenticated_on_server(test_server().await).await;
        // The review partition's own state: quarantined, awaiting a human decision.
        let id = app
            .seed_quarantined_unassigned("a quarantined memory awaiting a decision")
            .await;
        let proposal = propose(&app, &id).await;

        let (status, body) = app
            .post(
                "/debug/classification/decide",
                &[("selected", proposal.as_str()), ("decision", "approve")],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "an approval failed: {body}");
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
            )
            .await,
            1,
            "the approval did not journal a move"
        );
        // Task 11's rule, observable: deciding an opinion about a memory never un-hides it.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE quarantined_at != NONE;",
            )
            .await,
            1,
            "approval silently cleared the quarantine"
        );
    }

    #[tokio::test]
    async fn one_owners_pages_never_show_another_owners_anything() {
        let app = authenticated_on_server(test_server().await).await;
        let theirs = app
            .seed_personal("a private memory of the first owner")
            .await;
        let second = app.second_owner("second-owner").await;

        let (status, body) = second.get("/debug/collections").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains("a private memory of the first owner"),
            "the collections page leaked another owner's content"
        );
        let (status, history) = second.get("/debug/operations").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !history.contains("/debug/operations/semantic_operation"),
            "the history page listed another owner's operations"
        );

        // Reaching for the row by id answers exactly like reaching for nothing — and both posts carry a
        // revision, so the equality is the *writer's* answer and not the form-validation refusal that a
        // revision-less post would get before it ever looked at an id.
        let (foreign, _) = second
            .post(
                "/debug/collections/edit",
                &[
                    ("id", theirs.as_str()),
                    ("revision", "1"),
                    ("content", "not yours"),
                    ("action", "save"),
                ],
            )
            .await;
        let (absent, _) = second
            .post(
                "/debug/collections/edit",
                &[
                    ("id", "fact:never-existed-anywhere"),
                    ("revision", "1"),
                    ("content", "not yours"),
                    ("action", "save"),
                ],
            )
            .await;
        assert_eq!(
            foreign, absent,
            "a foreign id and an absent one answered differently ({foreign} vs {absent})"
        );
        // The control that makes the equality a refusal rather than two misses: the row is real and the
        // first owner still has it, unchanged.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a private memory of the first owner' \
                 AND deleted = false;",
            )
            .await,
            1,
            "the first owner's own row disappeared"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "the refused write still journaled something for the real owner"
        );
    }

    /// An edit form that carries no revision is refused, not applied at whatever version the row happens
    /// to hold. This is the control that keeps "the page binds its stale-conflict to the version it
    /// rendered" a property of the route rather than a property of the template.
    #[tokio::test]
    async fn an_edit_without_a_rendered_revision_is_refused_and_writes_nothing() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory nobody gave a revision").await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/edit",
                &[
                    ("id", id.as_str()),
                    ("content", "text that should not land"),
                    ("action", "save"),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("reload%20the%20page"),
            "a revision-less edit was not sent back to reload: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a memory nobody gave a revision';",
            )
            .await,
            1,
            "the refused edit changed the row anyway"
        );
        none(
            &app,
            "SELECT count() AS n FROM semantic_operation GROUP ALL;",
        )
        .await;
        // Positive control: the same content with the rendered revision lands.
        let revision = rendered_revision(&app, &id).await;
        app.post_form(
            "/debug/collections/edit",
            &[
                ("id", id.as_str()),
                ("revision", revision.as_str()),
                ("content", "text that should land"),
                ("action", "save"),
            ],
        )
        .await;
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'text that should land';",
            )
            .await,
            1,
            "the control edit did not apply"
        );
    }

    #[tokio::test]
    async fn a_decision_with_nothing_selected_says_so_and_writes_nothing() {
        let app = authenticated_on_server(test_server().await).await;
        let (status, _) = app
            .post("/debug/classification/decide", &[("decision", "approve")])
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        // The message is read from the redirect **target**, not from a bare `/debug/collections`.
        // `post` returns `(status, body)` and a 303 carries no body, so the note lives only in the
        // `Location` header — and the shared fixture's `round_trip` drops headers, which is correct for
        // every suite that uses it. Reaching the target with the query the handler chose is the assertion
        // that still tests the handler's own wording without widening a fixture other suites depend on.
        let (_, page) = app
            .get("/debug/collections?error=select%20at%20least%20one%20proposal")
            .await;
        let badges: Vec<String> = page
            .split("badge\">")
            .skip(1)
            .map(|tail| tail.chars().take_while(|c| *c != '<').collect())
            .collect();
        assert!(
            badges
                .iter()
                .any(|b| b.contains("select at least one proposal")),
            "the collections page did not render a refusal note: {badges:?}"
        );
        // And the refusal itself happened: 303 to the error target, never a 200 that rendered a decision.
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            page.contains("select at least one proposal"),
            "an empty selection was not reported: {page}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "an empty selection wrote something"
        );
    }

    // -- the shapes a browser actually posts ---------------------------------------------------
    //
    // Every case below sends what a `<form>` sends, not what a Rust test finds convenient. The review's
    // two HIGH findings were both invisible to the earlier suite for the same reason: no test ever posted
    // a second `selected` pair, and no test ever left the `tags` field empty.

    /// The row's rendered revision, read off the page rather than re-read from the store.
    ///
    /// A stale-conflict test that asked the store for the current version at POST time would be comparing
    /// a number against itself and could never fail, so the number has to come from the same bytes the
    /// owner's controls were built from.
    /// The value one of the row's own controls would post as its expected revision.
    ///
    /// Read out of the rendered **hidden input**, not out of a display cell and never out of the store: the
    /// claim under test is that the control a browser actually submits carries the revision the page saw, so
    /// reading anything else would let the test pass while the control stayed broken. The id field is read
    /// from the same form for the same reason.
    async fn rendered_form(app: &AuthenticatedTestApp, id: &str) -> (String, String) {
        rendered_form_on(app, id, "/debug/collections").await
    }

    async fn rendered_form_on(
        app: &AuthenticatedTestApp,
        id: &str,
        path: &str,
    ) -> (String, String) {
        let (status, page) = app.get(path).await;
        assert_eq!(status, StatusCode::OK);
        // The row's control block follows the row that links to the memory's detail page.
        let needle = urlencoded(id);
        // Split on the tag prefix, not `<tr>`: askama renders the control row as `<tr class="controls">`,
        // and a split that only matches the bare tag would count rows the page never made.
        let anchor = page
            .split("<tr")
            .position(|chunk| chunk.contains(&format!(r##"href="/debug/memories/{needle}"##)))
            .unwrap_or_else(|| panic!("the listing did not render the row {id}"));
        let controls = page
            .split("<tr")
            .nth(anchor + 1)
            .unwrap_or_else(|| panic!("no control row follows the row for {id}"));
        let revision = hidden_value(controls, "revision")
            .unwrap_or_else(|| panic!("the control row posts no revision: {controls}"));
        let posted_id =
            hidden_value(controls, "id").unwrap_or_else(|| panic!("the control row posts no id"));
        assert_eq!(
            posted_id, id,
            "the control row is wired to a different memory than its row: {posted_id} != {id}"
        );
        (posted_id, revision)
    }

    /// One hidden input's value, from a rendered form fragment.
    fn hidden_value(html: &str, name: &str) -> Option<String> {
        let marker = format!(r##"name="{name}" value=""##);
        let rest = html.split(&marker).nth(1)?;
        let value: String = rest.chars().take_while(|c| *c != '"').collect();
        Some(value)
    }

    async fn rendered_revision(app: &AuthenticatedTestApp, id: &str) -> String {
        let (_, revision) = rendered_form(app, id).await;
        assert!(
            revision.chars().all(|c| c.is_ascii_digit()) && !revision.is_empty(),
            "the rendered control carries a non-numeric revision: {revision:?}"
        );
        revision
    }

    /// The row's own controls are prefilled with the id and revision the page read for that row.
    ///
    /// This is the shape the review named as option (a) for H2: one form per row, carrying the row's
    /// identity, so an owner never hand-copies a handle into a field a mistake in would aim at somebody
    /// else's memory — and so the stale-conflict guard has a value to be stale about.
    #[tokio::test]
    async fn each_listed_memory_offers_its_own_prefilled_controls() {
        let app = authenticated_on_server(test_server().await).await;
        let first = app.seed_personal("the first listed memory").await;
        let second = app.seed_personal("the second listed memory").await;
        let (id_one, revision_one) = rendered_form(&app, &first).await;
        let (id_two, revision_two) = rendered_form(&app, &second).await;
        assert_eq!(
            (id_one.as_str(), id_two.as_str()),
            (first.as_str(), second.as_str())
        );
        // Two freshly written rows legitimately share revision one, so equality across rows proves nothing.
        // What *is* per-row is the field reacting to its own row: editing one row moves that row's rendered
        // revision and leaves the other's where it was.
        app.post_form(
            "/debug/collections/edit",
            &[
                ("id", first.as_str()),
                ("revision", revision_one.as_str()),
                ("content", "the first row, edited once"),
                ("action", "save"),
            ],
        )
        .await;
        let after_one = rendered_revision(&app, &first).await;
        let after_two = rendered_revision(&app, &second).await;
        assert_ne!(
            after_one, revision_one,
            "the control's revision did not move when its own row was edited"
        );
        assert_eq!(
            after_two, revision_two,
            "editing one row changed another row's rendered revision"
        );
        // Posting the *other* row's id with this row's revision must not touch the first row: the guard is
        // per-row, not per-page.
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/edit",
                &[
                    ("id", second.as_str()),
                    ("revision", "99999"),
                    ("content", "text aimed with a revision nobody rendered"),
                    ("action", "save"),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("changed%20since%20this%20page%20loaded"),
            "a mismatched revision was accepted rather than refused: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'the second listed memory';",
            )
            .await,
            1,
            "the refused edit rewrote the row it was aimed at"
        );
    }

    #[tokio::test]
    async fn two_checked_boxes_approve_both_proposals_and_journal_two_moves() {
        let app = authenticated_on_server(test_server().await).await;
        let first = app
            .seed_personal("the first memory an owner approves")
            .await;
        let second = app
            .seed_personal("the second memory an owner approves")
            .await;
        let first_reference = propose(&app, &first).await;
        let second_reference = propose(&app, &second).await;
        // The control both, exactly as two checked boxes post it: one `selected` pair each, no joining.
        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[
                    ("selected", first_reference.as_str()),
                    ("selected", second_reference.as_str()),
                    ("decision", "approve"),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a two-box approval was refused"
        );
        assert!(
            location.contains("2%20moved"),
            "the answer does not report both entries applied: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
            )
            .await,
            2,
            "a two-entry approval did not journal two moves"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM classification_proposal WHERE state = 'approved' GROUP ALL;",
            )
            .await,
            2,
            "only one proposal was marked decided"
        );
        // Positive control that both memories actually moved partition, not just both rows written.
        for id in [&first, &second] {
            assert_eq!(
                one(
                    &app,
                    &format!(
                        "SELECT count() AS n FROM fact WHERE id = type::record('fact', '{}') \
                         AND scope = 'personal' AND deleted = false;",
                        id.split(':').next_back().unwrap_or(id.as_str())
                    ),
                )
                .await,
                1,
                "{id} did not land in the destination the proposal named"
            );
        }
    }

    #[tokio::test]
    async fn two_checked_boxes_dismiss_both_without_writing_a_journal_row() {
        let app = authenticated_on_server(test_server().await).await;
        let first_reference =
            propose(&app, &app.seed_personal("a memory dismissed first").await).await;
        let second_reference =
            propose(&app, &app.seed_personal("a memory dismissed second").await).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[
                    ("selected", first_reference.as_str()),
                    ("selected", second_reference.as_str()),
                    ("decision", "dismiss"),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("2%20dismissed"),
            "the answer did not count both dismissals: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM classification_proposal WHERE state = 'dismissed' GROUP ALL;",
            )
            .await,
            2,
            "a two-box dismissal decided only one proposal"
        );
        // A dismissal is not a corpus write, in either count.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "a dismissal journaled something"
        );
        none(
            &app,
            "SELECT count() AS n FROM fact WHERE deleted = false AND scope != 'personal';",
        )
        .await;
    }

    #[tokio::test]
    async fn one_checked_box_still_works_and_a_tampered_one_is_refused_whole() {
        let app = authenticated_on_server(test_server().await).await;
        let lone = propose(&app, &app.seed_personal("a lone approval").await).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[("selected", lone.as_str()), ("decision", "approve")],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a single selection regressed"
        );
        assert!(
            location.contains("1%20moved"),
            "a single approval reported something else: {location}"
        );

        // A body where one of the two values is not a reference at all: refused *as a whole*, because
        // quietly applying the parseable half is a different act from the one the owner took.
        let second = propose(
            &app,
            &app.seed_personal("a memory nobody decided for").await,
        )
        .await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[
                    ("selected", second.as_str()),
                    ("selected", "not-a-reference"),
                    ("decision", "approve"),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("no%20longer%20a%20proposal"),
            "a half-parseable selection was applied rather than refused: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            1,
            "the refused batch wrote a second operation: the one lawful approval above is the control"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM classification_proposal WHERE state = 'pending' GROUP ALL;",
            )
            .await,
            1,
            "the undecidable batch decided the proposal it could parse"
        );
    }

    #[tokio::test]
    async fn an_empty_tags_field_keeps_the_tags_and_the_clear_control_removes_them() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory with tags worth keeping").await;
        app.set_tags(&id, &["keep-me".to_string(), "and-me".to_string()])
            .await;
        let revision = rendered_revision(&app, &id).await;

        // Text-only save: the browser posts `tags=` because the input exists and is empty.
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/edit",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("content", "the text after a correction"),
                    ("tags", ""),
                    ("action", "save"),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a text-only edit was refused: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE tags = ['keep-me', 'and-me'] AND content = \
                 'the text after a correction';",
            )
            .await,
            1,
            "an empty tags field wiped the tags — H2, the silent data loss on the primary edit control"
        );

        // Explicit replacement: a non-empty field means new tags.
        let revision = rendered_revision(&app, &id).await;
        app.post_form(
            "/debug/collections/edit",
            &[
                ("id", id.as_str()),
                ("revision", revision.as_str()),
                ("tags", "replaced, second"),
                ("action", "save"),
            ],
        )
        .await;
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE tags = ['replaced', 'second'];",
            )
            .await,
            1,
            "an explicit tag replacement did not land"
        );

        // The clear control: pressed, and only then.
        let revision = rendered_revision(&app, &id).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/edit",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("tags", ""),
                    ("clear_tags", "on"),
                    ("action", "save"),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "the clear control was refused: {location}"
        );
        assert_eq!(
            one(&app, "SELECT count() AS n FROM fact WHERE tags = [];").await,
            1,
            "the explicit clear-tags control did not remove every tag"
        );

        let repo = MutationRepo::new(app.db.inner());
        let operations = repo.history(&app.owner, 10).await.unwrap();
        let mut effects = Vec::new();
        for operation in operations {
            let detail = repo
                .operation(&app.owner, &operation.operation_key)
                .await
                .unwrap()
                .expect("each edit is journaled");
            let mut actual = detail.actual_effects;
            actual.sort();
            let mut expected = detail.expected_effects;
            expected.sort();
            assert_eq!(
                actual, expected,
                "the journal's approved and written effects disagree"
            );
            effects.push(actual);
        }
        effects.sort();
        assert_eq!(
            effects,
            vec![
                vec![
                    "content",
                    "lineage:snapshot",
                    "memory_edge:derived_from",
                    "vector"
                ],
                vec!["tags"],
                vec!["tags"],
            ],
            "text-only save and explicit tag changes must journal their own exact effects"
        );
        let (list_status, history) = app.get("/debug/operations").await;
        assert_eq!(list_status, StatusCode::OK);
        assert!(
            history.contains("/debug/operations/semantic_operation"),
            "the three edits above left nothing readable in the owner's history: {history}"
        );
    }

    #[tokio::test]
    async fn a_move_binds_the_revision_the_page_rendered_and_a_stale_one_is_refused() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory whose page went stale").await;
        let revision = rendered_revision(&app, &id).await;

        // Somebody else edits the row between the page render and the click — here, through the same
        // journaled writer a second owner's page would use.
        app.bump_semantic_revision(&id).await;

        let destination = app.own_project("stale-destination").await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("destination", "project"),
                    ("project", destination.as_str()),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("changed%20since%20this%20page%20loaded"),
            "a move against a superseded revision applied anyway: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
            )
            .await,
            0,
            "the refused move journaled an operation"
        );

        // Positive control: the *current* revision moves it, and the journal says so.
        let project = app.own_project("stale-move-destination").await;
        let fresh = rendered_revision(&app, &id).await;
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", id.as_str()),
                    ("revision", fresh.as_str()),
                    ("destination", "project"),
                    ("project", project.as_str()),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a lawful move was refused: {location}"
        );
        assert!(
            !location.contains("error="),
            "the lawful move reported a failure: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
            )
            .await,
            1,
            "the lawful move journaled nothing"
        );
    }

    #[tokio::test]
    async fn a_move_into_an_owned_project_files_the_memory_and_the_queue_offers_it() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app
            .seed_personal("a project fact that has to leave personal")
            .await;
        let project = app.own_project("moved-into-me").await;
        let revision = rendered_revision(&app, &id).await;

        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("destination", "project"),
                    ("project", project.as_str()),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a project move was refused: {location}"
        );
        assert!(
            !location.contains("error="),
            "the project move reported a failure: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE scope = 'project' AND project != NONE AND \
                 content = 'a project fact that has to leave personal';",
            )
            .await,
            1,
            "the row did not land in the project it was moved into"
        );
        // The id survived: one row, not a copy.
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a project fact that has to leave \
                 personal';",
            )
            .await,
            1,
            "the project move duplicated the row"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'move' GROUP ALL;",
            )
            .await,
            1,
            "the project move journaled something other than exactly one move"
        );

        // A destination the caller does not own is refused, and creates nothing.
        let other = app.second_owner("third-owner").await;
        let (status, location, _) = other
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", id.as_str()),
                    ("revision", revision.as_str()),
                    ("destination", "project"),
                    ("project", "moved-into-me"),
                ],
            )
            .await;
        // A row that is not the caller's answers exactly like a row that does not exist — the same
        // outward equivalence every other owner surface keeps, which is why this is a 404 and not a
        // redirect naming the partition.
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a foreign id answered differently from an absent one: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE scope = 'project' AND content = 'a project \
                 fact that has to leave personal';",
            )
            .await,
            1,
            "the refused foreign move moved the row after all"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM project WHERE normalized_alias = 'moved-into-me';",
            )
            .await,
            1,
            "a move by a non-owner created or changed a project row"
        );
    }

    #[tokio::test]
    async fn the_archive_is_the_only_page_that_shows_session_prose() {
        let app = authenticated_on_server(test_server().await).await;
        app.seed_session(
            "mixed-session",
            Some("the whole-session summary <b>tagged</b>"),
        )
        .await;
        let id = app
            .seed_personal("a memory stored during that session")
            .await;
        app.link_session("mixed-session", &id).await;

        let (status, archive) = app.get("/debug/collections/archive").await;
        assert_eq!(status, StatusCode::OK, "the archive page did not render");
        assert!(
            archive.contains("the whole-session summary"),
            "the archive withheld the prose it exists to show: {archive}"
        );
        // Askama escapes at render: the stored markup must not become an element.
        assert!(
            archive.contains("&#60;b&#62;"),
            "the archive rendered stored markup as markup: {archive}"
        );
        assert!(
            archive.contains("mixed"),
            "a mixed session was not labelled as one: {archive}"
        );

        // The scoped surfaces say nothing about it. The negative is only meaningful because the positive
        // above proves the same fixture can render the prose at all.
        for path in [
            "/debug/collections",
            "/debug/collections?partition=project",
            "/debug/sessions",
            "/debug/classification",
        ] {
            let (status, page) = app.get(path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(
                !page.contains("the whole-session summary"),
                "{path} leaked whole-session prose"
            );
        }
        // An anonymous reader does not get the archive either.
        let (status, _) = app.get_anonymous("/debug/collections/archive").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the archive served without a session"
        );
    }

    #[tokio::test]
    async fn an_agent_cannot_delete_a_review_row_a_human_can() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app
            .seed_quarantined_unassigned("a quarantined row an agent should not purge")
            .await;
        let scope = app.corpus_scope().await;

        // The tool path: refused, and answered like an absent row.
        let refusal = app
            .server()
            .delete_within(&scope, &id)
            .await
            .expect_err("an agent delete reached the review partition");
        assert!(
            matches!(
                refusal,
                crate::error::ServiceError::NotFound | crate::error::ServiceError::Stale
            ),
            "the corpus delete answered something other than the absent-row class: {refusal:?}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE quarantined_at != NONE AND deleted = false;",
            )
            .await,
            1,
            "the refused agent delete hid the review row"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "the refused agent delete journaled an operation"
        );

        // The human path, through the page: the same row, the same writer, and a journal row.
        let (status, location, _) = app
            .post_form_located(
                "/debug/collections/edit",
                &[("id", id.as_str()), ("action", "delete")],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "the human delete was refused: {location}"
        );
        assert!(
            !location.contains("error="),
            "the owner's own cleanup of a review row failed: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a quarantined row an agent should \
                 not purge' AND deleted = true AND quarantined_at != NONE;",
            )
            .await,
            1,
            "the human delete did not hide the review row it was allowed to hide"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation WHERE kind = 'delete' GROUP ALL;",
            )
            .await,
            1,
            "the human delete was not journaled"
        );
    }

    #[tokio::test]
    async fn revising_a_destination_invalidates_the_approval_it_replaced() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app
            .seed_personal("a memory whose destination changes")
            .await;
        let proposal = propose(&app, &id).await;
        let (handle, revision) = proposal
            .rsplit_once(':')
            .expect("a proposal reference carries its revision");

        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/revise",
                &[
                    ("id", handle),
                    ("revision", revision),
                    ("destination", "unassigned"),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "a revision was refused: {location}"
        );
        assert!(
            location.contains("revision%201"),
            "the answer did not report the moved proposal revision: {location}"
        );

        // The approval the page still holds (revision 0) is now stale-by-revision and must not apply the
        // destination the owner stopped asking for.
        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[("selected", proposal.as_str()), ("decision", "approve")],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            location.contains("0%20moved"),
            "a superseded approval was applied: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM semantic_operation GROUP ALL;"
            )
            .await,
            0,
            "an approval against a revised proposal wrote a journal row"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a memory whose destination changes' \
                 AND scope = 'personal';",
            )
            .await,
            1,
            "the refused approval moved the memory anyway"
        );

        // Positive control: the *current* revision applies, and files where the owner last said.
        let (status, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[
                    ("selected", format!("{handle}:1").as_str()),
                    ("decision", "approve"),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "the current approval failed: {location}"
        );
        assert!(
            location.contains("1%20moved"),
            "the current revision's approval did not apply: {location}"
        );
        assert_eq!(
            one(
                &app,
                "SELECT count() AS n FROM fact WHERE content = 'a memory whose destination changes' \
                 AND scope = 'unassigned';",
            )
            .await,
            1,
            "the approved revision did not file the memory where the owner last pointed it"
        );
    }

    // -- B1: the rendered revise form, extracted rather than hand-typed --------------------------------
    //
    // The focused review named a defect that no handler test could see: `collections.html`'s per-proposal
    // revise form posted `csrf`/`id`/`destination`/`project` and **no `revision`**, while `ReviseForm`
    // requires it, so serde refused with 422 before the handler ran (`browser-evidence-fix1/
    // revise-refusal.txt`, `revised-destination.json`: revision and journal unchanged after a real click).
    // Every existing case passed because it built its own body and supplied the field the page never made.
    // These cases therefore extract the pairs from the **rendered markup** and post them whole through
    // [`AuthenticatedTestApp::post_exact`], with no field added from the fixture's own copy.

    /// One `<form>` element's markup, located by its action and by a field it contains.
    ///
    /// A queue with two proposals renders two revise forms, so the marker (that proposal's own id) is what
    /// makes "*this* row's control" the thing under test rather than whichever form the scan happened to
    /// reach first.
    fn form_markup<'a>(page: &'a str, action: &str, marker: &str) -> Option<&'a str> {
        let open = format!(r##"action="{action}""##);
        let rest = page
            .split("<form")
            .skip(1)
            .find(|chunk| chunk.contains(&open) && chunk.contains(marker))?;
        rest.split("</form>").next()
    }

    /// One attribute of a tag, unquoting nothing because askama renders values in double quotes.
    fn attribute(tag: &str, name: &str) -> Option<String> {
        let marker = format!(r##"{name}=""##);
        let (_, rest) = tag.split_once(&marker)?;
        Some(rest[..rest.find('"')?].to_string())
    }

    /// The `<option>` values of one `<select>` in document order, and which of them a browser posts.
    ///
    /// An untouched `<select>` submits its **selected** option, or its first when none is marked, so the
    /// extracted "default" is what the form would post with no interaction — while the full list is what a
    /// test needs to choose a *different* destination without inventing a value the page never offered.
    fn select_options(form: &str, name: &str) -> (Vec<String>, String) {
        let open = format!(r##"<select name="{name}">"##);
        let (_, rest) = form
            .split_once(&open)
            .unwrap_or_else(|| panic!("no select named {name}"));
        let body = rest.split("</select>").next().expect("a closed select");
        let mut values = Vec::new();
        let mut chosen = None;
        for chunk in body.split("<option").skip(1) {
            let tag = chunk.split('>').next().unwrap_or(chunk);
            let value = attribute(tag, "value").unwrap_or_default();
            if attribute(tag, "selected").is_some() {
                chosen = Some(value.clone());
            }
            if values.is_empty() {
                values.push(value.clone());
            } else {
                values.push(value);
            }
        }
        let default = chosen.unwrap_or_else(|| values.first().cloned().unwrap_or_default());
        (values, default)
    }

    /// Every pair an untouched render of `form` posts, in document order.
    ///
    /// Checkboxes and submit buttons are skipped because neither contributes a field until it is clicked,
    /// which is the browser's rule and the reason the queue's `selected` boxes are handled by the separate
    /// multi-select cases rather than here. An `<input>` with no `value` attribute posts the empty string,
    /// which is what the free-text `project` field does until an owner types in it.
    fn posted_pairs(form: &str) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for chunk in form.split('<').skip(1) {
            let head = chunk.split('>').next().unwrap_or(chunk);
            let kind = if head.starts_with("input") {
                Postable::Input
            } else if head.starts_with("select") {
                Postable::Select
            } else if head.starts_with("textarea") {
                Postable::Textarea
            } else {
                continue;
            };
            let Some(name) = attribute(head, "name") else {
                continue;
            };
            match kind {
                Postable::Input => {
                    let r#type = attribute(head, "type").unwrap_or_else(|| "text".to_string());
                    if !matches!(r#type.as_str(), "checkbox" | "submit" | "button") {
                        pairs.push((name, attribute(head, "value").unwrap_or_default()));
                    }
                }
                Postable::Select => {
                    let (_, default) = select_options(form, &name);
                    pairs.push((name, default));
                }
                Postable::Textarea => {
                    let body = chunk
                        .split('>')
                        .nth(1)
                        .unwrap_or_default()
                        .split("</textarea>")
                        .next()
                        .unwrap_or_default();
                    pairs.push((name, body.to_string()));
                }
            }
        }
        pairs
    }

    enum Postable {
        Input,
        Select,
        Textarea,
    }

    /// A form body spelled exactly as a browser sends one: percent-encoded pairs joined with `&`.
    fn encode_body(pairs: &[(String, String)]) -> String {
        pairs
            .iter()
            .map(|(name, value)| format!("{name}={}", urlencoded(value)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Every `<form>` that the HTML5 parser would move out of the table it was written in.
    ///
    /// A `<form>` whose immediate parent is `<tr>` cannot be submitted at all: the parser foster-parents it
    /// *before* the table, so the page renders an empty form element and a real click posts nothing — which
    /// is what `browser-evidence-fix1/classification-dom-forms.json` recorded, and why `:has()` and ref
    /// selectors on those rows found nothing. A form nested inside another form is the same class of defect
    /// (`decide`'s whole-queue form would swallow a per-proposal form written inside it).
    fn table_form_violations(page: &str) -> Vec<String> {
        const VOID: &[&str] = &[
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
            "source", "track", "wbr",
        ];
        let mut stack: Vec<String> = Vec::new();
        let mut violations = Vec::new();
        let mut rest = page;
        while let Some(at) = rest.find('<') {
            let tag = &rest[at..];
            let Some(end) = tag.find('>') else { break };
            let body = &tag[1..end];
            rest = &tag[end + 1..];
            if body.starts_with('!') || body.starts_with('?') {
                continue;
            }
            let closing = body.starts_with('/');
            let name = body
                .trim_start_matches('/')
                .split([' ', '/', '>'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if closing {
                if let Some(at) = stack.iter().rposition(|open| *open == name) {
                    stack.truncate(at);
                }
                continue;
            }
            if VOID.contains(&name.as_str()) || body.ends_with('/') {
                continue;
            }
            if name == "form" {
                if stack.iter().any(|open| open == "form") {
                    violations.push(format!("a form nested inside another form: {body}"));
                }
                let nearest = stack
                    .iter()
                    .rev()
                    .find(|open| matches!(open.as_str(), "table" | "tr" | "td" | "th"));
                if matches!(nearest, Some(tag) if tag == "tr" || tag == "table") {
                    violations.push(format!(
                        "a form is a direct child of <{}>, which the parser foster-parents out of the table",
                        nearest.map(String::as_str).unwrap_or("?")
                    ));
                }
            }
            stack.push(name);
        }
        violations
    }

    /// The revise control for one proposal, on one page, as the pairs it posts plus its rendered revision.
    async fn revise_form_on(
        app: &AuthenticatedTestApp,
        path: &str,
        proposal: &str,
    ) -> Vec<(String, String)> {
        let (status, page) = app.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path} did not render");
        let form = form_markup(&page, "/debug/classification/revise", proposal)
            .unwrap_or_else(|| panic!("{path} renders no revise form for {proposal}"));
        posted_pairs(form)
    }

    /// A page's own revise control changes its proposal's destination and moves its revision.
    ///
    /// The destination is chosen from the **rendered option list** of that same form, not typed, so the
    /// case cannot pass by agreeing with a value the page does not offer; the revision comes out of the
    /// form's hidden field, so the case fails loudly on exactly the defect B1 is.
    #[tokio::test]
    async fn the_rendered_revise_form_carries_its_revision_on_both_pages() {
        for path in ["/debug/collections", "/debug/classification"] {
            let app = authenticated_on_server(test_server().await).await;
            let id = app
                .seed_personal("a memory whose rendered destination changes")
                .await;
            let proposal = propose(&app, &id).await;
            let handle = proposal
                .rsplit_once(':')
                .expect("a proposal reference")
                .0
                .to_string();
            let pairs = revise_form_on(&app, path, &handle).await;
            let named = pairs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            assert!(
                named.contains(&"revision"),
                "the revise control on {path} posts no revision field, which serde refuses with 422 \
                 before the handler runs: {named:?}"
            );
            for field in ["csrf", "id", "revision", "destination"] {
                assert!(
                    named.contains(&field),
                    "the revise control on {path} does not post {field}: {named:?}"
                );
            }
            let rendered = pairs
                .iter()
                .find(|(name, _)| name == "revision")
                .map(|(_, value)| value.clone())
                .expect("a revision field");
            assert_eq!(
                rendered,
                store_proposal_revision(&app, &handle).await.to_string(),
                "the revision the {path} form posts is not the revision the proposal carries"
            );

            // Choose a destination from the form's own options that differs from the rendered default.
            let (status, page) = app.get(path).await;
            assert_eq!(status, StatusCode::OK);
            let form = form_markup(&page, "/debug/classification/revise", &handle)
                .expect("the revise form");
            let (options, default) = select_options(form, "destination");
            // `project` is excluded because that arm needs a *name* the page never prefills, and typing one
            // is exactly what a rendered-form test must not do; the remaining rendered options are the ones
            // the control can post untouched, and the change is chosen among them.
            let movable = options
                .iter()
                .filter(|value| value.as_str() != "project")
                .cloned()
                .collect::<Vec<_>>();
            assert!(
                movable.len() > 1,
                "{path} offers fewer than two postable destination options: {options:?}"
            );
            let chosen = movable
                .iter()
                .find(|value| *value != &default)
                .cloned()
                .expect("a second postable option");
            let posted: Vec<(String, String)> = pairs
                .iter()
                .map(|(name, value)| {
                    if name == "destination" {
                        (name.clone(), chosen.clone())
                    } else {
                        (name.clone(), value.clone())
                    }
                })
                .collect();
            let (status, location, _) = app
                .post_exact("/debug/classification/revise", encode_body(&posted))
                .await;
            assert_eq!(
                status,
                StatusCode::SEE_OTHER,
                "the page's own revise form was refused on {path}: {location}"
            );
            assert!(
                !location.contains("error=") && location.contains("revision%201"),
                "the revise did not report a moved revision on {path}: {location}"
            );
            let (kind, revision) = store_proposal_destination(&app, &handle).await;
            assert_eq!(revision, 1, "the revision did not move on {path}");
            assert_eq!(
                kind, chosen,
                "the destination the page posted is not the destination the store holds"
            );
        }
    }

    /// A revise form extracted before a newer revision exists is refused and changes nothing.
    #[tokio::test]
    async fn a_revise_form_extracted_before_a_newer_revision_is_refused() {
        for path in ["/debug/collections", "/debug/classification"] {
            let app = authenticated_on_server(test_server().await).await;
            let id = app
                .seed_personal("a memory with a superseded revision")
                .await;
            let proposal = propose(&app, &id).await;
            let handle = proposal
                .rsplit_once(':')
                .expect("a proposal reference")
                .0
                .to_string();
            let stale = revise_form_on(&app, path, &handle).await;
            // Make the extracted control stale the way the world does it: another decision lands first.
            let (status, page) = app.get(path).await;
            assert_eq!(status, StatusCode::OK);
            let form = form_markup(&page, "/debug/classification/revise", &handle).expect("form");
            let (options, default) = select_options(form, "destination");
            let fresh: Vec<(String, String)> = stale
                .iter()
                .map(|(name, value)| {
                    if name == "destination" {
                        (
                            name.clone(),
                            options
                                .iter()
                                .filter(|v| v.as_str() != "project")
                                .find(|v| *v != &default)
                                .cloned()
                                .unwrap_or(default.clone()),
                        )
                    } else {
                        (name.clone(), value.clone())
                    }
                })
                .collect();
            app.post_exact("/debug/classification/revise", encode_body(&fresh))
                .await;

            let (status, location, _) = app
                .post_exact("/debug/classification/revise", encode_body(&stale))
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
            assert!(
                location.contains("error=")
                    && location.contains("changed%20since%20this%20page%20loaded"),
                "the superseded revise form was accepted rather than refused on {path}: {location}"
            );
            assert_eq!(
                store_proposal_revision(&app, &handle).await,
                1,
                "a superseded revise form moved the revision again on {path}"
            );
        }
    }

    /// Neither owner page renders a form the parser would move out of its table, or nest in another form.
    ///
    /// The checker carries its own positive control, because "zero violations" is also what a checker that
    /// never matches anything reports: a synthetic `<tr>`-parented form and a nested form both have to be
    /// caught by the same pass that reads the real pages.
    #[tokio::test]
    async fn no_owner_page_parents_a_form_from_a_table_row() {
        let app = authenticated_on_server(test_server().await).await;
        let id = app.seed_personal("a memory with a rendered decision").await;
        let proposal = propose(&app, &id).await;
        let handle = proposal
            .split(':')
            .next()
            .expect("a proposal reference")
            .to_string();
        for path in ["/debug/collections", "/debug/classification"] {
            let (status, page) = app.get(path).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                page.contains(r##"action="/debug/classification/revise""##),
                "{path} rendered no revise control to check"
            );
            let violations = table_form_violations(&page);
            assert!(
                violations.is_empty(),
                "{path} renders forms the parser cannot leave where they were written: {violations:?}"
            );
        }
        // Positive control: the two shapes the fix removed are shapes the checker reports.
        let broken = [
            r#"<table><tr><form action="/x"><input name="a" value="1"></form></tr></table>"#,
            r#"<form><table><tr><td><form action="/y"></form></td></tr></table></form>"#,
        ];
        for page in broken {
            assert!(
                !table_form_violations(page).is_empty(),
                "the shape check missed {page}"
            );
        }
        assert!(
            table_form_violations(
                r#"<table><tr><td><form action="/z"><input name="a" value="1"></form></td></tr></table>"#
            )
            .is_empty(),
            "the shape check flags a lawful td-contained form"
        );
        // And the extraction helpers themselves are proven against the live render, not only used by it:
        // a form whose revision field were dropped would leave `posted_pairs` reporting no such name.
        let (status, page) = app.get("/debug/collections").await;
        assert_eq!(status, StatusCode::OK);
        let form = form_markup(&page, "/debug/classification/revise", &handle).expect("form");
        let names = posted_pairs(form)
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"revision".to_string()),
            "the extractor cannot see a revision field the page renders: {names:?}"
        );
    }

    /// The revision a proposal row carries, read from the store rather than from any render.
    async fn store_proposal_revision(app: &AuthenticatedTestApp, handle: &str) -> i64 {
        store_proposal_destination(app, handle).await.1
    }

    /// The proposal's stored destination kind and revision.
    async fn store_proposal_destination(app: &AuthenticatedTestApp, handle: &str) -> (String, i64) {
        let rows: Vec<Json> = app
            .db
            .inner()
            .query(
                "SELECT destination_kind, revision FROM type::record('classification_proposal', $key) \
                 WHERE id = $id LIMIT 1;",
            )
            .bind((
                "key",
                handle
                    .split_once(':')
                    .map(|(_, key)| key.to_string())
                    .expect("a proposal handle"),
            ))
            .bind(("id", RecordId::parse_simple(handle).expect("a proposal id")))
            .await
            .expect("the proposal read ran")
            .check()
            .expect("the proposal read succeeded")
            .take(0)
            .expect("the proposal decodes");
        let row = rows
            .first()
            .unwrap_or_else(|| panic!("no stored proposal for {handle}"));
        (
            row["destination_kind"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            row["revision"].as_i64().unwrap_or(-1),
        )
    }

    #[tokio::test]
    async fn multi_pinned_cluster_effects_are_all_preserved_by_one_plan() {
        use crate::mutation::{ApplyService, ApprovedCluster, Plan};
        use alexandria_storage::models::{effect, operation_kind};
        let app = authenticated_on_server(test_server().await).await;
        let mut expected = Vec::new();
        for label in ["first pinned cluster", "second pinned cluster"] {
            let stayer = app.seed_personal(label).await;
            let cluster = app.seed_cluster(Some(label), &[0.1, 0.2]).await;
            app.add_cluster_member(&cluster, &stayer).await;
            expected.push(cluster);
        }
        let mut plan = Plan::new(operation_kind::STORE)
            .effect(effect::STORE)
            .effect(effect::VECTOR)
            .fact(alexandria_storage::models::FactChange::Create {
                scope: MemoryScope::Personal,
                content: "one member with two approved effects".into(),
                tags: vec![],
                confidence: 0.5,
                embedding: alexandria_storage::models::EmbeddingSnapshot::new(vec![0.1, 0.2], None),
                visibility: alexandria_storage::models::Visibility::live(),
                metadata: None,
            })
            .embedding(&[0.1, 0.2]);
        for cluster in &expected {
            plan = plan.cluster(ApprovedCluster::Join {
                cluster: RecordId::parse_simple(cluster).unwrap(),
                member: 0,
            });
        }
        let read = app.population().await;
        let applied = ApplyService::new(app.db.as_ref(), &app.owner, &read)
            .commit(&OperationId::new("multi-pinned"), &[], &plan)
            .await
            .unwrap();
        let rows: Vec<Json> = app
            .db
            .inner()
            .query("SELECT in FROM contains_memory WHERE out = $member ORDER BY in;")
            .bind(("member", applied.created[0].as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        let actual: Vec<String> = rows
            .iter()
            .map(|row| row["in"].as_str().unwrap().to_string())
            .collect();
        expected.sort();
        assert_eq!(
            actual, expected,
            "a plan must not replace an earlier pinned cluster effect"
        );
    }

    #[tokio::test]
    async fn archive_reports_truncation_only_when_a_further_session_exists() {
        let app = authenticated_on_server(test_server().await).await;
        let notice = "Showing the most recent page only";
        let (_, page) = app.get("/debug/collections/archive").await;
        assert!(!page.contains(notice));
        for n in 0..LIST_PAGE {
            app.seed_session(&format!("archive-{n}"), Some("archived summary"))
                .await;
        }
        let (status, page) = app.get("/debug/collections/archive").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page.matches("archived summary").count(), LIST_PAGE);
        assert!(
            !page.contains(notice),
            "an exactly full page is not truncated"
        );
        app.seed_session("archive-overflow", Some("archived summary"))
            .await;
        let (status, page) = app.get("/debug/collections/archive").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page.matches("archived summary").count(), LIST_PAGE);
        assert!(
            page.contains(notice),
            "the extra row must disclose truncation"
        );
    }

    #[tokio::test]
    async fn a_corrupt_destination_population_is_not_replaced_by_the_arriving_vector() {
        let app = authenticated_on_server(test_server().await).await;
        let project = app.own_project("corrupt-project").await;
        let destination = MemoryScope::Project(
            RecordId::parse_simple(&project)
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let corrupt = app
            .seed_cluster(Some("corrupt dimension"), &[0.1, 0.2])
            .await;
        for n in 0..2 {
            let member = app.seed_personal(&format!("corrupt member {n}")).await;
            assert!(
                app.move_row(&member, destination.clone(), &format!("seed-corrupt-{n}"))
                    .await
            );
            app.db
                .inner()
                .query("UPDATE $member SET embedding = [0.1, 0.2, 0.3];")
                .bind(("member", RecordId::parse_simple(&member).unwrap()))
                .await
                .unwrap()
                .check()
                .unwrap();
            app.add_cluster_member(&corrupt, &member).await;
        }
        let mover = app
            .seed_personal("a valid vector that must not overwrite the corrupt mean")
            .await;
        let revision = rendered_revision(&app, &mover).await;
        let (_, location, _) = app
            .post_form_located(
                "/debug/collections/move",
                &[
                    ("id", &mover),
                    ("revision", &revision),
                    ("destination", "project"),
                    ("project", &project),
                ],
            )
            .await;
        assert!(
            !location.contains("error="),
            "the scope move itself must still work: {location}"
        );
        let repo = MutationRepo::new(app.db.inner());
        let fact = FactId::try_from(RecordId::parse_simple(&mover).unwrap()).unwrap();
        assert_eq!(
            repo.expected_state(&app.owner, &fact).await.unwrap().scope,
            destination
        );
        assert!(
            repo.cluster_of(&app.owner, fact.as_record_id())
                .await
                .unwrap()
                .is_none(),
            "an incompatible population must leave the arrival ungrouped"
        );
        let rows: Vec<Json> = app
            .db
            .inner()
            .query("SELECT centroid FROM $cluster;")
            .bind(("cluster", RecordId::parse_simple(&corrupt).unwrap()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let centroid: Vec<f32> = serde_json::from_value(rows[0]["centroid"].clone()).unwrap();
        assert_eq!(centroid, vec![0.1, 0.2]);
    }

    #[tokio::test]
    async fn constant_vector_browser_moves_choose_the_destination_partition_not_the_source() {
        let app = authenticated_on_server(test_server().await).await;
        let a = app.own_project("project-a").await;
        let b = app.own_project("project-b").await;
        let a_scope = MemoryScope::Project(RecordId::parse_simple(&a).unwrap().try_into().unwrap());
        let b_scope = MemoryScope::Project(RecordId::parse_simple(&b).unwrap().try_into().unwrap());
        let b_member = app.seed_personal("classified into B first").await;
        let proposal = ClassificationRepo::new(app.db.inner(), &app.owner)
            .propose(
                &RecordId::parse_simple(&b_member)
                    .unwrap()
                    .try_into()
                    .unwrap(),
                Version::ZERO,
                b_scope.clone(),
                None,
            )
            .await
            .unwrap();
        let selected = format!("{}:{}", proposal.id, proposal.revision.get());
        let (_, location, _) = app
            .post_form_located(
                "/debug/classification/decide",
                &[("selected", &selected), ("decision", "approve")],
            )
            .await;
        assert!(
            location.contains("1%20moved"),
            "classification must create B's candidate: {location}"
        );

        let a_member = app.seed_personal("A's surviving member").await;
        assert!(app.move_row(&a_member, a_scope.clone(), "seed-a").await);
        let a_cluster = app.seed_cluster(Some("A"), &[0.1, 0.2]).await;
        app.add_cluster_member(&a_cluster, &a_member).await;
        let mover = app.seed_personal("same-vector memory moves twice").await;
        let personal_stayer = app.seed_personal("Personal's surviving member").await;
        let personal_cluster = app.seed_cluster(Some("Personal"), &[0.1, 0.2]).await;
        app.add_cluster_member(&personal_cluster, &mover).await;
        app.add_cluster_member(&personal_cluster, &personal_stayer)
            .await;
        let repo = MutationRepo::new(app.db.inner());
        let b_cluster = repo
            .cluster_of(&app.owner, &RecordId::parse_simple(&b_member).unwrap())
            .await
            .unwrap()
            .unwrap()
            .0;
        // A mixed cluster with a perfect centroid must never win destination ranking either.
        let mixed = app
            .seed_cluster(Some("historically mixed"), &[0.1, 0.2])
            .await;
        app.add_cluster_member(&mixed, &personal_stayer).await;
        app.add_cluster_member(&mixed, &b_member).await;
        let initial_count = one(
            &app,
            "SELECT count() AS n FROM semantic_operation GROUP ALL;",
        )
        .await;
        let target = RecordId::parse_simple(&mover).unwrap();
        let fact = FactId::try_from(target.clone()).unwrap();
        for (step, project, scope, cluster) in [
            (1, &a, a_scope, RecordId::parse_simple(&a_cluster).unwrap()),
            (2, &b, b_scope, b_cluster),
        ] {
            let path = if step == 1 {
                "/debug/collections"
            } else {
                "/debug/collections?partition=project"
            };
            let (_, revision) = rendered_form_on(&app, &mover, path).await;
            let (status, location, _) = app
                .post_form_located(
                    "/debug/collections/move",
                    &[
                        ("id", &mover),
                        ("revision", &revision),
                        ("destination", "project"),
                        ("project", project),
                    ],
                )
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
            assert!(
                !location.contains("error="),
                "constant-vector move failed: {location}"
            );
            assert_eq!(
                repo.expected_state(&app.owner, &fact).await.unwrap().scope,
                scope
            );
            assert_eq!(app.semantic_revision(&mover).await, step);
            assert_eq!(
                repo.cluster_of(&app.owner, &target)
                    .await
                    .unwrap()
                    .unwrap()
                    .0,
                cluster
            );
            assert_eq!(
                one(
                    &app,
                    "SELECT count() AS n FROM semantic_operation GROUP ALL;"
                )
                .await,
                initial_count + step as i64
            );
        }
        assert_ne!(personal_cluster, a_cluster);
    }

    #[tokio::test]
    async fn collections_is_reachable_from_the_navigation() {
        let app = authenticated_on_server(test_server().await).await;
        let (status, body) = app.get("/debug/collections").await;
        assert_eq!(status, StatusCode::OK);
        let nav = body
            .split("<nav>")
            .nth(1)
            .and_then(|rest| rest.split("</nav>").next())
            .expect("the page renders a navigation bar");
        assert!(
            nav.contains(r#"href="/debug/collections""#),
            "Collections is not in the navigation, so the owner's workflow is reachable only by URL: {nav}"
        );
        assert!(
            nav.contains(r#"class="active""#) && nav.contains(r#"href="/debug/collections" "#),
            "the Collections entry is not the highlighted one: {nav}"
        );
        // And every other page's nav links it too, which is what "discoverable" means.
        let (_, memories) = app.get("/debug/memories").await;
        assert!(
            memories.contains(r#"href="/debug/collections""#),
            "the memories page cannot reach the owner's collections"
        );
    }

    #[tokio::test]
    async fn every_owner_page_renders_for_a_signed_in_owner() {
        let app = authenticated_on_server(test_server().await).await;
        app.seed_personal("a memory to list").await;
        for path in [
            "/debug/collections",
            "/debug/collections?partition=project",
            "/debug/collections?partition=unassigned",
            "/debug/classification",
            "/debug/operations",
        ] {
            let (status, body) = app.get(path).await;
            assert_eq!(status, StatusCode::OK, "{path} -> {status}");
            assert!(
                body.contains("<table") || body.contains("Nothing"),
                "{path} rendered neither a table nor an honest empty state"
            );
        }
    }
}
