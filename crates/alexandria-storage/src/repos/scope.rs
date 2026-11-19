//! The one spelling of "which partitions may this statement read".
//!
//! A scoped read has two ways to go wrong, and both are silent. Filtering in Rust after `LIMIT` makes
//! the page a *global* page trimmed afterwards, so one owner's results depend on how much data other
//! accounts happen to hold. Handing each reader its own text of the predicate makes the ninth one
//! forget `quarantined_at = NONE` or write `IS NOT NULL` — which on this engine is *satisfied by* `NONE`
//! and therefore filters nothing at all.
//!
//! So the conjunct and its binds are built here, once, and every scoped caller splices the same text and
//! binds the same names. Forgetting to put it in a `WHERE` is then visible in that caller's own source
//! rather than being a difference in SQL nobody notices until a leak is reported.

use crate::access::{OwnerContext, ScopedContext, WriteAuthorization};
use crate::error::StorageError;
use crate::models::MemoryScope;
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::method::{Query, Transaction};
use surrealdb::types::{QueryError, RecordId, SurrealValue};

/// The partitions a [`ScopedContext`] may read, as a `WHERE` conjunct plus its binds.
///
/// The record ids it carries are resolved handles, so `Debug` is written by hand below rather than
/// derived: a derived one would put the workspace and every project id of this caller into a log line,
/// which is the class of value this crate keeps out of its renders. The shape is what a diagnostic needs.
///
/// `Clone` because a caller hands one to a query struct (see [`crate::repos::SessionPageQuery`]) and the
/// conjunct has to survive as many statements as the page needs; the handles are the caller's own.
#[derive(Clone)]
pub struct ScopeFilter {
    workspace: RecordId,
    personal: bool,
    unassigned: bool,
    projects: Vec<RecordId>,
}

impl std::fmt::Debug for ScopeFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeFilter")
            .field("personal", &self.personal)
            .field("unassigned", &self.unassigned)
            .field("projects", &self.projects.len())
            .finish_non_exhaustive()
    }
}

impl ScopeFilter {
    /// Projects the context's partitions into a predicate.
    ///
    /// An empty context selects nothing rather than everything: a caller that resolved no partitions
    /// must not silently become a table scan.
    pub fn new(context: &ScopedContext) -> Self {
        let mut filter = Self {
            workspace: context.workspace().as_record_id().clone(),
            personal: false,
            unassigned: false,
            projects: Vec::new(),
        };
        for partition in context.partitions() {
            match &partition.scope {
                MemoryScope::Personal => filter.personal = true,
                MemoryScope::Unassigned => filter.unassigned = true,
                MemoryScope::Project(project) => {
                    filter.projects.push(project.as_record_id().clone())
                }
            }
        }
        filter
    }

    /// The resolved workspace, for statements that key on workspace alone.
    pub fn workspace(&self) -> RecordId {
        self.workspace.clone()
    }

    pub fn is_empty(&self) -> bool {
        !self.personal && !self.unassigned && self.projects.is_empty()
    }

    /// Whether this predicate names **exactly one** partition, rather than a union.
    ///
    /// A destination candidate population must be one partition, because the claim a destination makes is
    /// "this cluster is mine", singular: a filter that granted Personal *and* a project would call a
    /// cluster straddling those two partitions pure, which is the mixed membership the one-scope rule
    /// forbids, laundered by a union instead of by a workspace. The writers that rank a destination answer
    /// this before ranking, and refuse rather than discover the question later.
    pub fn is_single_partition(&self) -> bool {
        self.personal as usize + self.unassigned as usize + self.projects.len() == 1
    }

    /// The conjunct. Binds [`Self::apply`] supplies: `$scope_workspace`, `$scope_personal`,
    /// `$scope_unassigned`, `$scope_projects`.
    ///
    /// `scope = 'project' AND project INSIDE $scope_projects` is what makes a project partition mean a
    /// *specific* project. Testing only `scope = 'project'` would read every project row in the
    /// workspace — including projects the caller never selected — which is the leak with the owner's own
    /// workspace stamp still in place. An empty `$scope_projects` matches nothing, so a project list that
    /// came in empty cannot widen the read.
    pub fn conjunct(&self) -> &'static str {
        "workspace = $scope_workspace \
         AND ( \
             (scope = 'personal' AND $scope_personal) \
             OR (scope = 'unassigned' AND $scope_unassigned) \
             OR (scope = 'project' AND project INSIDE $scope_projects) \
         )"
    }

    /// Binds the names [`Self::conjunct`] uses.
    ///
    /// `Query` is the same builder whether it came from a `Surreal` handle or from a `Transaction`, so
    /// one method serves both plain reads and guarded writes.
    pub fn apply<'q>(&self, q: Query<'q, Any>) -> Query<'q, Any> {
        q.bind(("scope_workspace", self.workspace.clone()))
            .bind(("scope_personal", self.personal))
            .bind(("scope_unassigned", self.unassigned))
            .bind(("scope_projects", self.projects.clone()))
    }

    /// The single partition a focal record lives in, for the activation flow — which must warm one
    /// partition per focal row rather than the request's whole union.
    pub fn for_partition(partition: &crate::models::Partition) -> Self {
        let mut filter = Self {
            workspace: partition.workspace.as_record_id().clone(),
            personal: false,
            unassigned: false,
            projects: Vec::new(),
        };
        match &partition.scope {
            MemoryScope::Personal => filter.personal = true,
            MemoryScope::Unassigned => filter.unassigned = true,
            MemoryScope::Project(project) => filter.projects.push(project.as_record_id().clone()),
        }
        filter
    }

    /// A filter that names **only** a workspace, for the mutation engine's own reads and writes.
    ///
    /// A journaled change is aimed at rows by handle under an account and workspace stamp, not at a
    /// resolved read population: the partitions a request happened to select say nothing about which
    /// rows that same request may edit, and `AGENTS.md`'s rule that an owner's write is authorized by
    /// the row's partition rather than by a read's window is what this constructor keeps visible. Every
    /// partition-scoped *read* still goes through [`ScopeFilter::new`].
    ///
    /// Both partition flags are false and the project list is empty on purpose: a statement that names
    /// `$scope_personal` must not find it true merely because a workspace was bound.
    pub fn from_workspace(workspace: &crate::models::identity::WorkspaceId) -> Self {
        Self {
            workspace: workspace.as_record_id().clone(),
            personal: false,
            unassigned: false,
            projects: Vec::new(),
        }
    }
}

/// The number of times a guarded write re-issues its claim before the engine's write conflict is
/// reported as retryable rather than retried here.
///
/// Same bound as Task 4's one-use claims, for the same reason: the engine resolves the race on the
/// first retry in practice, and the bound exists so a pathological store cannot spin inside one call.
pub(crate) const GUARD_CONFLICT_ATTEMPTS: usize = 8;

/// A value bound into a guarded statement, closed over the types the scoped writes actually carry.
///
/// `Clone` because a guard retry re-issues the whole statement, so the binds have to survive as many
/// attempts as the loop is willing to make.
#[derive(Clone)]
pub enum Bind {
    Text(String),
    OptText(Option<String>),
    Float(f64),
    Floats(Vec<f32>),
    TextList(Vec<String>),
    OptInt(Option<i64>),
    Bool(bool),
    Time(DateTime<Utc>),
    OptTime(Option<DateTime<Utc>>),
    OptRecord(Option<RecordId>),
}

impl Bind {
    fn apply<'q>(self, q: Query<'q, Any>, name: &'q str) -> Query<'q, Any> {
        match self {
            Bind::Text(value) => q.bind((name, value)),
            Bind::OptText(value) => q.bind((name, value)),
            Bind::Float(value) => q.bind((name, value)),
            Bind::Floats(value) => q.bind((name, value)),
            Bind::TextList(value) => q.bind((name, value)),
            Bind::OptInt(value) => q.bind((name, value)),
            Bind::Time(value) => q.bind((name, value)),
            Bind::OptTime(value) => q.bind((name, value)),
            Bind::Bool(value) => q.bind((name, value)),
            Bind::OptRecord(value) => q.bind((name, value)),
        }
    }
}

/// Why a statement inside a guarded transaction did not run to completion.
///
/// The two are not one event: the engine's write conflict is the racing authority change this loop exists
/// to retry, and everything else is a real failure that must reach the caller with its own cause intact.
/// Classifying both as "retry" would spin the bound against a syntax error.
enum StatementFailure {
    Conflict,
    Failed(StorageError),
}

impl StatementFailure {
    fn of(error: surrealdb::Error) -> Self {
        if is_write_conflict(&error) {
            StatementFailure::Conflict
        } else {
            StatementFailure::Failed(guard_internal(error))
        }
    }
}

/// One data statement destined for the transaction that re-asserts its writer's authority.
///
/// `sql` may hold several SurrealQL statements separated by `;` when they are one atomic write (a
/// `CREATE` plus the `RELATE` that links it): they share one query, so they share one transaction, and
/// the decoded rows are the *first* statement's. `ids` binds record handles, `values` binds everything
/// else, and `scope` adds the population binds a statement naming [`ScopeFilter::conjunct`] needs.
pub struct GuardedWrite<'a> {
    sql: String,
    ids: Vec<(&'a str, RecordId)>,
    values: Vec<(&'a str, Bind)>,
    scope: Option<&'a ScopeFilter>,
}

impl<'a> GuardedWrite<'a> {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            ids: Vec::new(),
            values: Vec::new(),
            scope: None,
        }
    }

    pub fn id(mut self, name: &'a str, id: RecordId) -> Self {
        self.ids.push((name, id));
        self
    }

    pub fn value(mut self, name: &'a str, bind: Bind) -> Self {
        self.values.push((name, bind));
        self
    }

    pub fn with_scope(mut self, scope: &'a ScopeFilter) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Appends the value binds a statement built conditionally, one `SET` clause at a time.
    ///
    /// The session create is the caller: which columns it names depends on which optional fields the
    /// request carried, so the bind list is assembled alongside the SQL text rather than written out
    /// as a fixed set. Every other writer names its columns statically and uses [`GuardedWrite::value`].
    pub fn with_values(mut self, values: Vec<(&'a str, Bind)>) -> Self {
        self.values.extend(values);
        self
    }
}

/// The single spelling of "one scoped write, guarded inside its own transaction".
///
/// Every scoped writer in this crate runs through here, because the guard is the property and it has to
/// be impossible for a new seam to forget it: the transaction opens, [`claim_authority`] re-asserts the
/// owner's credential *as a conditional write*, the data statement runs, and only then does the
/// transaction commit. A concurrent revoke, suspension or epoch bump therefore costs the commit an engine
/// write conflict and the data write is rolled back with it — which a guarded `SELECT` before the write
/// does not give (`scoped_repos_test::a_guarded_read_alone_is_not_atomic` pins the difference).
///
/// `$account` and `$workspace` are bound from the context, always, so no call site can drop the owner
/// stamp or name an account it does not hold — and no call site can *override* them either: `Bind` has
/// no bare-record variant left, because the one writer that used a `workspace` value of its own to aim
/// a write outside the owner's workspace was activation, and that seam now takes its workspace from
/// here (see [`crate::repos::ScopedHeatWriter::warm`]). A future writer that genuinely needs to reach a
/// row outside its owner's workspace is a new authority question, not a new bind.
///
/// `Ok(None)` means the authority was gone before the statement ran and nothing was written; the caller
/// owes its caller [`StorageError::Stale`], not a success. Exhausting the retry bound is
/// [`StorageError::RetryableBookkeepingConflict`] — a transient race over bookkeeping, distinct from a
/// semantic refusal, per the plan's error contract.
pub(crate) async fn guarded_run<T>(
    db: &Surreal<Any>,
    owner: &OwnerContext,
    write: &GuardedWrite<'_>,
    now: DateTime<Utc>,
) -> Result<Option<Vec<T>>, StorageError>
where
    T: SurrealValue + Send,
{
    let per_statement = guarded_writes(db, owner, std::slice::from_ref(write), now).await?;
    Ok(per_statement.map(|mut rows| rows.remove(0)))
}

/// The multi-statement form of [`guarded_run`]: one claim, one transaction, every statement in order.
///
/// Returns the rows each statement wrote, in the order given, so a caller can tell a warm that happened
/// from a warm that was refused without opening a transaction per target. [`guarded_run`] is this with a
/// one-element list — there is exactly one place the claim, the retry bound and the rollback-on-conflict
/// live.
///
/// An empty slice is a committed success with nothing in it, answered before any transaction opens. That
/// is not a shortcut: [`claim_authority`] *writes* — the account row, and for a hosted caller the
/// credential's `last_used_at` — so a pass with zero statements that still ran the claim would be a
/// durable write recording that nothing was written, and a contention on the account row for nothing.
/// Task 6's review named this as the hand-off; `an_empty_warm_pass_claims_nothing_and_opens_no_transaction`
/// is the witness, counting committed account writes with the engine's own event.
pub(crate) async fn guarded_writes<T>(
    db: &Surreal<Any>,
    owner: &OwnerContext,
    writes: &[GuardedWrite<'_>],
    now: DateTime<Utc>,
) -> Result<Option<Vec<Vec<T>>>, StorageError>
where
    T: SurrealValue + Send,
{
    if writes.is_empty() {
        return Ok(Some(Vec::new()));
    }
    for attempt in 0..GUARD_CONFLICT_ATTEMPTS {
        let tx = db.clone().begin().await.map_err(guard_internal)?;
        if !claim_authority(&tx, owner, now).await? {
            tx.cancel().await.map_err(guard_internal)?;
            return Ok(None);
        }

        let mut per_statement: Vec<Vec<T>> = Vec::with_capacity(writes.len());
        let mut failed: Option<StatementFailure> = None;
        for write in writes {
            let mut query = tx.query(write.sql.clone());
            query = query.bind(("account", owner.account().as_record_id().clone()));
            query = query.bind(("workspace", owner.workspace().as_record_id().clone()));
            for (name, id) in write.ids.clone() {
                query = query.bind((name, id));
            }
            if let Some(scope) = write.scope {
                query = scope.apply(query);
            }
            for (name, bind) in write.values.clone() {
                query = bind.apply(query, name);
            }
            match query.await {
                Ok(response) => match response.check() {
                    Ok(mut checked) => match checked.take::<Vec<T>>(0) {
                        Ok(rows) => per_statement.push(rows),
                        Err(error) => {
                            failed = Some(StatementFailure::of(error));
                            break;
                        }
                    },
                    Err(error) => {
                        failed = Some(StatementFailure::of(error));
                        break;
                    }
                },
                Err(error) => {
                    failed = Some(StatementFailure::of(error));
                    break;
                }
            }
        }

        // A statement that did not run means the pass is incomplete. Cancel the whole transaction — the
        // targets already written inside it are rolled back with it — so a half-warmed neighbourhood can
        // never be what a retried pass sees.
        if let Some(failure) = failed {
            tx.cancel().await.map_err(guard_internal)?;
            match failure {
                StatementFailure::Conflict => {
                    if attempt + 1 < GUARD_CONFLICT_ATTEMPTS {
                        continue;
                    }
                    return Err(StorageError::RetryableBookkeepingConflict);
                }
                StatementFailure::Failed(error) => return Err(error),
            }
        }

        if let Err(error) = tx.commit().await {
            if is_write_conflict(&error) {
                if attempt + 1 < GUARD_CONFLICT_ATTEMPTS {
                    continue;
                }
                return Err(StorageError::RetryableBookkeepingConflict);
            }
            return Err(guard_internal(error));
        }
        return Ok(Some(per_statement));
    }
    Err(StorageError::RetryableBookkeepingConflict)
}

/// The system writer's half of [`guarded_writes`]: same transaction shape, same retry bound, same
/// rollback-on-conflict — only the claim differs.
///
/// The claim is the **actual-write active-account barrier**: the capability is verified against the
/// issuing instance (pointer equality, no write), and then the partition's owner account is
/// re-asserted *as a conditional write* inside the transaction — `status = 'active'` on the account
/// row that owns the workspace — so a suspension landing between the dispatcher's enumeration and
/// the write costs the commit a conflict and rolls the data write back with it. There is no
/// credential half and no epoch half: system maintenance holds no credential, and an epoch bump
/// revokes credentials rather than suspending an account. `Ok(None)` means the account was no
/// longer active and the caller owes [`StorageError::Stale`].
///
/// This is deliberately a sibling of [`guarded_writes`] rather than a flag inside it: the owner
/// claim binds an account *and* a credential from an [`OwnerContext`], and threading a "which kind
/// of claimant" enum through every scoped writer would make the owner path carry a variant it never
/// uses. The statement loop is the part that must not drift, and it is shared below.
/// The single-statement form of [`guarded_system_writes`], mirroring the relationship
/// [`guarded_run`] has to [`guarded_writes`]: one write, one flat row vector. Most maintenance
/// writes are single statements, and the flat shape is what a caller reading `RETURN AFTER` rows
/// from one statement wants without peeling a nested vec.
///
/// `pub` because the dreaming jobs live in the binary crate, not in storage: the system writer's
/// entry points are the maintenance API surface, while everything they delegate to below stays
/// crate-private.
pub async fn guarded_system_run<T>(
    db: &Surreal<Any>,
    instance: &crate::Database,
    system: &crate::access::SystemContext,
    partition: &crate::models::identity::Partition,
    write: &GuardedWrite<'_>,
) -> Result<Option<Vec<T>>, StorageError>
where
    T: SurrealValue + Send,
{
    let per_statement =
        guarded_system_writes(db, instance, system, partition, std::slice::from_ref(write)).await?;
    Ok(per_statement.map(|mut rows| rows.remove(0)))
}

pub async fn guarded_system_writes<T>(
    db: &Surreal<Any>,
    instance: &crate::Database,
    system: &crate::access::SystemContext,
    partition: &crate::models::identity::Partition,
    writes: &[GuardedWrite<'_>],
) -> Result<Option<Vec<Vec<T>>>, StorageError>
where
    T: SurrealValue + Send,
{
    if !instance.holds_authority(system) {
        return Err(StorageError::Stale);
    }
    if writes.is_empty() {
        return Ok(Some(Vec::new()));
    }
    for attempt in 0..GUARD_CONFLICT_ATTEMPTS {
        let tx = db.clone().begin().await.map_err(guard_internal)?;
        if !claim_system_for_workspace(&tx, partition.workspace.as_record_id()).await? {
            tx.cancel().await.map_err(guard_internal)?;
            return Ok(None);
        }
        let run = run_guarded_statements(&tx, writes, partition.workspace.as_record_id()).await;
        match run {
            StatementRun::Done(per_statement) => {
                if let Err(error) = tx.commit().await {
                    if is_write_conflict(&error) {
                        if attempt + 1 < GUARD_CONFLICT_ATTEMPTS {
                            continue;
                        }
                        return Err(StorageError::RetryableBookkeepingConflict);
                    }
                    return Err(guard_internal(error));
                }
                return Ok(Some(per_statement));
            }
            StatementRun::Conflict => {
                tx.cancel().await.map_err(guard_internal)?;
                if attempt + 1 < GUARD_CONFLICT_ATTEMPTS {
                    continue;
                }
                return Err(StorageError::RetryableBookkeepingConflict);
            }
            StatementRun::Failed(error) => {
                tx.cancel().await.map_err(guard_internal)?;
                return Err(error);
            }
        }
    }
    Err(StorageError::RetryableBookkeepingConflict)
}

/// The statement loop both claimants share, parameterised only by the workspace their binds carry.
async fn run_guarded_statements<T>(
    tx: &Transaction<Any>,
    writes: &[GuardedWrite<'_>],
    workspace: &RecordId,
) -> StatementRun<T>
where
    T: SurrealValue + Send,
{
    let mut per_statement: Vec<Vec<T>> = Vec::with_capacity(writes.len());
    for write in writes {
        let mut query = tx.query(write.sql.clone());
        query = query.bind(("workspace", workspace.clone()));
        for (name, id) in write.ids.clone() {
            query = query.bind((name, id));
        }
        if let Some(scope) = write.scope {
            query = scope.apply(query);
        }
        for (name, bind) in write.values.clone() {
            query = bind.apply(query, name);
        }
        match query.await {
            Ok(response) => match response.check() {
                Ok(mut checked) => match checked.take::<Vec<T>>(0) {
                    Ok(rows) => per_statement.push(rows),
                    Err(error) => {
                        return StatementRun::of(error);
                    }
                },
                Err(error) => return StatementRun::of(error),
            },
            Err(error) => return StatementRun::of(error),
        }
    }
    StatementRun::Done(per_statement)
}

/// What one pass over a guarded statement list produced: rows, a retryable conflict, or a failure.
enum StatementRun<T> {
    Done(Vec<Vec<T>>),
    Conflict,
    Failed(StorageError),
}

impl<T> StatementRun<T> {
    fn of(error: surrealdb::Error) -> Self {
        match StatementFailure::of(error) {
            StatementFailure::Conflict => StatementRun::Conflict,
            StatementFailure::Failed(error) => StatementRun::Failed(error),
        }
    }
}

/// The system claim's conditional write: the account row that owns this workspace, re-asserted as
/// active at the moment the data statement is about to run.
pub(crate) async fn claim_system_for_workspace(
    tx: &Transaction<Any>,
    workspace: &RecordId,
) -> Result<bool, StorageError> {
    let account_rows: Vec<serde_json::Value> = tx
        .query(
            "UPDATE account SET updated_at = time::now() WHERE id IN (SELECT VALUE owner FROM \
             workspace WHERE id = $workspace) AND status = 'active' RETURN AFTER;",
        )
        .bind(("workspace", workspace.clone()))
        .await
        .map_err(guard_internal)?
        .check()
        .map_err(guard_internal)?
        .take(0)
        .map_err(guard_internal)?;
    Ok(!account_rows.is_empty())
}

/// Re-asserts an [`OwnerContext`]'s write authorization *as a conditional write on the rows that carry
/// it*, inside the caller's transaction. This is the single spelling of the Task 5 in-transaction
/// commit predicate; every scoped writer in this crate calls it rather than restating the SQL.
///
/// Two rows for a hosted context, in this order: the account (status and current epoch) and then the
/// specific credential (still owned by that account, not revoked, not consumed, neither deadline
/// passed, minted at the bound epoch). Both are *written* — `updated_at` / `last_used_at` — which is
/// what makes a concurrent change to either row cost this transaction a write conflict instead of
/// letting it commit over that change. A guarded `SELECT` would not: measured on the pinned 3.3.0
/// engine a revoke committed by another writer between a guard read and the commit silently wins
/// (`scoped_repos_test::a_guarded_read_alone_is_not_atomic` pins that, and
/// `a_credential_revoked_mid_transaction_does_not_commit_a_scoped_write` pins the shape that holds).
///
/// A trusted-local context has no credential and guards the account row only. It does not *claim* one:
/// `WriteAuthorization::TrustedLocal` carries no credential id, so the local path cannot be answered as
/// though a bearer token had been presented.
///
/// The epoch is the one `authorize` read off the live account row — but it is compared against the
/// account row *again* here, so a recovery that raised the epoch between authorization and this write is
/// caught even though nothing touched the credential's row. Each UPDATE binds the complete record
/// ID so numeric and opaque keys retain their identity.
///
/// `Ok(false)` means the authority is gone and the caller must cancel its transaction; it is not a
/// permission to proceed.
pub(crate) async fn claim_authority(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool, StorageError> {
    let (epoch, credential) = match owner.write_authorization() {
        WriteAuthorization::Credential {
            credential_id,
            auth_epoch,
        } => (*auth_epoch, Some(credential_id.clone())),
        WriteAuthorization::TrustedLocal { auth_epoch } => (*auth_epoch, None),
    };

    let account_rows: Vec<serde_json::Value> = tx
        .query(
            "UPDATE $id SET updated_at = time::now() WHERE id = $id AND \
             status = 'active' AND auth_epoch = $epoch RETURN AFTER;",
        )
        .bind(("id", owner.account().as_record_id().clone()))
        .bind(("epoch", epoch))
        .await
        .map_err(guard_internal)?
        .check()
        .map_err(guard_internal)?
        .take(0)
        .map_err(guard_internal)?;
    if account_rows.is_empty() {
        return Ok(false);
    }

    let Some(credential_id) = credential else {
        return Ok(true);
    };
    let credential_rows: Vec<serde_json::Value> = tx
        .query(
            "UPDATE $id SET last_used_at = time::now() WHERE id = $id \
             AND account = $account AND revoked = false AND consumed = false AND auth_epoch = $epoch \
             AND (expires_at = NONE OR expires_at > $now) AND \
             (absolute_expires_at = NONE OR absolute_expires_at > $now) RETURN AFTER;",
        )
        .bind(("id", credential_id.as_record_id().clone()))
        .bind(("account", owner.account().as_record_id().clone()))
        .bind(("epoch", epoch))
        .bind(("now", now))
        .await
        .map_err(guard_internal)?
        .check()
        .map_err(guard_internal)?
        .take(0)
        .map_err(guard_internal)?;
    Ok(!credential_rows.is_empty())
}

/// The opaque key of a record id, for `type::record('table', $key)`.
///
/// The key alone, taken from the parsed `RecordId`, and then *bound as a parameter*:
/// `record_id_to_string` is not a parser-safe SQL round trip for numeric or quoted opaque keys, so it
/// is never interpolated into statement text.
pub(crate) fn record_key(id: &RecordId) -> Result<String, StorageError> {
    use surrealdb::types::RecordIdKey;
    let key = match &id.key {
        RecordIdKey::String(text) => text.clone(),
        RecordIdKey::Number(number) => number.to_string(),
        RecordIdKey::Uuid(uuid) => uuid.to_string(),
        other => format!("{other:?}"),
    };
    if key.is_empty() {
        return Err(StorageError::Internal(anyhow::anyhow!(
            "record id has an empty key"
        )));
    }
    Ok(key)
}

/// Whether an engine failure is the optimistic write conflict a guard turns on a racing authority
/// change. Classified by `QueryError` variant — the accessor Task 4's claims use — never by message.
pub(crate) fn is_write_conflict(error: &surrealdb::Error) -> bool {
    matches!(error.query_details(), Some(QueryError::TransactionConflict))
}

fn guard_internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

#[cfg(test)]
mod system_barrier_tests {
    use super::*;
    use crate::models::OperationId;

    /// One owner, one seeded row in their workspace, and the two halves of the barrier: an active
    /// account lets the guarded system write through, a suspended one refuses it with `None` and the
    /// row is untouched.
    #[tokio::test]
    async fn a_suspended_account_blocks_the_guarded_system_write() {
        let (db, system) = crate::Database::bootstrap_embedded_for_tests()
            .await
            .unwrap();
        let ids = crate::repos::IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "barrier-owner",
                "digest-barrier-padding-padding-pad",
                &OperationId::new("b1"),
            )
            .await
            .unwrap();
        let partition = crate::models::identity::Partition {
            workspace: ids.workspace.clone(),
            scope: crate::models::identity::MemoryScope::Personal,
        };
        db.inner()
            .query(
                "CREATE type::record('fact', 'barrier-row') SET content = 'before', \
                    embedding = [0.1, 0.2], workspace = $workspace, deleted = false;",
            )
            .bind(("workspace", ids.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let write = GuardedWrite::new(
            "UPDATE type::record('fact', 'barrier-row') SET content = 'after' WHERE \
             workspace = $workspace RETURN AFTER;",
        );
        let written: Option<Vec<serde_json::Value>> =
            guarded_system_run(db.inner(), &db, &system, &partition, &write)
                .await
                .unwrap();
        assert!(
            written.is_some(),
            "an active account lets the write through; guarded_system_run returned {written:?}"
        );

        db.inner()
            .query("UPDATE ONLY $target SET status = 'suspended' RETURN NONE;")
            .bind(("target", ids.account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let write = GuardedWrite::new(
            "UPDATE type::record('fact', 'barrier-row') SET content = 'must-not-land' WHERE \
             workspace = $workspace RETURN AFTER;",
        );
        let refused: Option<Vec<serde_json::Value>> =
            guarded_system_run(db.inner(), &db, &system, &partition, &write)
                .await
                .unwrap();
        assert!(refused.is_none(), "a suspended account refuses the write");

        let rows: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT content FROM type::record('fact', 'barrier-row');")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            rows[0]["content"], "after",
            "the refused pass wrote nothing: {rows:?}"
        );
    }

    /// The capability check is part of the barrier: a system context from another instance is
    /// refused before any statement runs.
    #[tokio::test]
    async fn a_foreign_system_context_cannot_run_guarded_system_writes() {
        let (_db, system) = crate::Database::bootstrap_embedded_for_tests()
            .await
            .unwrap();
        let (other, _its_system) = crate::Database::bootstrap_embedded_for_tests()
            .await
            .unwrap();
        let partition = crate::models::identity::Partition {
            workspace: crate::models::identity::WorkspaceId::try_from(RecordId::new(
                "workspace",
                "w",
            ))
            .unwrap(),
            scope: crate::models::identity::MemoryScope::Personal,
        };
        let write = GuardedWrite::new("UPDATE anything SET x = 1;");
        let refused: Result<Option<Vec<serde_json::Value>>, _> =
            guarded_system_run(other.inner(), &other, &system, &partition, &write).await;
        assert!(matches!(refused, Err(StorageError::Stale)), "{refused:?}");
    }
}
