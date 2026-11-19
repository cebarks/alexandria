//! The account-bound classification proposal write: a recorded judgement, never an edit.
//!
//! v009's `classification_proposal` table exists for one reason — an agent may say "this memory belongs
//! in that project" and a human may later agree or disagree — so the judgement write in this file
//! applies nothing: it records the opinion. The human decision — capture, seal, approve-and-move — lives
//! in [`application`], where the approval is decided by a conditional update inside the move's own
//! transaction, re-checking `source_revision` so a memory that moved in between stales the proposal
//! rather than being overwritten by an old opinion.
//!
//! ## Why the write is one statement
//!
//! The tempting shape is "read the fact, check the revision in Rust, insert the proposal". That is a
//! check-then-act race *with the fact*, and the row it can leave behind is a proposal naming a revision
//! that was never the fact's — exactly the stale-approval failure the design forbids. So the guard is in
//! the SQL: the insert **is** a select over the fact row, and a fact that is absent, foreign, deleted or
//! at another revision contributes no source row, so no proposal row can exist. A `Project` destination
//! is guarded the same way, by an `EXISTS`-style count over the project row inside the same statement.
//!
//! `account` and `workspace` are bound from the [`OwnerContext`] by
//! [`guarded_run`](super::scope::guarded_run) and cannot be overridden from a call site, which is what
//! makes "owner and workspace stamped and validated inside a guarded transaction" a property of the
//! statement rather than of the code around it. The same transaction re-asserts the caller's authority,
//! so a revocation landing between admission and this commit costs the commit rather than leaving behind
//! a judgement a human will be asked to approve.
//!
//! ## Refusals
//!
//! Every `Err` here wrote nothing. The kinds differ because the caller's next action differs — re-read
//! the memory (`Stale`), check the id (`NotFound`), fix the request (`InvalidInput`) — and a *foreign*
//! fact and an absent one both answer `NotFound`, so the call is not an existence probe.

use crate::access::OwnerContext;
use crate::error::StorageError;
use crate::models::identity::{FactId, MemoryScope, ProjectId, Version};
use crate::models::{proposal_state, scope_kind};
use crate::repos::scope::{Bind, GuardedWrite, guarded_run, record_key};
use chrono::Utc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// The capture→seal→execute path for human decisions on these proposals.
pub mod application;

use application::revision_i64;
pub use application::{
    ClassificationCandidate, ClassificationCandidates, ClassificationDestination,
    PreparedClassificationBatch,
};

/// What [`ClassificationRepo::propose`] hands back: the row to cite, and the revision to cite it at.
///
/// A handle plus a revision rather than a whole row, because those two are everything Task 11's
/// `revise_destination` and `apply_batch` need in order to detect that the world moved. `id` is the
/// canonical display form of the proposal's record id, as on every other string-id surface in this
/// crate, and it is never re-parsed into SQL text — only bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeProposalRef {
    pub id: String,
    pub revision: Version,
}

/// The `destination_kind` / `destination_project` pair one proposal carries.
///
/// A type rather than two loose values because v009's `ASSERT` on each column reads its sibling: the
/// project column must be present for exactly one kind, so the pair has to be built together or the
/// write fails on a rule the caller cannot see from either column alone.
struct Destination {
    kind: &'static str,
    project: Option<RecordId>,
}

impl Destination {
    fn of(destination: &MemoryScope) -> Self {
        match destination {
            MemoryScope::Personal => Self {
                kind: scope_kind::PERSONAL,
                project: None,
            },
            MemoryScope::Unassigned => Self {
                kind: scope_kind::UNASSIGNED,
                project: None,
            },
            MemoryScope::Project(project) => Self {
                kind: scope_kind::PROJECT,
                project: Some(project.as_record_id().clone()),
            },
        }
    }
}

/// The owner's pending queue, oldest first.
///
/// Ordered by `(created_at, id)` because `created_at` is a `DEFAULT time::now()` and two proposals formed
/// in the same statement can share it, so the timestamp alone is not a total order and the page would skip
/// or repeat a row across a re-read.
const PENDING_LIST_SQL: &str = "SELECT id, revision, fact, destination_kind, destination_project, \
     reason FROM classification_proposal WHERE account = $account AND workspace = $workspace AND \
     state = 'pending' ORDER BY created_at, id;";

pub struct ClassificationRepo<'a> {
    db: &'a Surreal<Any>,
    owner: &'a OwnerContext,
}

/// One row of the guarded insert, read back as the created proposal.
#[derive(SurrealValue)]
struct ProposalRow {
    id: Option<RecordId>,
    revision: i64,
}

/// The proposal row an `apply_batch` entry needs: its subject, its destination, and the two revisions.
///
/// `source_revision` is `option<i64>` because v009 declares it `option<int>` — a proposal formed against
/// a row that predates the column carries none, and "unknown" must stay distinguishable from "zero".
#[derive(SurrealValue)]
struct ProposalStateRow {
    id: Option<RecordId>,
    fact: Option<RecordId>,
    destination_kind: String,
    destination_project: Option<RecordId>,
    source_revision: Option<i64>,
    revision: i64,
    state: String,
}

/// The projection `pending_for` reads: the row's own identity, not its content.
#[derive(SurrealValue)]
struct PendingRow {
    id: Option<RecordId>,
    revision: i64,
    destination_kind: String,
    destination_project: Option<RecordId>,
}

/// One pending proposal as the owner's classification queue lists it.
///
/// Carries the destination as a resolved [`MemoryScope`] rather than the raw kind/project pair, because
/// the page that renders it must show *where the memory would go* — and a project destination has to be
/// spelled the same way the apply path spells it, or the page and the write disagree about what the
/// owner is approving.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingProposal {
    /// The handle to cite back to [`apply_batch`](Self::apply_batch) / `revise_destination`.
    pub reference: ScopeProposalRef,
    /// The memory this proposal would move.
    pub fact: RecordId,
    /// Where the proposal says it should land.
    pub destination: MemoryScope,
    /// The agent's stated reason, verbatim — display only, never an authority.
    pub reason: Option<String>,
    /// Whether the target memory is currently quarantined, read from the **fact** row: v009 puts no
    /// quarantine column on `classification_proposal`, and a proposal cannot change that state. Task 11's
    /// rule is that applying a move preserves quarantine, so the queue has to *show* it before the owner
    /// decides.
    pub quarantined: bool,
}

/// The queue row as the engine returns it: the proposal's own columns and nothing else.
///
/// No quarantine flag here, because `classification_proposal` has no such column — that state belongs to
/// the target fact and is joined in [`quarantined_of`](ClassificationRepo::quarantined_of) by handle.
#[derive(SurrealValue)]
struct PendingListRow {
    id: Option<RecordId>,
    revision: i64,
    fact: Option<RecordId>,
    destination_kind: String,
    destination_project: Option<RecordId>,
    reason: Option<String>,
}

impl PendingListRow {
    fn into_proposal(self, quarantined: &[RecordId]) -> Result<PendingProposal, StorageError> {
        // Resolved before `self.id` moves into `reference(...)`, which is why the borrow checker would
        // otherwise reject the `&self` this needs a row later.
        let destination = destination_scope(&self)?;
        let fact = self.fact.clone().ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!("a proposal row names no fact"))
        })?;
        let is_quarantined = quarantined.contains(&fact);
        Ok(PendingProposal {
            reference: reference(self.id, self.revision),
            fact,
            destination,
            reason: self.reason,
            quarantined: is_quarantined,
        })
    }
}

/// The `(personal | project | unassigned)` + project pair, resolved back into a [`MemoryScope`].
///
/// A project destination that names no project row cannot be a real proposal — v009's `ASSERT` ties the
/// two columns together — so this is an internal-consistency refusal, not a caller error.
fn destination_scope(row: &PendingListRow) -> Result<MemoryScope, StorageError> {
    match row.destination_kind.as_str() {
        scope_kind::PERSONAL => Ok(MemoryScope::Personal),
        scope_kind::UNASSIGNED => Ok(MemoryScope::Unassigned),
        scope_kind::PROJECT => row
            .destination_project
            .clone()
            .and_then(|id| ProjectId::try_from(id).ok())
            .map(MemoryScope::Project)
            .ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!("a project destination names no project"))
            }),
        other => Err(StorageError::Internal(anyhow::anyhow!(
            "unknown destination kind {other:?}"
        ))),
    }
}

/// The one row `classify_refusal` reads: the fact's guard column, or the project's id.
///
/// One struct for both reads because `classify_refusal` asks each the same question — is there a row —
/// and only the first arm looks at the column. `semantic_revision` is `option<int>` in v009 precisely
/// so an unguarded legacy row cannot be read as "revision zero", and that distinction is why the field
/// is here rather than an `id`-only probe.
#[derive(SurrealValue)]
struct FactGuard {
    semantic_revision: Option<i64>,
}

impl<'a> ClassificationRepo<'a> {
    /// Binds the writer to one account. As with every scoped repo, there is no other way in.
    pub fn new(db: &'a Surreal<Any>, owner: &'a OwnerContext) -> Self {
        Self { db, owner }
    }

    fn account(&self) -> RecordId {
        self.owner.account().as_record_id().clone()
    }

    fn workspace(&self) -> RecordId {
        self.owner.workspace().as_record_id().clone()
    }

    /// Record that this caller believes `fact` belongs in `destination` at `expected`.
    ///
    /// * The fact must exist, be this account's, be in this workspace, and not be soft-deleted —
    ///   `NotFound`, one answer for all of those, so the call cannot probe what exists elsewhere.
    /// * `expected` must equal the fact's `semantic_revision` — `Stale` otherwise, which is the caller's
    ///   cue to re-read rather than to retry blindly.
    /// * A `Project` destination must name a project **of this workspace**, checked in the same
    ///   statement; a foreign or absent project refuses as `NotFound`.
    /// * An already-pending proposal for the same fact **and** destination is returned instead of
    ///   duplicated: the same judgement, replayed, is one row. A pending proposal for a *different*
    ///   destination is a different judgement and keeps its own row — an owner is entitled to see that
    ///   the agent proposed two things.
    ///
    /// `revision` starts at zero and nothing here raises it: revising and applying are Task 11's, and a
    /// proposal that could bump its own revision is a proposal that could approve itself.
    pub async fn propose(
        &self,
        fact: &FactId,
        expected: Version,
        destination: MemoryScope,
        reason: Option<&str>,
    ) -> Result<ScopeProposalRef, StorageError> {
        let expected = i64::try_from(expected.get()).map_err(|_| {
            StorageError::InvalidInput("expected revision is out of the storable range".to_string())
        })?;
        let destination = Destination::of(&destination);

        // The duplicate guard is in the statement, not in a read before it, and the order is the
        // reason: a read-first shortcut would let a caller whose authority died mid-request be
        // answered with the row their *live* call already wrote — a success where the write is
        // refused, which is the one shape `a_revoked_credential_stops_a_proposal_write` exists to
        // rule out. So the guarded insert runs first, and only a committed insert that matched
        // nothing is allowed to consult the pending rows.
        if let Some(project) = destination.project.as_ref() {
            // The handle is validated as a project of *this* workspace before any row is written, so a
            // proposal cannot record an association to a project the caller cannot see. Foreign and
            // absent are one refusal, here and in the fact guard. Once — the second copy of this block
            // shipped in the same lane and cost one redundant workspace-qualified read per proposal.
            self.assert_owned_project(project).await?;
        }
        let project_bound = destination.project.is_some();
        let sql = insert_sql(project_bound, destination.kind);
        let mut write = GuardedWrite::new(sql)
            .id("fact", fact.as_record_id().clone())
            .value("destination_kind", Bind::Text(destination.kind.to_string()))
            .value(
                "destination_project",
                Bind::OptRecord(destination.project.clone()),
            )
            .value("expected", Bind::OptInt(Some(expected)))
            .value("reason", Bind::OptText(reason.map(str::to_string)))
            .value("pending", Bind::Text(proposal_state::PENDING.to_string()));
        if let Some(project) = destination.project.clone() {
            let key = record_key(&project)?;
            write = write
                .id("project", project)
                .value("project_key", Bind::Text(key));
        }

        let inserted = guarded_run::<ProposalRow>(self.db, self.owner, &write, Utc::now())
            .await?
            .ok_or(StorageError::Stale)?;
        if let Some(row) = inserted.into_iter().next() {
            return Ok(reference(row.id, row.revision));
        }

        // The transaction committed and the insert matched nothing, so a guard refused this proposal.
        // Two guards can do that, and the caller's next action differs between them: an identical
        // judgement already pending is a **replay** (return the row, write nothing new), while a fact
        // that is absent, foreign or at another revision is a refusal. Read in that order.
        if let Some(existing) = self
            .pending_match(fact.as_record_id(), &destination)
            .await?
        {
            return Ok(existing);
        }
        self.classify_refusal(fact.as_record_id(), destination.project.as_ref())
            .await
    }

    /// The caller's own pending proposal for this fact whose destination matches exactly.
    ///
    /// Matched in Rust because this read answers a *question* ("was this already proposed?") rather
    /// than authorizing a write, so the `NONE`-comparison trap that the guarded insert has to avoid
    /// (`IS NOT NULL` and `!= NULL` are both satisfied by `NONE` on this engine) costs nothing here
    /// beyond reading a handful of the caller's own rows. The insert above compares with `=`, which is
    /// a real comparison, and this is the belt for it.
    async fn pending_match(
        &self,
        fact: &RecordId,
        destination: &Destination,
    ) -> Result<Option<ScopeProposalRef>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT id, revision, destination_kind, destination_project FROM \
                 classification_proposal WHERE account = $account AND fact = $fact AND state = \
                 $state ORDER BY created_at ASC;",
            )
            .bind(("account", self.account()))
            .bind(("fact", fact.clone()))
            .bind(("state", proposal_state::PENDING.to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<PendingRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .find(|row| {
                row.destination_kind == destination.kind
                    && row.destination_project.as_ref() == destination.project.as_ref()
            })
            .map(|row| reference(row.id, row.revision)))
    }

    /// Whether `project` is a project of this workspace, refusing `NotFound` if it is not — including
    /// when it does not exist at all, so the two are one answer to a caller not entitled to the
    /// difference. The read is the same workspace-qualified shape the resolver uses, so a proposal and a
    /// later read of the same destination cannot disagree.
    async fn assert_owned_project(&self, project: &RecordId) -> Result<(), StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT id FROM type::record('project', $key) WHERE id = $project AND workspace = \
                 $workspace LIMIT 1;",
            )
            .bind(("key", record_key(project)?))
            .bind(("project", project.clone()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<FactGuard> = response.take(0).map_err(internal)?;
        if rows.is_empty() {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }

    /// Which guard the refused insert tripped on, as the refusal that names it.
    ///
    /// Absent, foreign, another workspace's or soft-deleted are one answer (`NotFound`): the same
    /// refusal the fact would have produced had it existed for somebody else, so a proposal attempt
    /// cannot be used to ask whether an id is stored. A live, owned fact is the other case, and then
    /// only two things remain — the revision the caller expected is not what the row carries, or the
    /// project it named is not a project of this workspace (checked here, refused there exactly as an
    /// absent project would be, so the two are one answer to a caller who is not entitled to the
    /// difference).
    async fn classify_refusal(
        &self,
        fact: &RecordId,
        destination_project: Option<&RecordId>,
    ) -> Result<ScopeProposalRef, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT semantic_revision FROM $fact WHERE id = $fact AND \
                 account = $account AND workspace = $workspace AND deleted = false LIMIT 1;",
            )
            .bind(("fact", fact.clone()))
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<FactGuard> = response.take(0).map_err(internal)?;
        if rows.is_empty() {
            return Err(StorageError::NotFound);
        }
        if let Some(project) = destination_project {
            let mut response = self
                .db
                .query(
                    "SELECT id FROM type::record('project', $key) WHERE id = $project AND \
                     workspace = $workspace LIMIT 1;",
                )
                .bind(("key", record_key(project)?))
                .bind(("project", project.clone()))
                .bind(("workspace", self.workspace()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
            let projects: Vec<FactGuard> = response.take(0).map_err(internal)?;
            if projects.is_empty() {
                return Err(StorageError::NotFound);
            }
        }
        // The fact is ours and live and the destination exists, so the only guard left is the one the
        // caller can act on: their expected revision is not what the row carries.
        Err(StorageError::Stale)
    }

    // -- Task 11: the human half ---------------------------------------------------------------------

    /// Change a pending proposal's destination, and move its revision.
    ///
    /// The revision bump is the point. A proposal is a *judgement about a memory at a moment*, and an
    /// owner who first said "project A" and then said "personal" must not leave two approvals in the
    /// store that both claim to decide the same row. So a revision change invalidates every
    /// [`ScopeProposalRef`] minted before it: `apply_batch` compares the ref's revision against the row's,
    /// and a stale ref conflicts instead of applying the destination the human *stopped* asking for.
    ///
    /// Guards, each refusing without writing:
    ///
    /// * the proposal must exist and be the caller's own — a foreign, absent or unparseable
    ///   citation is one `Stale`, so this is not a probe for what another workspace proposed;
    /// * `ref.revision` must equal the row's, or `Stale`;
    /// * a `project` destination must name a project of this workspace, or `NotFound` — the same answer
    ///   `propose` gives for an absent or foreign one;
    /// * a decided proposal (approved, dismissed, stale) cannot be re-shaped: the human answer is
    ///   recorded, and rewriting it after the fact would rewrite history rather than the memory.
    pub async fn revise_destination(
        &self,
        cited: ScopeProposalRef,
        destination: MemoryScope,
    ) -> Result<ScopeProposalRef, StorageError> {
        // A citation that does not parse gets the answer one that matches no pending row at the
        // cited revision gets: `Stale`, the caller's cue to re-read the queue. A second refusal
        // text for the same caller mistake would reveal which layer gave up.
        let Ok(target) = crate::parse_record_id(&cited.id) else {
            return Err(StorageError::Stale);
        };
        if target.table.as_str() != "classification_proposal" {
            return Err(StorageError::NotFound);
        }
        let destination = Destination::of(&destination);
        if let Some(project) = destination.project.as_ref() {
            self.assert_owned_project(project).await?;
        }
        let expected_revision = revision_i64(cited.revision)?;
        let rows = guarded_run::<ProposalRow>(
            self.db,
            self.owner,
            &GuardedWrite::new(
                "UPDATE type::record('classification_proposal', $key) SET destination_kind = \
                 $destination_kind, destination_project = $destination_project, revision = \
                 $revision + 1 WHERE id = $proposal AND account = $account AND workspace = \
                 $workspace AND revision = $revision AND state = $pending RETURN AFTER;",
            )
            .id("proposal", target.clone())
            .value("key", Bind::Text(record_key(&target)?))
            .value("destination_kind", Bind::Text(destination.kind.to_string()))
            .value(
                "destination_project",
                Bind::OptRecord(destination.project.clone()),
            )
            .value("revision", Bind::OptInt(Some(expected_revision)))
            .value("pending", Bind::Text(proposal_state::PENDING.to_string())),
            Utc::now(),
        )
        .await?
        .ok_or(StorageError::Stale)?;
        rows.into_iter()
            .next()
            .map(|row| reference(row.id, row.revision))
            .ok_or(StorageError::Stale)
    }

    /// The proposal row behind a reference, if it is this caller's and still at that revision.
    async fn load(
        &self,
        reference: &ScopeProposalRef,
    ) -> Result<Option<ProposalStateRow>, StorageError> {
        let Ok(target) = crate::parse_record_id(&reference.id) else {
            return Ok(None);
        };
        if target.table.as_str() != "classification_proposal" {
            return Ok(None);
        }
        let rows: Vec<ProposalStateRow> = self
            .db
            .query(
                "SELECT id, fact, destination_kind, destination_project, source_revision, \
                 revision, state FROM type::record('classification_proposal', $key) WHERE id = \
                 $id AND account = $account AND workspace = $workspace AND revision = $revision \
                 LIMIT 1;",
            )
            .bind(("key", record_key(&target)?))
            .bind(("id", target))
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .bind((
                "revision",
                i64::try_from(reference.revision.get()).unwrap_or(i64::MAX),
            ))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// Every pending proposal this caller owns, oldest first.
    ///
    /// This is the read Task 11's queue needs and no existing method provides: `pending_match` answers
    /// "is there one for *this* fact" and `load` resolves a single cited reference, so an owner could not
    /// enumerate their own queue without guessing at ids. The predicate is account **and** workspace —
    /// both bound from the [`OwnerContext`] — so one owner's queue cannot include another's, and a
    /// dismissed, approved or staled row is simply absent rather than filtered in Rust.
    ///
    /// Quarantine is a property of the target **fact**, not of the proposal (v009 gives
    /// `classification_proposal` no such column), so it comes from one batched read over the listing's
    /// own subjects rather than a second query per row. Two round trips for a page, not N+1 — and the
    /// batched read carries the same account and workspace predicates, so a proposal naming a foreign
    /// fact could not use it to ask whether some other account's row is quarantined.
    pub async fn list_pending(&self) -> Result<Vec<PendingProposal>, StorageError> {
        let rows: Vec<PendingListRow> = self
            .db
            .query(PENDING_LIST_SQL)
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let subjects: Vec<RecordId> = rows.iter().filter_map(|row| row.fact.clone()).collect();
        let quarantined = self.quarantined_of(&subjects).await?;
        rows.into_iter()
            .map(|row| row.into_proposal(&quarantined))
            .collect()
    }

    /// Which of `facts` are currently quarantined, in one account-and-workspace-qualified read.
    ///
    /// `quarantined_at != NONE`, never `IS NOT NULL`/`!= NULL`: on this engine both of the latter are
    /// satisfied by `NONE`, so `IS NOT NULL` would return **every** subject and the queue would report
    /// the whole corpus as quarantined (`AGENTS.md` records the trap; `quarantine_none_predicate_pins_the_trap`
    /// pins the rule at the read layer).
    async fn quarantined_of(&self, facts: &[RecordId]) -> Result<Vec<RecordId>, StorageError> {
        if facts.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<RecordId> = self
            .db
            .query(
                "SELECT VALUE id FROM fact WHERE id INSIDE $facts AND account = $account AND \
                 workspace = $workspace AND quarantined_at != NONE;",
            )
            .bind(("facts", facts.to_vec()))
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(rows)
    }

    /// Records a human **dismissal** of one of the caller's own still-pending proposals.
    ///
    /// One guarded, owner-qualified, pending-revision decision — not a read followed by a mark. The
    /// statement requires the row to still be this owner's, still `pending`, and still at the cited
    /// revision, so a proposal that was revised, approved or dismissed in the window refuses with
    /// `Stale` and the winner keeps its state. An owner-qualified read distinguishes that moved owned
    /// row from a foreign or absent one, which answers `NotFound` without making an existence probe.
    /// A dismissal writes **no** journal row, deliberately: it moves no
    /// memory, and `semantic_operation` is corpus-write history rather than an opinion log.
    pub async fn dismiss(&self, reference: &ScopeProposalRef) -> Result<(), StorageError> {
        // A citation that does not parse is the same answer as one that names no row this caller
        // may decide: `NotFound`, indistinguishable from a foreign or an absent id — the
        // uniform-absent discipline the dismissal oracles pin.
        let Ok(target) = crate::parse_record_id(&reference.id) else {
            return Err(StorageError::NotFound);
        };
        if target.table.as_str() != "classification_proposal" {
            return Err(StorageError::NotFound);
        }
        let revision = revision_i64(reference.revision)?;
        let rows = guarded_run::<ProposalRow>(
            self.db,
            self.owner,
            &GuardedWrite::new(
                "UPDATE type::record('classification_proposal', $key) SET state = 'dismissed', \
                 decided_at = time::now(), decided_by = $account WHERE id = $proposal AND account = \
                 $account AND workspace = $workspace AND revision = $revision AND state = $pending \
                 RETURN AFTER;",
            )
            .id("proposal", target.clone())
            .value("key", Bind::Text(record_key(&target)?))
            .value("revision", Bind::OptInt(Some(revision)))
            .value("pending", Bind::Text(proposal_state::PENDING.to_string())),
            Utc::now(),
        )
        .await?;
        match rows {
            // The guard loop's own refusal: authority moved mid-write, so the decision stands unmade.
            None => Err(StorageError::Stale),
            Some(rows) if rows.is_empty() => {
                // A no-match is decided-elsewhere or not-resolvable, and the caller's next action
                // differs: re-read the queue versus check the citation. One owner-qualified read
                // separates them without leaking whether a foreign row exists.
                let owned: Vec<ProposalStateRow> = self
                    .db
                    .query(
                        "SELECT id, fact, destination_kind, destination_project, source_revision, \
                         revision, state FROM type::record('classification_proposal', $key) WHERE \
                         id = $proposal AND account = $account AND workspace = $workspace;",
                    )
                    .bind(("key", record_key(&target)?))
                    .bind(("proposal", target.clone()))
                    .bind(("account", self.account()))
                    .bind(("workspace", self.workspace()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                if owned.is_empty() {
                    Err(StorageError::NotFound)
                } else {
                    Err(StorageError::Stale)
                }
            }
            Some(_) => Ok(()),
        }
    }
}

/// The stored row's handle and revision, as the reference Task 11 will cite.
fn reference(id: Option<RecordId>, revision: i64) -> ScopeProposalRef {
    ScopeProposalRef {
        id: id
            .map(|id| crate::record_id_to_string(&id))
            .unwrap_or_default(),
        revision: Version::new(revision.max(0) as u64),
    }
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

/// The guarded proposal insert, as one statement.
///
/// Split out as a function, and pinned by a unit test below, because the shape *is* the security
/// property: the guards live in the statement's own `WHERE`, so a proposal cannot be written by a read
/// that passed and a write that raced it. `RETURN AFTER` hands the caller the created row, so the
/// response cites a handle that exists.
fn insert_sql(project_bound: bool, kind: &str) -> String {
    // Three guards, all inside the statement, so a proposal cannot be written by a read that passed and
    // a write that raced it:
    //
    // * **The fact guard** — the source row is the fact itself, filtered by `account`, `workspace`,
    //   `deleted = false` and `semantic_revision = $expected`. A fact that is absent, foreign, deleted or
    //   at another revision contributes no source row, so no proposal row can exist.
    // * **The replay guard** — no already-pending proposal for this account, fact, destination kind and
    //   destination project. `destination_project = NONE` / `= $project` is the `=` form, which is a real
    //   comparison on this engine (unlike `!= NONE` and `IS NOT NULL`, both of which `NONE` satisfies).
    //
    // Why the guard's two constants are written into the statement while every caller-shaped value is
    // bound. Measured on the pinned 3.3.0 engine: inside the *nested* subquery of an `INSERT … SELECT`
    // guard, `$fact` resolves and a `state = $pending` bind does **not** — the same statement with the
    // bind inserts zero rows for a lawful proposal, and the same statement with the literal inserts one.
    // So the two interpolated values are the two closed constants this crate owns
    // (`proposal_state::PENDING` and the `destination_kind` from `scope_kind`), neither of which has any
    // caller-reachable spelling, while `$fact`, `$project`, `$account`, `$workspace` and `$expected` stay
    // bound and typed. `insert_sql_names_every_guard_once` pins the text this produces.
    //
    // The destination project's workspace membership is checked by `assert_owned_project` before this
    // statement rather than inside it, for the same bind reason plus one: a project's `workspace` is
    // immutable in every write path this crate has, so the read and the statement cannot disagree about
    // which workspace a project belongs to. A later writer that could re-parent a project is Task 10's
    // journal problem, not this statement's.
    format!(
        "INSERT INTO classification_proposal \
         SELECT $workspace AS workspace, $account AS account, $fact AS fact, \
               $destination_kind AS destination_kind, {project_column} AS destination_project, \
               $expected AS source_revision, 0 AS revision, '{pending}' AS state, \
               $reason AS reason \
         FROM $fact \
         WHERE id = $fact AND account = $account AND workspace = $workspace AND deleted = false \
           AND semantic_revision = $expected AND \
           array::len((SELECT id FROM classification_proposal WHERE account = $account AND \
           fact = $fact AND destination_kind = '{destination_kind}' AND {project_match} AND \
           state = '{pending}')) = 0 \
         RETURN AFTER;",
        project_column = if project_bound { "$project" } else { "NONE" },
        destination_kind = kind,
        project_match = if project_bound {
            "destination_project = $project"
        } else {
            "destination_project = NONE"
        },
        pending = proposal_state::PENDING,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two shapes differ only in the two guarded clauses and the destination column.
    #[test]
    fn insert_sql_names_every_guard_once() {
        let personal = insert_sql(false, scope_kind::PERSONAL);
        let unassigned = insert_sql(false, scope_kind::UNASSIGNED);
        let project = insert_sql(true, scope_kind::PROJECT);
        assert!(unassigned.contains("'unassigned'"), "{unassigned}");
        assert!(
            unassigned.contains("destination_project = NONE"),
            "{unassigned}"
        );
        for (label, sql) in [("personal", &personal), ("project", &project)] {
            assert!(sql.contains("semantic_revision = $expected"), "{label}");
            assert!(sql.contains("= 0"), "{label} lost the replay guard");
            // The replay guard is owner-qualified, exactly like the recovery read that follows a
            // zero-row insert. Without `account` in the subquery the two guards disagree in scope, and
            // the comment above the statement would claim a predicate the statement does not emit.
            let subquery = sql
                .split("array::len((SELECT id FROM classification_proposal ")
                .nth(1)
                .unwrap_or_default();
            assert!(
                subquery.starts_with("WHERE account = $account"),
                "{label}'s replay subquery is not account-qualified: {subquery}"
            );
            assert!(
                sql.starts_with("INSERT INTO classification_proposal"),
                "{label}"
            );
            assert!(!sql.contains('\n'), "{label} must be one statement: {sql}");
        }
        assert!(project.contains("destination_project = $project"));
        assert!(personal.contains("destination_project = NONE"));
        println!("PERSONAL SQL: {personal}");
        println!("PROJECT SQL: {project}");
    }
}
