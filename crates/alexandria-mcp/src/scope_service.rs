//! Resolving *which* partition a request means, and the one path that may create a project.
//!
//! This is the layer where a caller's words become a storage handle, and the whole task lives in the
//! direction of that arrow. A tool parameter is *data*: `project: "alexandria"` is a claim about a
//! name, not a claim about a row, and nothing downstream may treat it as one. So every lawful
//! partition-bearing action here passes through a read that re-validates the name against the
//! **caller's own workspace** — and the authority for that workspace is an [`OwnerContext`] the
//! transport issued, never a value this crate can be handed.
//!
//! ## Four decisions this module owns
//!
//! **A selector is resolved, or it is established — and only one path establishes.**
//! [`ScopeService::establish_project`] is the resolve-*or-create* operation, reached from exactly one
//! tool (`resolve_project`, which is the design's explicit context establishment). Every other route —
//! a store naming a project, a search naming one, a proposal naming a destination — resolves through
//! [`ScopeService::resolve_selector`] and refuses a name it cannot find. That split is what keeps "the
//! agent typed a project nobody created" from silently minting a partition, which the design names
//! twice: an implicit create is how one conversation's guess becomes somebody's permanent layout.
//!
//! **A non-empty selector that cannot resolve is an error, never a fallback to Personal.** Refusing is
//! also the only *safe* answer: a personal fallback would write a memory that the caller's own
//! project-scoped read cannot see, and the response would say it was stored.
//!
//! **A proposal is a judgement, not an edit.** [`ScopeService::propose`] writes a pending row and
//! reports the revision it was formed against. There is no path from this module to moving a memory:
//! that is Task 11's approval, through Task 10's journal, under a decision this server has not seen.
//!
//! **Responses carry whole records or nothing.** [`fit_page`] trims at a record boundary and says so
//! (`truncated`, `oversize`, `next_cursor`); a memory text is never cut in half, because a truncated
//! `content` field reads exactly like a complete one to an agent — which is how a partial fact becomes
//! a confident memory.
//!
//! ## What is deliberately *not* here
//!
//! No caching of a "current project" (the design forbids one mutable account-level current project, and
//! a shared server has no such thing), no fuzzy alias matching, no rename, and no reading of the
//! Unassigned partition into any recall path.

use alexandria_engine::project_identity::{
    CanonicalRepoKey, RemoteTransport, SanitizedRemote, normalize_remote,
};
use alexandria_storage::Database;
use alexandria_storage::access::{OwnerContext, ScopedContext};
use alexandria_storage::models::{FactId, MemoryScope, ProjectId, ReadSelection, Version};
use alexandria_storage::repos::{
    ClassificationRepo, Established, ProjectEntry, ResolvedProject, ScopedMemoryRepo,
    ScopedProjectRepo,
};
use serde::Serialize;
use surrealdb::types::RecordId;

use crate::error::ServiceError;

/// Rows a scope listing returns when the caller asked for no page size.
pub const DEFAULT_SCOPE_PAGE_LIMIT: u32 = 20;

/// The largest page either listing will return. A caller asking for the whole store should page, for
/// the same reason `list_reminders` caps its page.
pub const MAX_SCOPE_PAGE_LIMIT: u32 = 100;

/// The largest serialized record array a scope listing will return.
///
/// A row cap alone is not a bound: one memory can be kilobytes of text, and a hundred of them is a
/// response no agent should be handed. 64 KiB is the response figure the design already commits to for
/// inference replies, so the tool surface reuses it rather than inventing a second number to tune.
pub const MAX_SCOPE_PAGE_BYTES: usize = 64 * 1024;

/// The most proposals one `propose_memory_scope` call may carry.
///
/// The design's per-pass figure applied to one request: an agent that wants to propose more does it in
/// passes, which is also what stops a batch becoming the way to write unbounded rows.
pub const MAX_PROPOSALS_PER_REQUEST: usize = 20;

/// A project, as the tool surface reports it.
///
/// `canonical_identity` is the alias the workspace keys on — a folded rendering of the engine's
/// canonical repo key, or the alias an owner chose — and `display_name` is the label a caller supplied.
/// They are separate fields because one is an identity and the other is prose: a client that conflated
/// them would treat a rename as a re-key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectDescriptor {
    pub id: String,
    pub display_name: Option<String>,
    pub canonical_identity: String,
}

impl ProjectDescriptor {
    fn from_row(project: &ResolvedProjectRow) -> Self {
        Self {
            id: alexandria_storage::project_handle::format_project_handle(
                project.id.as_record_id(),
            ),
            display_name: project.display_name.clone(),
            canonical_identity: project.normalized_alias.clone(),
        }
    }
}

/// The storage row shape this layer renders, named locally so the wire type reads as the tool's shape.
type ResolvedProjectRow = ResolvedProject;

/// The outcome of `establish_project`, which is the only path that can create a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EstablishedProject {
    pub descriptor: ProjectDescriptor,
    /// Whether *this* call created the project — how a client tells a first association from a replay
    /// without asking the store, and the only observable evidence that the alias did or did not
    /// already resolve here.
    pub established: bool,
}

/// Where a store or import is allowed to write, once this layer has resolved it.
///
/// Two arms, and the difference is not decoration: `Personal` is a partition, `Project` is a partition
/// *and* a row this caller owns. Storage re-validates the handle on the write itself
/// (`ScopedMemoryRepo::resolve_write_partition`), so this is a resolution and not a permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    Personal,
    Project {
        id: ProjectId,
        descriptor: ProjectDescriptor,
    },
}

impl ResolvedTarget {
    /// The `MemoryScope` the resolved partition spells, for a read selection or a proposal destination.
    pub fn memory_scope(&self) -> MemoryScope {
        match self {
            Self::Personal => MemoryScope::Personal,
            Self::Project { id, .. } => MemoryScope::Project(id.clone()),
        }
    }
}

/// The read population a retrieval or recall request asked for, and what to call it in the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    /// Personal only — the automatic shape with no project in play.
    Personal,
    /// Personal plus the current project, which is what "auto with a project" means.
    PersonalAndProject,
    /// Exactly one project.
    Project,
    /// Personal plus every owned project, and never Unassigned.
    Assigned,
}

impl ReadScope {
    /// The label the response reports for the selection that actually ran.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::PersonalAndProject => "personal+project",
            Self::Project => "project",
            Self::Assigned => "assigned",
        }
    }
}

/// The remote form's wire shape, turned into the engine's descriptor.
///
/// Refused with the engine's own reason, because "which part of what you sent is not a hostname" is a
/// fact about the submitted value and the caller has to be able to fix it.
pub fn sanitized_remote(
    host: &str,
    port: Option<u16>,
    path: &str,
    transport: &str,
) -> Result<SanitizedRemote, ServiceError> {
    let transport = RemoteTransport::parse(transport).ok_or_else(|| {
        ServiceError::InvalidInput(
            "transport must be 'https' or 'ssh' — a transport this server has not heard of has no \
             default port to normalize against"
                .to_string(),
        )
    })?;
    Ok(SanitizedRemote {
        host: host.to_string(),
        port,
        path: path.to_string(),
        transport,
    })
}

/// The canonical identity one descriptor denotes. Refusals carry the engine's text, which names the
/// offending field and never echoes the submitted value.
pub fn canonical_key(remote: &SanitizedRemote) -> Result<CanonicalRepoKey, ServiceError> {
    normalize_remote(remote)
        .map_err(|error| ServiceError::InvalidInput(format!("invalid project remote: {error}")))
}

/// The service every scope-tool handler routes through.
///
/// Holds the database handle and nothing else: no default owner, no cached project, no current
/// partition. An [`OwnerContext`] arrives per call from the transport's admission, which is the only
/// reason a `ScopeService` value can neither outlive an authorization nor answer for a second account.
pub struct ScopeService<'a> {
    db: &'a Database,
}

impl<'a> ScopeService<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Resolve-or-create the caller's project for one selector. The only create path in this module.
    ///
    /// An opaque `project:<key>` handle is *resolved*, never created: ids are minted by storage, so
    /// "create by the id somebody typed" would be adoption. An alias or a canonical remote is
    /// resolve-or-create, and storage's establish is guarded, workspace-qualified and idempotent under
    /// a concurrent same-alias race — so two clients on one repository get one project and one handle.
    pub async fn establish_project(
        &self,
        owner: &OwnerContext,
        text: Option<&str>,
        remote: Option<&SanitizedRemote>,
        display_name: Option<&str>,
    ) -> Result<EstablishedProject, ServiceError> {
        let alias = match (text, remote) {
            (Some(text), None) => match parse_project_handle(text)? {
                Some(handle) => {
                    let project = ScopedProjectRepo::new(self.db.inner(), owner)
                        .resolve_id(&handle)
                        .await
                        .map_err(ServiceError::from)?;
                    return Ok(EstablishedProject {
                        descriptor: ProjectDescriptor::from_row(&project),
                        established: false,
                    });
                }
                None => text.trim().to_lowercase(),
            },
            (None, Some(remote)) => canonical_key(remote)?.to_alias(),
            (Some(_), Some(_)) => {
                return Err(ServiceError::InvalidInput(
                    "supply `project` or `remote`, not both: they are two names for one selector"
                        .to_string(),
                ));
            }
            (None, None) => {
                return Err(ServiceError::InvalidInput(
                    "resolve_project needs `project` (an opaque id or alias) or `remote` (a \
                     sanitized origin descriptor)"
                        .to_string(),
                ));
            }
        };
        let Established { project, created } = ScopedProjectRepo::new(self.db.inner(), owner)
            .establish(&alias, display_name)
            .await
            .map_err(ServiceError::from)?;
        Ok(EstablishedProject {
            descriptor: ProjectDescriptor::from_row(&project),
            established: created,
        })
    }

    /// Resolve a caller-named selector to that caller's own project, creating nothing.
    ///
    /// A handle that is another workspace's and a handle that was never minted both refuse as
    /// `NotFound`, and an alias that does not resolve here refuses the same way: the refusal is not an
    /// oracle for what exists elsewhere. An *empty* name is the caller's own mistake and says so.
    pub async fn resolve_selector(
        &self,
        owner: &OwnerContext,
        text: &str,
    ) -> Result<ResolvedProjectRow, ServiceError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ServiceError::InvalidInput(
                "project must not be empty — omit the parameter to leave the scope unassigned"
                    .to_string(),
            ));
        }
        let repo = ScopedProjectRepo::new(self.db.inner(), owner);
        if let Some(handle) = parse_project_handle(text)? {
            return repo.resolve_id(&handle).await.map_err(ServiceError::from);
        }
        // Not a handle: an alias. The resolver's own order (primary identity, then the explicit alias
        // table) is what "resolves" means here. An alias that resolves to nothing is `NotFound`, which is
        // the same answer a foreign project handle gives — the resolver cannot tell the caller which of
        // the two it looked at.
        repo.resolve_alias(text)
            .await
            .map_err(ServiceError::from)?
            .ok_or(ServiceError::NotFound)
    }

    /// Where a store/import may write, from the tool's own three parameters.
    ///
    /// The order is the contract, and every arm is reachable from a real client:
    ///
    /// 1. an explicit `scope` wins over any context — a caller that said so meant it;
    /// 2. `scope: 'project'` requires a resolvable `project`, else `context_project`, and creates
    ///    nothing;
    /// 3. an omitted `scope` uses `context_project` when one is present (and a lone `project` names the
    ///    target just as well — both are caller-supplied, so neither can be a guess);
    /// 4. no scope and no context: a **trusted-local** caller may still mean Personal; a hosted caller
    ///    is refused with [`SCOPE_REQUIRED`].
    ///
    /// Arm 4 is owner decision D4, and what Task 8 changed is only *how* a target becomes lawful — the
    /// ordering that mattered is intact: this runs **before** the first `embed` and before any write, so
    /// a refusal costs no provider work and leaves nothing for the caller to wonder about.
    pub async fn resolve_write_target(
        &self,
        owner: &OwnerContext,
        scope: Option<&str>,
        project: Option<&str>,
        context_project: Option<&str>,
        trusted_local: bool,
    ) -> Result<ResolvedTarget, ServiceError> {
        let scope = word(scope);
        // `project` and `context_project` are both caller-supplied names for the same thing, and the
        // explicit one wins: a stale context the client failed to refresh must not outrank a target the
        // request names for this memory.
        let named_target = word(project).or_else(|| word(context_project));
        match scope {
            Some("personal") => Ok(ResolvedTarget::Personal),
            Some("project") => match named_target {
                Some(text) => self.project_target(owner, text).await,
                None => Err(ServiceError::InvalidInput(
                    "scope 'project' needs `project` or `context_project`; nothing was stored"
                        .to_string(),
                )),
            },
            Some(other) => Err(ServiceError::InvalidInput(format!(
                "unknown scope '{other}': use 'personal' or 'project'"
            ))),
            None => match named_target {
                Some(text) => self.project_target(owner, text).await,
                None if trusted_local => Ok(ResolvedTarget::Personal),
                // D4's text, unchanged: Task 6 established the wording and its tests assert it. What
                // Task 8 changed is only *when* a target is lawful, which is the arms above.
                None => Err(ServiceError::InvalidInput(
                    crate::server::SCOPE_REQUIRED.to_string(),
                )),
            },
        }
    }

    /// One named selector, resolved to a write target.
    async fn project_target(
        &self,
        owner: &OwnerContext,
        text: &str,
    ) -> Result<ResolvedTarget, ServiceError> {
        let resolved = self.resolve_selector(owner, text).await?;
        Ok(ResolvedTarget::Project {
            id: resolved.id.clone(),
            descriptor: ProjectDescriptor::from_row(&resolved),
        })
    }

    /// The read population a retrieval or recall request asked for.
    ///
    /// `auto` is the design's automatic-recall shape: Personal, plus the named project when the caller
    /// supplied one. The arms that say two different things (`personal` with a project, `assigned` with
    /// a project, `project` without one) refuse rather than guess, because guessing is the inference
    /// this layer exists to withhold. `assigned` stays exactly what storage means by it — Personal plus
    /// every owned project, **never** Unassigned.
    pub async fn resolve_read_selection(
        &self,
        owner: &OwnerContext,
        scope: Option<&str>,
        project: Option<&str>,
    ) -> Result<(ReadSelection, ReadScope), ServiceError> {
        let scope = word(scope).unwrap_or("auto");
        let named = word(project);
        let built = match (scope, named) {
            ("auto", None) => (ReadScope::Personal, ReadSelection::Auto(None)),
            ("auto", Some(text)) => (
                ReadScope::PersonalAndProject,
                ReadSelection::Auto(Some(self.resolve_selector(owner, text).await?.id)),
            ),
            ("personal", None) => (
                ReadScope::Personal,
                ReadSelection::One(MemoryScope::Personal),
            ),
            ("project", Some(text)) => (
                ReadScope::Project,
                ReadSelection::One(MemoryScope::Project(
                    self.resolve_selector(owner, text).await?.id,
                )),
            ),
            ("assigned", None) => (ReadScope::Assigned, ReadSelection::Assigned),
            ("project", None) => {
                return Err(ServiceError::InvalidInput(
                    "scope 'project' needs a `project` to search".to_string(),
                ));
            }
            ("personal" | "assigned", Some(_)) => {
                return Err(ServiceError::InvalidInput(
                    "scope 'personal' and scope 'assigned' take no `project`: the first excludes \
                     projects, the second already spans all of yours"
                        .to_string(),
                ));
            }
            (other, _) => {
                return Err(ServiceError::InvalidInput(format!(
                    "unknown scope '{other}': use 'auto', 'personal', 'project' or 'assigned'"
                )));
            }
        };
        Ok((built.1, built.0))
    }

    /// One page of the caller's projects, each with its live memory count, trimmed to whole records.
    ///
    /// Storage reads one row of slack so a continuation is a fact about the store rather than a guess
    /// from a full page; the byte trim can only cut at a boundary, and when it does the cursor points
    /// at the *last returned* row so the next page resumes there.
    pub async fn list_projects(
        &self,
        owner: &OwnerContext,
        after_alias: Option<&str>,
        limit: u32,
    ) -> Result<ProjectPage, ServiceError> {
        let limit = clamp_scope_limit(limit);
        let page = ScopedProjectRepo::new(self.db.inner(), owner)
            .list(
                after_alias.map(str::trim).filter(|text| !text.is_empty()),
                limit as usize,
            )
            .await
            .map_err(ServiceError::from)?;
        // One row of slack again, so `next_cursor` reports a continuation the store proved rather than
        // a page that happened to be full — and when the byte cap trims, the cursor is the last row the
        // caller actually received, which cannot skip a record.
        let read_rows = page.entries.len();
        let (entries, oversize) = fit_page(page.entries, project_size);
        let trimmed = read_rows > entries.len();
        let next_cursor = match (oversize, trimmed) {
            (true, _) | (false, true) => entries
                .last()
                .map(|entry| entry.project.normalized_alias.clone()),
            (false, false) => page.next_cursor,
        };
        Ok(ProjectPage {
            entries,
            limit,
            next_cursor,
            oversize,
        })
    }

    /// One page of the caller's Unassigned partition, for owner review.
    ///
    /// `read` must be a context resolved from `ReadSelection::UnassignedReview` — the one door storage
    /// gives that partition, and the reason a review page cannot be reached from a recall path. The
    /// view carries the disclosure because a client that sees rows here must not conclude they are
    /// recallable: they are not, until an owner classifies them.
    pub async fn list_unassigned(
        &self,
        owner: &OwnerContext,
        read: &ScopedContext,
        after: Option<&str>,
        limit: u32,
    ) -> Result<UnassignedPage, ServiceError> {
        let limit = clamp_scope_limit(limit);
        let after = match word(after) {
            None => None,
            Some(text) => Some(FactId::try_from(parse_record_handle(text).ok_or_else(
                || ServiceError::InvalidInput("malformed review cursor".to_string()),
            )?)?),
        };
        // One row of slack, so "more exists" is a fact about the store rather than a guess from a full
        // page — and the cursor is then always the last row the caller was *given*, which is the only
        // position that cannot skip or repeat a record when the byte cap also cuts.
        let rows = ScopedMemoryRepo::new(self.db.inner(), owner, read)
            .review_page(
                after.as_ref().map(|id| id.as_record_id()),
                limit as usize + 1,
            )
            .await
            .map_err(ServiceError::from)?;
        let has_more = rows.len() as u32 > limit;
        let mut rows = rows;
        if has_more {
            rows.pop();
        }
        let (rows, oversize) = fit_page(rows, fact_size);
        let last = rows
            .last()
            .and_then(|row| row.id.as_ref())
            .map(alexandria_storage::record_id_to_string);
        Ok(UnassignedPage {
            next_cursor: (has_more || oversize).then_some(last).flatten(),
            rows,
            limit,
            oversize,
        })
    }

    /// The destination a proposal entry asked for, resolved to a scope.
    ///
    /// A `project` destination must name a project the caller owns, and any *other* destination must not
    /// carry a project: a half-filled destination is exactly the ambiguous request a human reviewer would
    /// then have to guess about, and guessing on the owner's behalf is what this surface never does.
    pub async fn resolve_destination(
        &self,
        owner: &OwnerContext,
        destination: &str,
        project: Option<&str>,
    ) -> Result<(MemoryScope, Option<ProjectDescriptor>), ServiceError> {
        match (
            destination.trim().to_ascii_lowercase().as_str(),
            word(project),
        ) {
            ("personal", None) => Ok((MemoryScope::Personal, None)),
            ("unassigned", None) => Ok((MemoryScope::Unassigned, None)),
            ("project", Some(text)) => {
                let resolved = self.resolve_selector(owner, text).await?;
                Ok((
                    MemoryScope::Project(resolved.id.clone()),
                    Some(ProjectDescriptor::from_row(&resolved)),
                ))
            }
            ("project", None) => Err(ServiceError::InvalidInput(
                "destination 'project' needs a `project` to move it to".to_string(),
            )),
            ("personal" | "unassigned", Some(_)) => Err(ServiceError::InvalidInput(
                "destination 'personal' and 'unassigned' take no `project`".to_string(),
            )),
            (other, _) => Err(ServiceError::InvalidInput(format!(
                "unknown destination '{other}': use 'personal', 'project' or 'unassigned'"
            ))),
        }
    }

    /// Record one proposal: pending, owner-stamped, revision-checked, and applied by nobody.
    pub async fn propose(
        &self,
        owner: &OwnerContext,
        fact: &FactId,
        expected: Version,
        destination: MemoryScope,
        reason: Option<&str>,
    ) -> Result<ProposedScope, ServiceError> {
        let reference = ClassificationRepo::new(self.db.inner(), owner)
            .propose(fact, expected, destination.clone(), reason)
            .await
            .map_err(ServiceError::from)?;
        Ok(ProposedScope {
            proposal_id: reference.id,
            revision: reference.revision.get(),
            destination: destination_label(&destination),
        })
    }
}

/// Where a proposal says a memory should go, as the tool words it.
pub fn destination_label(destination: &MemoryScope) -> String {
    match destination {
        MemoryScope::Personal => "personal".to_string(),
        MemoryScope::Unassigned => "unassigned".to_string(),
        MemoryScope::Project(_) => "project".to_string(),
    }
}

/// A proposal entry that named no memory at all, or one this caller cannot see.
pub fn parse_fact_handle(text: &str) -> Option<FactId> {
    parse_record_handle(text)
        .filter(|id| id.table.as_str() == "fact")
        .and_then(|id| FactId::try_from(id).ok())
}

/// One page of projects, as the tool answers.
#[derive(Debug, Clone)]
pub struct ProjectPage {
    pub entries: Vec<ProjectEntry>,
    pub limit: u32,
    pub next_cursor: Option<String>,
    /// The byte cap, not the row limit, ended this page.
    pub oversize: bool,
}

/// One page of the Unassigned review population, as the tool answers.
#[derive(Debug, Clone)]
pub struct UnassignedPage {
    pub rows: Vec<alexandria_storage::models::Fact>,
    pub limit: u32,
    pub next_cursor: Option<String>,
    pub oversize: bool,
}

/// What one accepted proposal entry became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedScope {
    pub proposal_id: String,
    pub revision: u64,
    pub destination: String,
}

/// The page size a caller asked for, clamped to the bound this surface publishes.
fn clamp_scope_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_SCOPE_PAGE_LIMIT)
}

/// An optional parameter as a trimmed, non-empty word — so `""` and `"   "` are the same *absence* as
/// omitting the field, which is the reading a client's JSON encoder gives them, decided in one place.
fn word(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// A caller-supplied `project:<key>` handle, or `None` when the text is an alias instead.
fn parse_project_handle(text: &str) -> Result<Option<ProjectId>, ServiceError> {
    match alexandria_storage::project_handle::parse_project_selector(text) {
        Ok(project) => Ok(project),
        // Handle-namespace text that is not a valid canonical point names nothing. The refusal is the
        // same `NotFound` a foreign or never-minted handle gets, so a caller cannot tell "mine but
        // malformed" from "not mine" — the uniformity `scope_tools_test` pins — and it is never
        // re-read as an alias, so no project-prefixed text can establish or adopt a name. Reminder
        // targets keep the loud strict refusal instead: they are a different contract.
        Err(_) if text.starts_with("project:") => Err(ServiceError::NotFound),
        Err(error) => Err(ServiceError::from(error)),
    }
}

/// A `table:key` handle, parsed and re-checked for the table it claims.
///
/// `parse_simple` rather than a string split, and `type::record` on the storage side rather than this
/// text in a statement: AGENTS.md records the display form as **not** a parser-safe round trip for
/// numeric and quoted opaque keys, and nothing here interpolates what it parses.
fn parse_record_handle(text: &str) -> Option<RecordId> {
    RecordId::parse_simple(text).ok()
}

/// Trim a page to whole records within [`MAX_SCOPE_PAGE_BYTES`], reporting what was dropped.
///
/// At least one record always survives: a page that returned *nothing* because the first row was large
/// would be indistinguishable from an empty population, which is the failure this whole pass is written
/// against. The `oversize` flag tells a client the cap ended the page rather than the store running out,
/// and the cursor then names the last row the caller was *given*, so no record can be skipped.
///
/// Sizes come from a caller-supplied measure rather than from `Serialize`: the row types here are
/// storage's, and making them serializable to satisfy a byte cap would widen another crate's public
/// shape for a page-trimming concern that belongs to this one.
fn fit_page<T>(rows: Vec<T>, size_of: impl Fn(&T) -> usize) -> (Vec<T>, bool) {
    let mut kept: Vec<T> = Vec::with_capacity(rows.len().min(128));
    let mut bytes = 0usize;
    for row in rows {
        let size = size_of(&row);
        if !kept.is_empty() && bytes + size > MAX_SCOPE_PAGE_BYTES {
            return (kept, true);
        }
        bytes += size;
        kept.push(row);
    }
    (kept, false)
}

/// The bytes one project row costs a response: its alias and label are the only unbounded parts, and an
/// alias is bounded by what a host and a repository path can hold, so this is a small fixed estimate
/// with the variable text measured rather than assumed.
fn project_size(entry: &ProjectEntry) -> usize {
    entry.project.normalized_alias.len()
        + entry.project.display_name.as_ref().map_or(0, String::len)
        + 128
}

/// The bytes one memory row costs a response. The content is the only field that can be large, and it
/// is the field the cap exists to keep whole.
fn fact_size(fact: &alexandria_storage::models::Fact) -> usize {
    fact.content.len() + fact.tags.iter().map(|tag| tag.len() + 4).sum::<usize>() + 128
}
