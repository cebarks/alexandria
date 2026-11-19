//! Durable refinement work: the fair queue, attempt lifecycle, rolling call budgets and the
//! separate enqueue-request stamp. The worker never issues SQL of its own; everything durable
//! lives here, behind owner- or system-checked repo methods.

use chrono::{DateTime, TimeDelta, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::method::Transaction;
use surrealdb::types::{RecordId, SurrealValue, ToSql, Value};

use super::{
    AuthorityRecord, EvidenceRecord, EvidenceSelection, PolicySnapshot, RefinementRepo, can_retry,
    internal, require_active, scope_fields, select_record, sha256,
};
use crate::Database;
use crate::access::{OwnerContext, SystemContext};
use crate::error::StorageError;
use crate::models::Provenance;
use crate::models::identity::{
    AccountId, FactId, MemoryScope, OperationId, Partition, ProjectId, Version, WorkspaceId,
};
use crate::models::mutation::{EdgeKind, ExpectedState, RelationExpectation};
use crate::models::refinement::ApprovedRelationshipEffect;
use crate::models::refinement::{
    ApprovalMode, Authority, Candidate, EffectivePolicy, EvidenceBundle, EvidenceCoverage,
    EvidenceSelectionKind, InferencePolicyId, OmissionReason, OmittedNeighbor, ProposalCategory,
    ProposalRevision, ProposalState, QualityOperation,
};
use crate::parse_record_id;
use crate::repos::scope::GUARD_CONFLICT_ATTEMPTS;

/// Interactive work classes: owner-initiated requests that share the per-account interactive
/// queue and call allowances. Scheduled assessment is deliberately absent.
const INTERACTIVE_KINDS: [&str; 3] = ["assess_unassigned", "opinion", "regenerate"];

/// Queue protection limits from the frozen design. Storage cannot read the engine's constants
/// (the dependency points the other way), so these are storage's own enforcement of the same
/// literals the engine defaults advertise.
const MAX_ACCOUNT_INTERACTIVE_QUEUE: usize = 20;
const MAX_GLOBAL_QUEUE: usize = 200;
/// The rolling charge window: one 24-hour day.
const ROLLING_WINDOW_SECS: i64 = 86_400;
/// The `system_config` keys the separate enqueue cadence and worker generation live under.
const ENQUEUE_REQUESTED_KEY: &str = "refinement.enqueue.requested_at";
const ENQUEUE_COMPLETED_KEY: &str = "refinement.enqueue.completed_at";
const WORKER_GENERATION_KEY: &str = "refinement.worker.generation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Assess,
    Opinion,
    Regenerate,
    AssessUnassigned,
}

impl WorkKind {
    /// The value the v014 `kind` column carries. Byte-exact, like `Job::as_str`.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkKind::Assess => "assess",
            WorkKind::Opinion => "opinion",
            WorkKind::Regenerate => "regenerate",
            WorkKind::AssessUnassigned => "assess_unassigned",
        }
    }

    fn from_row(kind: &str) -> Result<Self, StorageError> {
        match kind {
            "assess" => Ok(WorkKind::Assess),
            "opinion" => Ok(WorkKind::Opinion),
            "regenerate" => Ok(WorkKind::Regenerate),
            "assess_unassigned" => Ok(WorkKind::AssessUnassigned),
            _ => Err(StorageError::Internal(anyhow::anyhow!(
                "refinement attempt carries an unknown work kind"
            ))),
        }
    }

    /// Whether this class shares the interactive queue and interactive call allowance.
    fn is_interactive(self) -> bool {
        matches!(
            self,
            WorkKind::Opinion | WorkKind::Regenerate | WorkKind::AssessUnassigned
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptId(RecordId);

impl AttemptId {
    /// The typed row this attempt lives at, for repo-internal updates.
    pub fn record(&self) -> &RecordId {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl AttemptState {
    fn as_str(self) -> &'static str {
        match self {
            AttemptState::Queued => "queued",
            AttemptState::Running => "running",
            AttemptState::Completed => "completed",
            AttemptState::Cancelled => "cancelled",
            AttemptState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkSelection {
    Scheduled,
    Evidence(EvidenceSelection),
    Proposal(RecordId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueRequest {
    pub owner: AccountId,
    pub partition: Partition,
    pub kind: WorkKind,
    pub source_revision: Option<Version>,
    pub selection: WorkSelection,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptView {
    pub id: AttemptId,
    pub kind: WorkKind,
    pub state: AttemptState,
}

/// One queued attempt, as the system-side worker sees it. The authority is the snapshot bound
/// at enqueue time; dispatch re-checks it against the live account, consent and policy before
/// any selection, reservation or call.
pub struct QueuedWork {
    pub id: AttemptId,
    pub account: AccountId,
    pub partition: Partition,
    pub kind: WorkKind,
    pub selection: WorkSelection,
    pub authority: Authority,
    pub interactive: bool,
}

impl QueuedWork {
    /// The round-robin unit: one account's one partition in one work class. Serving a key moves
    /// it to the back of the line by simple exclusion — the next dispatch picks the first queued
    /// unit that is not the one just served.
    pub fn fairness_key(&self) -> (AccountId, Partition, bool) {
        (
            self.account.clone(),
            self.partition.clone(),
            self.interactive,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallLimits {
    pub pass: usize,
    pub account: usize,
    pub interactive: usize,
    pub global: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDenial {
    Pass,
    Account,
    Interactive,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reservation {
    Granted,
    Denied(BudgetDenial),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub pass: usize,
    pub account: usize,
    pub interactive: usize,
    pub global: usize,
}

impl RefinementRepo<'_> {
    /// Admit and persist one unit of refinement work. Idempotent per (owner, key); opinion and
    /// regeneration for one proposal revision coalesce onto the already-queued attempt so no two
    /// run concurrently. Refuses suspended accounts, absent or revoked consent, and a policy the
    /// owner never granted under, before anything is queued.
    pub async fn enqueue(
        &self,
        owner: &OwnerContext,
        request: EnqueueRequest,
    ) -> Result<AttemptId, StorageError> {
        if request.partition.workspace != *owner.workspace() || request.owner != *owner.account() {
            return Err(StorageError::InvalidInput(
                "refinement work is outside the owner workspace".into(),
            ));
        }
        if request.idempotency_key.trim().is_empty() || request.idempotency_key.len() > 256 {
            return Err(StorageError::InvalidInput(
                "refinement idempotency key is blank or too long".into(),
            ));
        }
        let interactive = request.kind.is_interactive();
        let record = RecordId::new(
            "refinement_attempt",
            sha256(
                format!(
                    "{}:{}",
                    owner.account().as_record_id().to_sql(),
                    request.idempotency_key
                )
                .as_bytes(),
            ),
        );
        self.write(async |tx| {
            require_active(tx, owner.account(), Some(owner)).await?;
            let effective = PolicySnapshot::read(tx, owner.account())
                .await?
                .effective()?;
            let authority =
                bind_authority(owner.account(), &effective).ok_or(StorageError::Unavailable)?;
            if select_record::<AttemptRow>(tx, record.clone())
                .await?
                .is_some()
            {
                // A resubmission of the same key is the same work, whatever state it reached.
                return Ok(AttemptId(record.clone()));
            }
            if let WorkSelection::Proposal(proposal) = &request.selection
                && matches!(request.kind, WorkKind::Opinion | WorkKind::Regenerate)
            {
                let queued: Vec<RecordId> = tx
                    .query(
                        "SELECT VALUE id FROM refinement_attempt \
                         WHERE account = $account AND proposal = $proposal AND state = 'queued' \
                         LIMIT 1",
                    )
                    .bind(("account", owner.account().as_record_id().clone()))
                    .bind(("proposal", proposal.clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                if let Some(id) = queued.into_iter().next() {
                    return Ok(AttemptId(id));
                }
            }
            if interactive {
                let queued: Vec<RecordId> = tx
                    .query(
                        "SELECT VALUE id FROM refinement_attempt \
                         WHERE state = 'queued' AND account = $account AND kind IN $kinds",
                    )
                    .bind(("account", owner.account().as_record_id().clone()))
                    .bind((
                        "kinds",
                        INTERACTIVE_KINDS
                            .iter()
                            .map(|kind| (*kind).to_owned())
                            .collect::<Vec<_>>(),
                    ))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                if queued.len() >= MAX_ACCOUNT_INTERACTIVE_QUEUE {
                    return Err(StorageError::Quota);
                }
            }
            let queued: Vec<RecordId> = tx
                .query("SELECT VALUE id FROM refinement_attempt WHERE state = 'queued'")
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if queued.len() >= MAX_GLOBAL_QUEUE {
                return Err(StorageError::Quota);
            }
            let (scope, project) = scope_fields(&request.partition.scope);
            let row = AttemptRecord {
                account: owner.account().as_record_id().clone(),
                workspace: request.partition.workspace.as_record_id().clone(),
                project,
                scope,
                kind: request.kind.as_str().to_owned(),
                idempotency_key: request.idempotency_key.clone(),
                selection: SelectionRecord::from(&request.selection),
                source_revision: request.source_revision.map(|version| version.get()),
                proposal: match &request.selection {
                    WorkSelection::Proposal(proposal) => Some(proposal.clone()),
                    _ => None,
                },
                state: "queued".to_owned(),
                authority: AuthorityRecord::from(&authority),
            };
            tx.query("CREATE $id CONTENT $row")
                .bind(("id", record.clone()))
                .bind(("row", row.into_value()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
            Ok(AttemptId(record.clone()))
        })
        .await
    }

    /// An owner-scoped read of one attempt. Another owner's id is indistinguishable from a
    /// missing one.
    pub async fn attempt(
        &self,
        owner: &OwnerContext,
        id: &AttemptId,
    ) -> Result<AttemptView, StorageError> {
        let rows: Vec<AttemptRow> = self
            .db
            .inner()
            .query("SELECT * FROM $id")
            .bind(("id", id.0.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let row = rows.into_iter().next().ok_or(StorageError::NotFound)?;
        if row.account != *owner.account().as_record_id() {
            return Err(StorageError::NotFound);
        }
        Ok(AttemptView {
            id: AttemptId(row.id.ok_or_else(missing_row_id)?),
            kind: WorkKind::from_row(&row.kind)?,
            state: attempt_state(&row.state)?,
        })
    }

    /// Every queued attempt, oldest first. The worker applies fairness on top of this order.
    pub async fn queued_work(
        &self,
        system: &SystemContext,
    ) -> Result<Vec<QueuedWork>, StorageError> {
        self.require_system(system)?;
        let rows: Vec<AttemptRow> = self
            .db
            .inner()
            .query(
                "SELECT * FROM refinement_attempt WHERE state = 'queued' ORDER BY created_at ASC",
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        rows.into_iter().map(queued_work).collect()
    }

    /// Look up one explicit assigned attempt without scanning the server's queue.
    pub async fn queued_assessment_at(
        &self,
        system: &SystemContext,
        owner: &OwnerContext,
        partition: &Partition,
        id: &AttemptId,
    ) -> Result<Option<QueuedWork>, StorageError> {
        self.require_system(system)?;
        if &partition.workspace != owner.workspace() || partition.scope == MemoryScope::Unassigned {
            return Err(StorageError::NotFound);
        }
        let (scope, project) = scope_fields(&partition.scope);
        let rows: Vec<AttemptRow> = self.db.inner().query(
            "SELECT * FROM $id WHERE state = 'queued' AND kind = 'assess' \
             AND account = $account AND workspace = $workspace AND scope = $scope AND project = $project",
        )
        .bind(("id", id.0.clone()))
        .bind(("account", owner.account().as_record_id().clone()))
        .bind(("workspace", partition.workspace.as_record_id().clone()))
        .bind(("scope", scope))
        .bind(("project", project))
        .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
        rows.into_iter().next().map(queued_work).transpose()
    }

    /// Atomically lease one attempt for dispatch. `false` means somebody else claimed it first.
    pub async fn claim_work(
        &self,
        system: &SystemContext,
        id: &AttemptId,
    ) -> Result<bool, StorageError> {
        self.require_system(system)?;
        let rows: Vec<Value> = self
            .db
            .inner()
            .query(
                "UPDATE $id SET state = 'running', lease_until = time::now() \
                 WHERE state = 'queued' RETURN AFTER",
            )
            .bind(("id", id.0.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(rows.len() == 1)
    }

    /// Settle a running attempt. Refuses to overwrite a state another writer already moved.
    pub async fn finish_work(
        &self,
        system: &SystemContext,
        id: &AttemptId,
        state: AttemptState,
    ) -> Result<(), StorageError> {
        self.require_system(system)?;
        let rows: Vec<Value> = self
            .db
            .inner()
            .query("UPDATE $id SET state = $state, lease_until = NONE WHERE state = 'running' RETURN AFTER")
            .bind(("id", id.0.clone()))
            .bind(("state", state.as_str()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if rows.len() == 1 {
            Ok(())
        } else {
            Err(StorageError::Stale)
        }
    }

    /// Cancel still-queued work whose authority no longer holds. Revocation cancels queued
    /// authority, not just in-flight calls.
    pub async fn cancel_queued(
        &self,
        system: &SystemContext,
        id: &AttemptId,
    ) -> Result<(), StorageError> {
        self.require_system(system)?;
        self.db
            .inner()
            .query("UPDATE $id SET state = 'cancelled' WHERE state = 'queued' RETURN NONE")
            .bind(("id", id.0.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        Ok(())
    }

    /// Boot-time recovery: return interrupted (running) attempts to the queue.
    pub async fn requeue_interrupted(&self, system: &SystemContext) -> Result<usize, StorageError> {
        self.require_system(system)?;
        let rows: Vec<Value> = self
            .db
            .inner()
            .query(
                "UPDATE refinement_attempt SET state = 'queued', lease_until = NONE \
                 WHERE state = 'running' RETURN AFTER",
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(rows.len())
    }

    /// Return one running attempt to the queue: a spent budget is not a failure, and the
    /// work must stay dispatchable once the window refills.
    pub async fn requeue_work(
        &self,
        system: &SystemContext,
        id: &AttemptId,
    ) -> Result<(), StorageError> {
        self.require_system(system)?;
        self.db
            .inner()
            .query(
                "UPDATE $id SET state = 'queued', lease_until = NONE \
                 WHERE state = 'running' RETURN NONE",
            )
            .bind(("id", id.0.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        Ok(())
    }

    /// Load one stored proposal revision with the evidence it was grounded in, for secondary
    /// review. The payloads are the canonical JSON the persist path wrote.
    pub async fn proposal_with_evidence(
        &self,
        system: &SystemContext,
        id: &RecordId,
    ) -> Result<(ProposalRevision, EvidenceBundle), StorageError> {
        self.require_system(system)?;
        if id.table.as_str() != "refinement_proposal" {
            return Err(StorageError::NotFound);
        }
        let rows: Vec<ProposalFullRow> = self
            .db
            .inner()
            .query("SELECT * FROM $id")
            .bind(("id", id.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let row = rows.into_iter().next().ok_or(StorageError::NotFound)?;
        let evidence_rows: Vec<EvidenceRecord> = self
            .db
            .inner()
            .query("SELECT * FROM $evidence")
            .bind(("evidence", row.evidence.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let evidence_row = evidence_rows.into_iter().next().ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!("proposal evidence row is missing"))
        })?;
        let evidence = decode_evidence(&evidence_row)?;
        let payload: serde_json::Value = serde_json::from_str(&row.proposal_payload)
            .map_err(|error| StorageError::Internal(anyhow::Error::new(error)))?;
        let proposal = decode_proposal(row, payload, evidence.clone())?;
        Ok((proposal, evidence))
    }

    /// The persisted enqueue-request stamp and completed stamp, in whole seconds.
    pub async fn refinement_progress(
        &self,
        system: &SystemContext,
    ) -> Result<(u64, Option<u64>), StorageError> {
        self.require_system(system)?;
        let requested = read_stamp(self.db.inner(), ENQUEUE_REQUESTED_KEY).await?;
        let completed = read_stamp(self.db.inner(), ENQUEUE_COMPLETED_KEY).await?;
        Ok((requested.unwrap_or(0), completed))
    }

    /// Issue the short, idempotent request for a refinement pass. `Some` means this call issued
    /// exactly one new request; concurrent or overlapping calls coalesce onto the first. No
    /// maintenance audit row is written: enqueueing is a request, not a housekeeping action.
    ///
    /// The retry loop is inlined and its transaction body is a plain free `async fn`, not a
    /// generic `AsyncFn` action: this future is awaited from the spawned dreaming loop, whose
    /// `Send` obligation a higher-ranked action closure cannot satisfy.
    pub async fn refinement_enqueue_request(
        &self,
        system: &SystemContext,
        interval_secs: u64,
        now_secs: u64,
    ) -> Result<Option<(u64, Option<u64>)>, StorageError> {
        self.require_system(system)?;
        let db = self.db.inner().clone();
        for _ in 0..GUARD_CONFLICT_ATTEMPTS {
            // `Surreal::begin` consumes its receiver, so each attempt takes a fresh clone of
            // the shared handle.
            let tx = db.clone().begin().await.map_err(internal)?;
            let error = match enqueue_request_transaction(&tx, interval_secs, now_secs).await {
                Ok(issued) => match tx.commit().await {
                    Ok(_) => return Ok(issued),
                    Err(error) => internal(error),
                },
                Err(error) => {
                    tx.cancel().await.map_err(internal)?;
                    error
                }
            };
            if !can_retry(&error) {
                return Err(error);
            }
            tokio::task::yield_now().await;
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    /// The current worker generation. A candidate or in-flight call from an older generation
    /// cannot authorize application.
    pub async fn worker_generation(&self, system: &SystemContext) -> Result<u64, StorageError> {
        self.require_system(system)?;
        Ok(read_stamp(self.db.inner(), WORKER_GENERATION_KEY)
            .await?
            .unwrap_or(0))
    }

    /// Invalidate every outstanding generation by advancing it. Called by the supervisor's
    /// replacement path and boot-time recovery.
    pub async fn bump_worker_generation(
        &self,
        system: &SystemContext,
    ) -> Result<u64, StorageError> {
        self.require_system(system)?;
        self.write(async |tx| {
            let current = read_stamp_tx(tx, WORKER_GENERATION_KEY).await?.unwrap_or(0);
            let next = current.checked_add(1).ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!("generation space exhausted"))
            })?;
            write_stamp_tx(tx, WORKER_GENERATION_KEY, next).await?;
            Ok(next)
        })
        .await
    }
}

/// One enqueue-request attempt inside an open transaction. A plain `async fn` with by-value
/// parameters, so the spawned dreaming loop's `Send` obligation is computed over concrete
/// lifetimes rather than through a higher-ranked action closure's generic future.
async fn enqueue_request_transaction(
    tx: &Transaction<Any>,
    interval_secs: u64,
    now_secs: u64,
) -> Result<Option<(u64, Option<u64>)>, StorageError> {
    let requested = read_stamp_tx(tx, ENQUEUE_REQUESTED_KEY).await?;
    // Downtime coalesces: any number of missed intervals is one request, now.
    let due = requested.is_none_or(|last| last.saturating_add(interval_secs) <= now_secs);
    if !due {
        return Ok(None);
    }
    match requested {
        Some(seen) => {
            let rows: Vec<Value> = tx
                .query(
                    "UPDATE system_config SET value = $now, updated_at = time::now() \
                     WHERE key = $key AND value = $seen RETURN AFTER",
                )
                .bind(("now", now_secs.to_string()))
                .bind(("key", ENQUEUE_REQUESTED_KEY))
                .bind(("seen", seen.to_string()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if rows.is_empty() {
                // A concurrent tick issued this request first; it is issued.
                return Ok(None);
            }
        }
        None => {
            tx.query(
                "CREATE system_config SET key = $key, value = $now, \
                 updated_at = time::now() RETURN AFTER",
            )
            .bind(("key", ENQUEUE_REQUESTED_KEY))
            .bind(("now", now_secs.to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        }
    }
    let completed = read_stamp_tx(tx, ENQUEUE_COMPLETED_KEY).await?;
    Ok(Some((now_secs, completed)))
}

/// The rolling call budgets. One reserve is one outbound attempt, including retries; every
/// reserve charges the origin pass, the account, and the global window, and interactive work
/// charges the account's interactive allowance as well. Rejected authority charges nothing.
pub struct BudgetRepo<'a> {
    db: &'a Database,
    limits: CallLimits,
}

impl<'a> BudgetRepo<'a> {
    pub fn new(db: &'a Database, limits: CallLimits) -> Self {
        Self { db, limits }
    }

    pub async fn reserve_attempt(
        &self,
        expected_policy: &InferencePolicyId,
        account: &AccountId,
        _partition: &Partition,
        pass: &PassId,
        interactive: bool,
        now: DateTime<Utc>,
    ) -> Result<Reservation, StorageError> {
        // Own every input before the first await. This future is awaited from a `Send`-boxed
        // async-trait boundary (the worker's quota), and references captured across a retry
        // loop whose action is generic over the transaction's lifetime cannot prove `Send`
        // there — the same lesson the review-UI lane took: owned data across awaits, reference
        // params by value inside the loop's body.
        let db = self.db.inner().clone();
        let limits = self.limits;
        let policy = expected_policy.clone();
        let account = account.clone();
        let pass = pass.clone();
        for _ in 0..GUARD_CONFLICT_ATTEMPTS {
            // `Surreal::begin` consumes its receiver, so each attempt takes a fresh clone of
            // the shared handle — the same per-call clone the parent repo's helpers use.
            let tx = db.clone().begin().await.map_err(internal)?;
            let error = match reserve_transaction(
                &tx,
                limits,
                policy.clone(),
                account.clone(),
                pass.clone(),
                interactive,
                now,
            )
            .await
            {
                Ok(reservation) => match tx.commit().await {
                    Ok(_) => return Ok(reservation),
                    Err(error) => internal(error),
                },
                Err(error) => {
                    tx.cancel().await.map_err(internal)?;
                    error
                }
            };
            if !can_retry(&error) {
                return Err(error);
            }
            tokio::task::yield_now().await;
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    /// Usage as of `now`: the origin pass counter plus the rolling account, interactive and
    /// global windows. Charges that expired are not reported.
    pub async fn usage(
        &self,
        account: &AccountId,
        pass: &PassId,
        now: DateTime<Utc>,
    ) -> Result<Usage, StorageError> {
        let pass_rows: Vec<BudgetCountRow> = self
            .db
            .inner()
            .query("SELECT * FROM $id")
            .bind(("id", pass_key(pass)))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let pass_calls = pass_rows
            .into_iter()
            .next()
            .map_or(0, |row| row.attempted_calls);
        let account_total =
            rolling_plain(self.db.inner(), "account", Some(account), false, now).await?;
        let interactive_total =
            rolling_plain(self.db.inner(), "account", Some(account), true, now).await?;
        let global_total = rolling_plain(self.db.inner(), "global", None, false, now).await?;
        Ok(Usage {
            pass: usize::try_from(pass_calls).unwrap_or(usize::MAX),
            account: usize::try_from(account_total).unwrap_or(usize::MAX),
            interactive: usize::try_from(interactive_total).unwrap_or(usize::MAX),
            global: usize::try_from(global_total).unwrap_or(usize::MAX),
        })
    }
}

/// One reservation attempt inside an open transaction. A plain `async fn`, not a generic
/// `AsyncFn` action: the retry loop's caller needs a `Send` future, and reference parameters
/// crossing a higher-ranked closure defeat that proof. Small inputs arrive by value.
async fn reserve_transaction(
    tx: &Transaction<Any>,
    limits: CallLimits,
    policy: InferencePolicyId,
    account: AccountId,
    pass: PassId,
    interactive: bool,
    now: DateTime<Utc>,
) -> Result<Reservation, StorageError> {
    // Authority is checked inside the same transaction as the charge: a suspended account, a
    // revoked or mismatched consent, or a superseded policy refuses without spending anything.
    require_active(tx, &account, None).await?;
    let effective = PolicySnapshot::read(tx, &account).await?.effective()?;
    let authority = bind_authority(&account, &effective).ok_or(StorageError::Stale)?;
    if authority.inference_policy_id != policy {
        return Err(StorageError::Stale);
    }
    let pass_calls = select_record::<BudgetCountRow>(tx, pass_key(&pass))
        .await?
        .map_or(0, |row| row.attempted_calls);
    if pass_calls >= limits.pass as u64 {
        return Ok(Reservation::Denied(BudgetDenial::Pass));
    }
    if rolling(tx, "account", Some(&account), false, now).await? >= limits.account as u64 {
        return Ok(Reservation::Denied(BudgetDenial::Account));
    }
    if interactive
        && rolling(tx, "account", Some(&account), true, now).await? >= limits.interactive as u64
    {
        return Ok(Reservation::Denied(BudgetDenial::Interactive));
    }
    if rolling(tx, "global", None, false, now).await? >= limits.global as u64 {
        return Ok(Reservation::Denied(BudgetDenial::Global));
    }
    charge_pass(tx, &pass, now).await?;
    if interactive {
        charge(
            tx,
            account_key("interactive", &account, now),
            "account",
            Some(account.as_record_id().clone()),
            1,
            now,
        )
        .await?;
    } else {
        charge(
            tx,
            account_key("account", &account, now),
            "account",
            Some(account.as_record_id().clone()),
            0,
            now,
        )
        .await?;
    }
    charge(tx, global_key(now), "global", None, 0, now).await?;
    Ok(Reservation::Granted)
}

/// Admission at a selection, reservation or egress edge. Suspended accounts, revoked or absent
/// consent, mismatched destinations, and a superseded configured policy all refuse.
pub async fn admission(
    db: &Database,
    account: &AccountId,
    expected_policy: &InferencePolicyId,
) -> Result<(), StorageError> {
    let tx = db.inner().clone().begin().await.map_err(internal)?;
    let result = async {
        require_active(&tx, account, None).await?;
        let effective = PolicySnapshot::read(&tx, account).await?.effective()?;
        let authority = bind_authority(account, &effective).ok_or(StorageError::Stale)?;
        if authority.inference_policy_id != *expected_policy {
            return Err(StorageError::Stale);
        }
        Ok(())
    }
    .await;
    tx.cancel().await.map_err(internal)?;
    result
}

/// The authority an account's current policy, consent and approval mode bind to right now.
pub async fn current_authority(
    db: &Database,
    account: &AccountId,
) -> Result<Option<Authority>, StorageError> {
    let tx = db.inner().clone().begin().await.map_err(internal)?;
    let result = async {
        let effective = PolicySnapshot::read(&tx, account).await?.effective()?;
        Ok(bind_authority(account, &effective))
    }
    .await;
    tx.cancel().await.map_err(internal)?;
    result
}

/// The storage-side binder for the engine's `current_authority`: consent must be this
/// account's, unrevoked, and for exactly the configured destinations. Mode is clamped by the
/// configured ceiling, exactly as the engine clamps it.
fn bind_authority(account: &AccountId, policy: &EffectivePolicy) -> Option<Authority> {
    let configured = policy.configured.as_ref()?;
    let consent = policy.consent.as_ref()?;
    if &consent.account != account
        || consent.revoked_at.is_some()
        || consent.destination_set.is_empty()
        || consent.destination_set != configured.configuration.destination_set
    {
        return None;
    }
    Some(Authority {
        account: account.clone(),
        consent_revision: consent.revision,
        owner_mode_revision: policy.approval.revision,
        inference_policy_id: configured.configuration.inference_policy_id.clone(),
        configured_revision: configured.revision,
        mode: if configured.configuration.allow_automatic {
            policy.approval.mode
        } else {
            ApprovalMode::Human
        },
    })
}

fn attempt_state(state: &str) -> Result<AttemptState, StorageError> {
    match state {
        "queued" => Ok(AttemptState::Queued),
        "running" => Ok(AttemptState::Running),
        "completed" => Ok(AttemptState::Completed),
        "cancelled" => Ok(AttemptState::Cancelled),
        "failed" => Ok(AttemptState::Failed),
        _ => Err(StorageError::Internal(anyhow::anyhow!(
            "refinement attempt carries an unknown state"
        ))),
    }
}

fn missing_row_id() -> StorageError {
    StorageError::Internal(anyhow::anyhow!(
        "refinement attempt row is missing its record id"
    ))
}

fn queued_work(row: AttemptRow) -> Result<QueuedWork, StorageError> {
    let kind = WorkKind::from_row(&row.kind)?;
    let scope = decode_scope_name(&row.scope, row.project.as_ref())?;
    Ok(QueuedWork {
        id: AttemptId(row.id.ok_or_else(missing_row_id)?),
        account: AccountId::try_from(row.account)?,
        partition: Partition {
            workspace: WorkspaceId::try_from(row.workspace)?,
            scope,
        },
        kind,
        selection: match row.proposal.clone() {
            Some(proposal) => WorkSelection::Proposal(proposal),
            None => row.selection.into_selection()?,
        },
        authority: Authority {
            account: AccountId::try_from(row.authority.account.clone())?,
            consent_revision: row.authority.consent_revision,
            owner_mode_revision: row.authority.owner_mode_revision,
            inference_policy_id: InferencePolicyId(row.authority.inference_policy_id.clone()),
            configured_revision: row.authority.configured_revision,
            mode: row.authority.mode,
        },
        interactive: kind.is_interactive(),
    })
}

fn decode_scope_name(scope: &str, project: Option<&RecordId>) -> Result<MemoryScope, StorageError> {
    match scope {
        "personal" => Ok(MemoryScope::Personal),
        "unassigned" => Ok(MemoryScope::Unassigned),
        "project" => {
            let project = project
                .cloned()
                .ok_or(StorageError::Internal(anyhow::anyhow!(
                    "project-scoped refinement row is missing its project"
                )))?;
            Ok(MemoryScope::Project(ProjectId::try_from(project)?))
        }
        _ => Err(StorageError::Internal(anyhow::anyhow!(
            "refinement row carries an unknown scope"
        ))),
    }
}

#[derive(Clone, SurrealValue)]
struct AttemptRecord {
    account: RecordId,
    workspace: RecordId,
    project: Option<RecordId>,
    scope: String,
    kind: String,
    idempotency_key: String,
    selection: SelectionRecord,
    source_revision: Option<u64>,
    proposal: Option<RecordId>,
    state: String,
    authority: AuthorityRecord,
}

impl From<&Authority> for AuthorityRecord {
    fn from(authority: &Authority) -> Self {
        AuthorityRecord {
            account: authority.account.as_record_id().clone(),
            consent_revision: authority.consent_revision,
            owner_mode_revision: authority.owner_mode_revision,
            inference_policy_id: authority.inference_policy_id.0.clone(),
            configured_revision: authority.configured_revision,
            mode: authority.mode,
        }
    }
}

#[derive(Clone, SurrealValue)]
struct SelectionRecord {
    kind: String,
    focal: Option<RecordId>,
    related: Vec<RecordId>,
}

impl From<&WorkSelection> for SelectionRecord {
    fn from(selection: &WorkSelection) -> Self {
        match selection {
            WorkSelection::Scheduled => SelectionRecord {
                kind: "scheduled".into(),
                focal: None,
                related: Vec::new(),
            },
            WorkSelection::Evidence(evidence) => SelectionRecord {
                kind: "evidence".into(),
                focal: Some(evidence.focal.as_record_id().clone()),
                related: evidence
                    .related
                    .iter()
                    .map(|id| id.as_record_id().clone())
                    .collect(),
            },
            WorkSelection::Proposal(_) => SelectionRecord {
                kind: "scheduled".into(),
                focal: None,
                related: Vec::new(),
            },
        }
    }
}

impl SelectionRecord {
    fn into_selection(self) -> Result<WorkSelection, StorageError> {
        match self.kind.as_str() {
            "scheduled" => Ok(WorkSelection::Scheduled),
            "evidence" => {
                let focal = self.focal.ok_or(StorageError::Internal(anyhow::anyhow!(
                    "evidence selection is missing its focal"
                )))?;
                let mut related = Vec::with_capacity(self.related.len());
                for id in self.related {
                    related.push(FactId::try_from(id)?);
                }
                Ok(WorkSelection::Evidence(EvidenceSelection {
                    focal: FactId::try_from(focal)?,
                    related,
                }))
            }
            _ => Err(StorageError::Internal(anyhow::anyhow!(
                "refinement attempt carries an unknown selection"
            ))),
        }
    }
}

#[derive(Clone, SurrealValue)]
struct AttemptRow {
    id: Option<RecordId>,
    account: RecordId,
    workspace: RecordId,
    project: Option<RecordId>,
    scope: String,
    kind: String,
    idempotency_key: String,
    selection: SelectionRecord,
    source_revision: Option<u64>,
    proposal: Option<RecordId>,
    state: String,
    authority: AuthorityRecord,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Clone, SurrealValue)]
struct ProposalFullRow {
    id: Option<RecordId>,
    workspace: RecordId,
    project: Option<RecordId>,
    scope: String,
    revision: u64,
    state: ProposalState,
    needs_human: bool,
    deferred: bool,
    evidence: RecordId,
    origin_policy: AuthorityRecord,
    proposal_payload: String,
}

#[derive(Clone, SurrealValue)]
struct BudgetCountRow {
    attempted_calls: u64,
    window_end: DateTime<Utc>,
    request_bytes: Option<u64>,
}

#[derive(Debug, serde::Deserialize, SurrealValue)]
struct StampRow {
    value: String,
}

fn pass_key(pass: &PassId) -> RecordId {
    RecordId::new("refinement_budget", format!("pass:{}", pass.0))
}

fn account_key(prefix: &str, account: &AccountId, now: DateTime<Utc>) -> RecordId {
    RecordId::new(
        "refinement_budget",
        format!(
            "{}:{}:{}",
            prefix,
            account.as_record_id().to_sql(),
            now.timestamp()
        ),
    )
}

fn global_key(now: DateTime<Utc>) -> RecordId {
    RecordId::new("refinement_budget", format!("global:{}", now.timestamp()))
}

/// Charge the origin pass's own counter. Pass rows never expire: a pass is bounded by its
/// work, not by a window, and the rolling queries exclude them by bucket.
async fn charge_pass(
    tx: &Transaction<Any>,
    pass: &PassId,
    now: DateTime<Utc>,
) -> Result<(), StorageError> {
    let id = pass_key(pass);
    let existing = select_record::<BudgetCountRow>(tx, id.clone()).await?;
    let rows: Vec<Value> = match existing {
        Some(_) => tx
            .query("UPDATE $id SET attempted_calls = attempted_calls + 1 RETURN AFTER")
            .bind(("id", id))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?,
        None => tx
            .query(
                "CREATE $id SET bucket = 'pass', account = NONE, window_start = $now, \
                 window_end = $now, attempted_calls = 1, request_bytes = 0 RETURN AFTER",
            )
            .bind(("id", id))
            .bind(("now", now))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?,
    };
    if rows.len() != 1 {
        return Err(StorageError::Internal(anyhow::anyhow!(
            "budget charge did not land"
        )));
    }
    Ok(())
}

/// One charge against a rolling bucket. Rows are anchored at the charging second, so the
/// 24-hour window each charge opens is exact and per-charge: a charge counts toward a usage
/// read for precisely 24 hours. `marker` distinguishes the interactive sub-counter rows
/// (`request_bytes = 1`) from the total rows; the column is otherwise unused by this repo.
async fn charge(
    tx: &Transaction<Any>,
    id: RecordId,
    bucket: &str,
    account: Option<RecordId>,
    marker: u64,
    now: DateTime<Utc>,
) -> Result<(), StorageError> {
    let end = now + TimeDelta::seconds(ROLLING_WINDOW_SECS);
    let existing = select_record::<BudgetCountRow>(tx, id.clone()).await?;
    let rows: Vec<Value> = match existing {
        Some(row) if row.window_end > now => tx
            .query("UPDATE $id SET attempted_calls = attempted_calls + 1 RETURN AFTER")
            .bind(("id", id))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?,
        Some(_) => tx
            .query(
                "UPDATE $id SET window_start = $now, window_end = $end, attempted_calls = 1, \
                 request_bytes = $marker RETURN AFTER",
            )
            .bind(("id", id))
            .bind(("now", now))
            .bind(("end", end))
            .bind(("marker", marker))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?,
        None => tx
            .query(
                "CREATE $id SET bucket = $bucket, account = $account, window_start = $now, \
                 window_end = $end, attempted_calls = 1, request_bytes = $marker RETURN AFTER",
            )
            .bind(("id", id))
            .bind(("bucket", bucket))
            .bind(("account", account))
            .bind(("now", now))
            .bind(("end", end))
            .bind(("marker", marker))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?,
    };
    if rows.len() != 1 {
        return Err(StorageError::Internal(anyhow::anyhow!(
            "budget charge did not land"
        )));
    }
    Ok(())
}

/// Sum the still-active charges for one bucket. `interactive_only` counts only the rows the
/// interactive marker owns.
async fn rolling(
    tx: &Transaction<Any>,
    bucket: &str,
    account: Option<&AccountId>,
    interactive_only: bool,
    now: DateTime<Utc>,
) -> Result<u64, StorageError> {
    let mut sql = "SELECT attempted_calls, window_end, request_bytes FROM refinement_budget \
         WHERE bucket = $bucket AND window_end > $now"
        .to_string();
    if account.is_some() {
        sql.push_str(" AND account = $account");
    }
    if interactive_only {
        sql.push_str(" AND request_bytes = 1");
    }
    let mut query = tx.query(sql).bind(("bucket", bucket)).bind(("now", now));
    if let Some(account) = account {
        query = query.bind(("account", account.as_record_id().clone()));
    }
    let rows: Vec<BudgetCountRow> = query
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    Ok(rows.into_iter().map(|row| row.attempted_calls).sum())
}

async fn rolling_plain(
    db: &Surreal<Any>,
    bucket: &str,
    account: Option<&AccountId>,
    interactive_only: bool,
    now: DateTime<Utc>,
) -> Result<u64, StorageError> {
    let mut sql = "SELECT attempted_calls, window_end, request_bytes FROM refinement_budget \
         WHERE bucket = $bucket AND window_end > $now"
        .to_string();
    if account.is_some() {
        sql.push_str(" AND account = $account");
    }
    if interactive_only {
        sql.push_str(" AND request_bytes = 1");
    }
    let mut query = db.query(sql).bind(("bucket", bucket)).bind(("now", now));
    if let Some(account) = account {
        query = query.bind(("account", account.as_record_id().clone()));
    }
    let rows: Vec<BudgetCountRow> = query
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    Ok(rows.into_iter().map(|row| row.attempted_calls).sum())
}

async fn read_stamp(db: &Surreal<Any>, key: &str) -> Result<Option<u64>, StorageError> {
    let rows: Vec<StampRow> = db
        .query("SELECT * FROM system_config WHERE key = $key LIMIT 1")
        .bind(("key", key))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.value.parse().ok()))
}

async fn read_stamp_tx(tx: &Transaction<Any>, key: &str) -> Result<Option<u64>, StorageError> {
    let rows: Vec<StampRow> = tx
        .query("SELECT * FROM system_config WHERE key = $key LIMIT 1")
        .bind(("key", key))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.value.parse().ok()))
}

async fn write_stamp_tx(tx: &Transaction<Any>, key: &str, value: u64) -> Result<(), StorageError> {
    let rows: Vec<Value> = tx
        .query("UPDATE system_config SET value = $value, updated_at = time::now() WHERE key = $key RETURN AFTER")
        .bind(("value", value.to_string()))
        .bind(("key", key))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    if rows.is_empty() {
        tx.query(
            "CREATE system_config SET key = $key, value = $value, updated_at = time::now() RETURN AFTER",
        )
        .bind(("key", key))
        .bind(("value", value.to_string()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    }
    Ok(())
}

// --- Payload decoding: the inverse of the canonical JSON the persist path writes. ---

fn decode_evidence(row: &EvidenceRecord) -> Result<EvidenceBundle, StorageError> {
    let value: serde_json::Value = serde_json::from_str(&row.payload)
        .map_err(|error| StorageError::Internal(anyhow::Error::new(error)))?;
    let object = value.as_object().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("evidence payload is not an object"))
    })?;
    let focal = decode_candidate(value_field(object, "focal")?)?;
    let related = object
        .get("related")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .map(decode_candidate)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let coverage = decode_coverage(object.get("coverage"))?;
    Ok(EvidenceBundle {
        focal,
        related,
        coverage,
        inference_policy: InferencePolicyId(row.inference_policy_id.clone()),
    })
}

fn decode_coverage(value: Option<&serde_json::Value>) -> Result<EvidenceCoverage, StorageError> {
    let object = value.and_then(|value| value.as_object()).ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("evidence payload lacks coverage"))
    })?;
    let selection = match str_field(object, "selection")? {
        "Scheduled" => EvidenceSelectionKind::Scheduled,
        "OwnerSelected" => EvidenceSelectionKind::OwnerSelected,
        _ => {
            return Err(StorageError::Internal(anyhow::anyhow!(
                "evidence payload carries an unknown selection kind"
            )));
        }
    };
    let omitted = object
        .get("omitted")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    let entry = value.as_object().ok_or_else(|| {
                        StorageError::Internal(anyhow::anyhow!("omitted entry is not an object"))
                    })?;
                    let reason = match str_field(entry, "reason")? {
                        "OutsideSelection" => OmissionReason::OutsideSelection,
                        "NeighborLimit" => OmissionReason::NeighborLimit,
                        "ByteLimit" => OmissionReason::ByteLimit,
                        "Ineligible" => OmissionReason::Ineligible,
                        _ => {
                            return Err(StorageError::Internal(anyhow::anyhow!(
                                "omitted entry carries an unknown reason"
                            )));
                        }
                    };
                    Ok(OmittedNeighbor {
                        id: FactId::try_from(record_value(
                            entry.get("id").unwrap_or(&serde_json::Value::Null),
                        )?)?,
                        reason,
                    })
                })
                .collect::<Result<Vec<_>, StorageError>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(EvidenceCoverage {
        focal_text_complete: object
            .get("focal_text_complete")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        omitted_neighbors: omitted,
        selection,
    })
}

fn decode_candidate(value: &serde_json::Value) -> Result<Candidate, StorageError> {
    let object = value.as_object().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("candidate payload is not an object"))
    })?;
    let partition = object
        .get("partition")
        .and_then(|value| value.as_object())
        .ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!("candidate payload lacks a partition"))
        })?;
    let workspace = WorkspaceId::try_from(record_value(
        partition
            .get("workspace")
            .unwrap_or(&serde_json::Value::Null),
    )?)?;
    let scope = decode_scope(partition.get("scope"))?;
    let provenance: Vec<Provenance> =
        serde_json::from_value(object.get("provenance").cloned().unwrap_or_default())
            .map_err(|error| StorageError::Internal(anyhow::Error::new(error)))?;
    Ok(Candidate {
        id: FactId::try_from(record_value(
            object.get("id").unwrap_or(&serde_json::Value::Null),
        )?)?,
        partition: Partition { workspace, scope },
        version: Version::new(
            object
                .get("version")
                .and_then(|value| value.as_u64())
                .unwrap_or_default(),
        ),
        complete_content: str_field(object, "complete_content")?.to_owned(),
        tags: serde_json::from_value(object.get("tags").cloned().unwrap_or_default())
            .map_err(|error| StorageError::Internal(anyhow::Error::new(error)))?,
        provenance,
        semantic_hash: str_field(object, "semantic_hash")?.to_owned(),
    })
}

fn decode_scope(value: Option<&serde_json::Value>) -> Result<MemoryScope, StorageError> {
    let object = value
        .and_then(|value| value.as_object())
        .ok_or_else(|| StorageError::Internal(anyhow::anyhow!("scope payload is missing")))?;
    match str_field(object, "kind")? {
        "personal" => Ok(MemoryScope::Personal),
        "unassigned" => Ok(MemoryScope::Unassigned),
        "project" => Ok(MemoryScope::Project(ProjectId::try_from(record_value(
            object.get("project").unwrap_or(&serde_json::Value::Null),
        )?)?)),
        _ => Err(StorageError::Internal(anyhow::anyhow!(
            "scope payload carries an unknown kind"
        ))),
    }
}

fn decode_proposal(
    row: ProposalFullRow,
    payload: serde_json::Value,
    evidence: EvidenceBundle,
) -> Result<ProposalRevision, StorageError> {
    let object = payload.as_object().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("proposal payload is not an object"))
    })?;
    let category = match str_field(object, "category")? {
        "rewrite" => ProposalCategory::Rewrite,
        "consolidate" => ProposalCategory::Consolidate,
        "contradiction" => ProposalCategory::Contradiction,
        "noise" => ProposalCategory::Noise,
        "supersede" => ProposalCategory::Supersede,
        "split" => ProposalCategory::Split,
        _ => {
            return Err(StorageError::Internal(anyhow::anyhow!(
                "proposal payload carries an unknown category"
            )));
        }
    };
    let sources = object
        .get("sources")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .map(decode_expected_state)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let effects = object
        .get("relationship_effects")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .map(decode_effect)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let scope = decode_scope_name(&row.scope, row.project.as_ref())?;
    Ok(ProposalRevision {
        id: row.id.ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!("proposal row is missing its record id"))
        })?,
        revision: Version::new(row.revision),
        partition: Partition {
            workspace: WorkspaceId::try_from(row.workspace)?,
            scope,
        },
        sources,
        category,
        proposed: decode_operation(object.get("operation"))?,
        approved_relationship_effects: effects,
        rationale: str_field(object, "rationale")?.to_owned(),
        uncertainty: str_field(object, "uncertainty")?.to_owned(),
        origin_policy: Authority {
            account: AccountId::try_from(row.origin_policy.account.clone())?,
            consent_revision: row.origin_policy.consent_revision,
            owner_mode_revision: row.origin_policy.owner_mode_revision,
            inference_policy_id: InferencePolicyId(row.origin_policy.inference_policy_id.clone()),
            configured_revision: row.origin_policy.configured_revision,
            mode: row.origin_policy.mode,
        },
        state: row.state,
        needs_human: row.needs_human,
        deferred: row.deferred,
        evidence,
    })
}

fn decode_operation(value: Option<&serde_json::Value>) -> Result<QualityOperation, StorageError> {
    let object = value
        .and_then(|value| value.as_object())
        .ok_or_else(|| StorageError::Internal(anyhow::anyhow!("operation payload is missing")))?;
    match str_field(object, "kind")? {
        "rewrite" => Ok(QualityOperation::Rewrite {
            source: fact_value(object.get("source"))?,
            content: str_field(object, "content")?.to_owned(),
        }),
        "consolidate" => Ok(QualityOperation::Consolidate {
            canonical: fact_value(object.get("canonical"))?,
            sources: fact_values(object.get("sources"))?,
        }),
        "contradiction" => Ok(QualityOperation::Contradiction {
            sources: fact_values(object.get("sources"))?,
        }),
        "noise" => Ok(QualityOperation::Noise {
            source: fact_value(object.get("source"))?,
        }),
        "supersede" => Ok(QualityOperation::Supersede {
            source: fact_value(object.get("source"))?,
            replacement: fact_value(object.get("replacement"))?,
        }),
        "split" => Ok(QualityOperation::Split {
            source: fact_value(object.get("source"))?,
            parts: serde_json::from_value(object.get("parts").cloned().unwrap_or_default())
                .map_err(|error| StorageError::Internal(anyhow::Error::new(error)))?,
        }),
        "no_safe_change" => Ok(QualityOperation::NoSafeChange),
        _ => Err(StorageError::Internal(anyhow::anyhow!(
            "operation payload carries an unknown kind"
        ))),
    }
}

fn decode_effect(value: &serde_json::Value) -> Result<ApprovedRelationshipEffect, StorageError> {
    let object = value.as_object().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("effect payload is not an object"))
    })?;
    match str_field(object, "kind")? {
        "link_source_to_source" => Ok(ApprovedRelationshipEffect::LinkSourceToSource {
            source: fact_value(object.get("source"))?,
            target: fact_value(object.get("target"))?,
        }),
        "remove_source_membership" => Ok(ApprovedRelationshipEffect::RemoveSourceMembership {
            source: fact_value(object.get("source"))?,
            cluster: record_value(object.get("cluster").unwrap_or(&serde_json::Value::Null))?,
        }),
        "add_source_membership" => Ok(ApprovedRelationshipEffect::AddSourceMembership {
            source: fact_value(object.get("source"))?,
            cluster: record_value(object.get("cluster").unwrap_or(&serde_json::Value::Null))?,
        }),
        "generated_endpoint" => Ok(ApprovedRelationshipEffect::GeneratedEndpoint {
            operation: OperationId::new(str_field(object, "operation")?),
            slot: u16::try_from(
                object
                    .get("slot")
                    .and_then(|value| value.as_u64())
                    .ok_or_else(|| {
                        StorageError::Internal(anyhow::anyhow!("effect slot is missing"))
                    })?,
            )
            .map_err(|_| StorageError::Internal(anyhow::anyhow!("effect slot is out of range")))?,
        }),
        _ => Err(StorageError::Internal(anyhow::anyhow!(
            "effect payload carries an unknown kind"
        ))),
    }
}

fn decode_expected_state(value: &serde_json::Value) -> Result<ExpectedState, StorageError> {
    let object = value.as_object().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("source payload is not an object"))
    })?;
    let graph = object
        .get("graph")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .map(decode_relation)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(ExpectedState {
        fact: fact_value(object.get("fact"))?,
        workspace: WorkspaceId::try_from(record_value(
            object.get("workspace").unwrap_or(&serde_json::Value::Null),
        )?)?,
        scope: decode_scope(object.get("scope"))?,
        version: Version::new(
            object
                .get("version")
                .and_then(|value| value.as_u64())
                .unwrap_or_default(),
        ),
        semantic_hash: str_field(object, "semantic_hash")?.to_owned(),
        graph,
    })
}

fn decode_relation(value: &serde_json::Value) -> Result<RelationExpectation, StorageError> {
    let object = value.as_object().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("relation payload is not an object"))
    })?;
    let kind = decode_edge_kind(str_field(object, "kind")?)?;
    let from = record_value(object.get("from").unwrap_or(&serde_json::Value::Null))?;
    let to = record_value(object.get("to").unwrap_or(&serde_json::Value::Null))?;
    let present = object
        .get("present")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if present {
        let edge = object
            .get("edge")
            .and_then(|value| value.as_str())
            .map(parse_record_id)
            .transpose()?;
        Ok(RelationExpectation::Present {
            kind,
            from,
            to,
            edge,
        })
    } else {
        Ok(RelationExpectation::Absent { kind, from, to })
    }
}

fn decode_edge_kind(effect: &str) -> Result<EdgeKind, StorageError> {
    match effect {
        "contains_memory" => Ok(EdgeKind::ClusterMembership),
        "contains_session_memory" => Ok(EdgeKind::SessionMembership),
        "has_provenance" => Ok(EdgeKind::Provenance),
        "extracted_from" => Ok(EdgeKind::ExtractedFrom),
        other => {
            let kind = other.strip_prefix("memory_edge:").ok_or_else(|| {
                StorageError::Internal(anyhow::anyhow!("relation payload carries an unknown kind"))
            })?;
            Ok(EdgeKind::Memory(kind.to_owned()))
        }
    }
}

fn record_value(value: &serde_json::Value) -> Result<RecordId, StorageError> {
    let text = value.as_str().ok_or_else(|| {
        StorageError::Internal(anyhow::anyhow!("payload record id is not a string"))
    })?;
    parse_record_id(text)
}

fn fact_value(value: Option<&serde_json::Value>) -> Result<FactId, StorageError> {
    FactId::try_from(record_value(value.unwrap_or(&serde_json::Value::Null))?)
}

fn fact_values(value: Option<&serde_json::Value>) -> Result<Vec<FactId>, StorageError> {
    Ok(value
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .map(|value| fact_value(Some(value)))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default())
}

fn value_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a serde_json::Value, StorageError> {
    object
        .get(field)
        .ok_or_else(|| StorageError::Internal(anyhow::anyhow!("payload field {field} is missing")))
}

fn str_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a str, StorageError> {
    object
        .get(field)
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!(
                "payload field {field} is missing or not a string"
            ))
        })
}
