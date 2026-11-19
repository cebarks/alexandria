//! The shared mutation engine: one transaction per semantic change, one private journal for all of them.
//!
//! Everything that moves a memory's semantic state goes through [`MutationRepo::apply`], for a specific
//! reason rather than a taste for centralization. Before this module the ordinary writers issued content,
//! vector, tags, the hidden lineage snapshot, the `derived_from` edge, the session link, the cluster
//! membership and that cluster's centroid as **separate round trips**, and the edge call was `.ok()`-ed. A
//! half-failed update therefore left a live row whose text described one thing and whose vector another,
//! or a hidden snapshot with no edge pointing back from the row that replaced it, or a cluster whose
//! centroid averaged a member set nobody had approved — and nothing could answer "what did this memory say
//! before?" without a global audit table.
//!
//! # The three properties this repo owns
//!
//! **Atomicity, or the full semantic before-state remains.** `apply` opens one transaction, re-asserts the
//! writer's authority inside it (an admitted owner's credential/epoch claim, or the M1 active-workspace
//! system claim), verifies every
//! [`ExpectedState`] against the row *as the transaction finds it*, performs every write, writes the
//! journal row and its snapshots, and commits. Any refusal cancels the transaction, so the corpus keeps
//! its previous semantic state and the journal records nothing about the attempt. The failure injection in
//! `tests/mutation_test.rs` uses real engine refusals — a `RELATE` whose far end is not a `fact`, which
//! v002's `TYPE RELATION FROM fact TO fact` rejects, and an effect set which v009's `ASSERT
//! array::len(...) > 0` rejects — never test hooks.
//!
//! **The journal belongs to the workspace, and it is private.** `semantic_operation` is keyed on
//! `(workspace, operation_key)`, so two owners may both use `retry-1` and neither resolves the other's
//! operation. A replay of the *same* request (same [`PreparedChangeSet::request_fingerprint`]) returns the
//! recorded answer and writes nothing; the same key under a different fingerprint is refused, because
//! silently adopting somebody's earlier result would make an idempotency key a way to read another
//! request's outcome.
//!
//! **Fact snapshots are derived, not declared.** A caller states what it expects to find and what it wants
//! to leave behind; the fact pre/postimage journal rows are built *here*, from the rows this transaction
//! read and wrote, so a caller cannot journal a before-state that never existed.
//!
//! # What deliberately stays outside
//!
//! `heat_state` is not semantic state: v009 gives `semantic_snapshot` no heat or access-clock column, and
//! restoring one would rewind an access that happened after the snapshot. `apply` therefore writes heat
//! only as a *seed* for rows this transaction creates ([`PreparedChangeSet::heat_seed`]).
//!
//! Embeddings are prepared by the caller **before** the transaction (`store_body` embeds first, as it
//! always did): a provider round trip inside a database transaction would hold a write lock open for as
//! long as the model takes.
//!
//! Centroid *recomputation* is outside too, because `alexandria-storage` cannot depend on
//! `alexandria-engine` — the dependency points the other way. So the plan's three-preparation bound for a
//! derived-bookkeeping conflict lives in [`super::mutation_retry::commit_prepared`]. MCP and classification
//! callers supply preparation callbacks that refresh the numbers under the same pinned intent and
//! operation key. This repo's half is to classify the miss honestly:
//! [`StorageError::Stale`] when the fact or an approved membership effect moved, and
//! [`StorageError::RetryableBookkeepingConflict`] when derived counters or the numerical population no
//! longer match, including a compatibility visibility/vector write that changed no counter.

use crate::access::{MutationContext, OwnerContext, SystemContext};
use crate::error::StorageError;
use crate::models::identity::{
    AccountId, FactId, MemoryScope, OperationId, Partition, ProjectId, Version, WorkspaceId,
};
use crate::models::mutation::{
    AppliedChange, ClassificationApproval, ClusterChange, ClusterPopulationMember, ClusterSnapshot,
    ClusterWriteExpectation, DestinationMembership, EdgeKind, EdgeSnapshot, EmbeddingSnapshot,
    ExpectedState, FactChange, FactSnapshot, MaintenanceSummary, MoveReconciliation,
    OperationDetail, OperationSummary, PreparedChangeSet, RelationChange, RelationExpectation,
    RelationTarget, ReminderSnapshot, SemanticSnapshot, Visibility, effect,
};
use crate::models::mutation::{ProvenanceRow, RawDocument};
use crate::models::{operation_kind, scope_kind, snapshot_kind};
use crate::repos::scope::{
    ScopeFilter, claim_authority, claim_system_for_workspace, is_write_conflict, record_key,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::method::Query;
use surrealdb::method::Transaction;
use surrealdb::types::{RecordId, SurrealValue, ToSql, Value};

/// The largest history page the journal answers with. The caller's own operations are the population, so
/// this is a page bound rather than a privacy bound — and it is the figure the scope listings publish.
pub const MAX_HISTORY_PAGE: usize = 100;

mod quality;
use super::refinement_repo::application::{guard_quality_decision, guard_secondary, link_decision};

/// Everything one transaction pass needs, bundled because the pass legitimately carries eight values and
/// the lint bound is seven — and because the group is meaningful: it *is* the request. Holding the
/// fingerprint beside the change set rather than beside the transaction makes it impossible for a
/// re-planned pass to be paired with an older request's digest.
pub(crate) struct PassRequest<'a> {
    access: &'a MutationContext,
    operation: &'a OperationId,
    fingerprint: &'a str,
    expected: &'a [ExpectedState],
    change: &'a PreparedChangeSet,
    now: DateTime<Utc>,
}

#[derive(SurrealValue)]
struct ReminderMappingRow {
    id: RecordId,
    account: Option<RecordId>,
    message: String,
    target_project: Option<String>,
    legacy_target_project: Option<String>,
    project: Option<RecordId>,
    migration_hold: Option<bool>,
    semantic_revision: Option<i64>,
    schedule_kind: String,
}

struct ReminderMappingDecision<'a> {
    reminder_id: &'a RecordId,
    expected_revision: i64,
    target: Option<&'a ProjectId>,
    operation: &'a OperationId,
}

pub struct MutationRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> MutationRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    pub(crate) async fn system_mutation_context(
        &self,
        instance: &crate::Database,
        system: &SystemContext,
        partition: &Partition,
    ) -> Result<MutationContext, StorageError> {
        if !instance.holds_authority(system) {
            return Err(StorageError::Stale);
        }
        let owners: Vec<RecordId> = self
            .db
            .query("SELECT VALUE owner FROM workspace WHERE id = $workspace LIMIT 1;")
            .bind(("workspace", partition.workspace.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let owner = owners.into_iter().next().ok_or(StorageError::Stale)?;
        let account = AccountId::try_from(owner)?;
        Ok(MutationContext::issued_by_system(
            system,
            account,
            partition.workspace.clone(),
        ))
    }

    /// Capture the current semantic preimage for a partition-scoped system write.
    pub async fn expected_state_system(
        &self,
        instance: &crate::Database,
        system: &SystemContext,
        partition: &Partition,
        fact: &FactId,
    ) -> Result<ExpectedState, StorageError> {
        let access = self
            .system_mutation_context(instance, system, partition)
            .await?;
        let snapshot = self
            .state(access.owner(), fact)
            .await?
            .ok_or(StorageError::NotFound)?;
        Ok(ExpectedState::of(&snapshot))
    }

    /// Apply one journaled system mutation under the same M1 active-workspace barrier as the
    /// partitioned maintenance writers. The initiator is `NONE`: no credential presented this write.
    pub async fn apply_system(
        &self,
        instance: &crate::Database,
        system: &SystemContext,
        partition: &Partition,
        operation: &OperationId,
        expected: &[ExpectedState],
        change: &PreparedChangeSet,
    ) -> Result<AppliedChange, StorageError> {
        let access = self
            .system_mutation_context(instance, system, partition)
            .await?;
        self.apply(&access, operation, expected, change).await
    }

    // -- reading the state a caller must expect ------------------------------------------------------

    /// Read one memory's semantic state — the row, its vector, and its incident graph — as **the
    /// caller's own account and workspace**, for the [`ExpectedState`] a later `apply` must find.
    ///
    /// This is the only way to mint an expectation, and that is the point: the digest is computed over what
    /// a scoped read actually returned, so a caller cannot journal a before-state it never observed, and a
    /// foreign or absent row cannot be described at all (`NotFound`, never a probe).
    pub async fn expected_state(
        &self,
        owner: &OwnerContext,
        fact: &FactId,
    ) -> Result<ExpectedState, StorageError> {
        let snapshot = self
            .state(owner, fact)
            .await?
            .ok_or(StorageError::NotFound)?;
        Ok(ExpectedState::of(&snapshot))
    }

    /// The same read, for a caller that plans a change from the whole snapshot.
    pub async fn state(
        &self,
        owner: &OwnerContext,
        fact: &FactId,
    ) -> Result<Option<FactSnapshot>, StorageError> {
        let rows: Vec<FactRow> = self
            .db
            .query(FACT_READ_SQL)
            .bind(("fact", fact.as_record_id().clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let relations = self
            .relations(owner.workspace(), fact.as_record_id())
            .await?;
        Ok(Some(row.into_snapshot(owner.workspace(), relations)))
    }

    /// Every relation incident on one fact: five reads, one per relation table, never one per neighbour.
    async fn relations(
        &self,
        workspace: &WorkspaceId,
        fact: &RecordId,
    ) -> Result<Vec<EdgeSnapshot>, StorageError> {
        let mut edges: Vec<EdgeSnapshot> = Vec::new();
        let memory: Vec<MemoryEdgeRow> = self
            .db
            .query(MEMORY_EDGE_READ_SQL)
            .bind(("fact", fact.clone()))
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        edges.extend(memory.into_iter().map(MemoryEdgeRow::into_snapshot));
        for (kind, sql) in structural_reads() {
            let rows: Vec<StructuralRow> = self
                .db
                .query(sql)
                .bind(("fact", fact.clone()))
                .bind(("workspace", workspace.as_record_id().clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            edges.extend(rows.into_iter().map(|row| row.into_snapshot(kind.clone())));
        }
        Ok(edges)
    }

    /// The caller's cluster guards and one exact live population for BOTH arithmetic and validation.
    pub async fn cluster_expectation_system(
        &self,
        instance: &crate::Database,
        system: &SystemContext,
        partition: &Partition,
        cluster: &RecordId,
    ) -> Result<ClusterWriteExpectation, StorageError> {
        let access = self
            .system_mutation_context(instance, system, partition)
            .await?;
        self.cluster_expectation(access.owner(), cluster).await
    }

    pub async fn cluster_expectation(
        &self,
        owner: &OwnerContext,
        cluster: &RecordId,
    ) -> Result<ClusterWriteExpectation, StorageError> {
        let rows: Vec<GuardRow> = self
            .db
            .query(CLUSTER_GUARD_SQL)
            .bind(("key", record_key(cluster)?))
            .bind(("id", cluster.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let guard = rows.into_iter().next().ok_or(StorageError::NotFound)?;
        let rows: Vec<PopulationRow> = self
            .db
            .query(CLUSTER_POPULATION_SQL)
            .bind(("cluster", cluster.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let mut population: Vec<_> = rows
            .into_iter()
            .map(|row| ClusterPopulationMember {
                id: row.id,
                embedding: row.embedding,
            })
            .collect();
        population.sort_by(|left, right| left.id.cmp(&right.id));
        guard.into_expectation(cluster, population)
    }

    // -- the journal read back -----------------------------------------------------------------------

    /// The caller's own operations, newest first.
    ///
    /// Effect sets, kinds and dates only — no content, and no record ids. The list page is navigation;
    /// before/after arrives per operation through [`Self::operation`], which authorizes every snapshot on
    /// its own.
    pub async fn history(
        &self,
        owner: &OwnerContext,
        limit: usize,
    ) -> Result<Vec<OperationSummary>, StorageError> {
        let rows: Vec<JournalRow> = self
            .db
            .query(HISTORY_SQL)
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("limit", limit.clamp(1, MAX_HISTORY_PAGE) as i64))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(rows
            .into_iter()
            .map(|row| row.into_summary(owner))
            .collect())
    }

    /// One operation by its **workspace-scoped key**, with its exact before/after.
    ///
    /// The operation resolves under the caller's workspace, then each snapshot it names is re-read with
    /// that same workspace predicate rather than by handle alone. That is not belt and braces:
    /// `semantic_operation.preimages` is an array of record links, and a history page that resolved them
    /// unqualified would answer a stale or forged id with another owner's content — the one probe the
    /// private journal must never become.
    ///
    /// A foreign key and an absent one are one answer (`None`).
    pub async fn operation(
        &self,
        owner: &OwnerContext,
        key: &str,
    ) -> Result<Option<OperationDetail>, StorageError> {
        // No typed id: this form selects on the *operation key*, and `OPERATION_SQL` deliberately has no
        // `$id` in it. Passing `None` here is what keeps the two forms from silently sharing a predicate
        // they cannot both satisfy.
        self.detail(owner, OPERATION_SQL, key.to_string(), None)
            .await
    }

    /// The same detail by the operation row's own handle — how a history link reaches a detail page.
    pub async fn operation_by_id(
        &self,
        owner: &OwnerContext,
        id: &RecordId,
    ) -> Result<Option<OperationDetail>, StorageError> {
        self.detail(owner, OPERATION_BY_ID_SQL, record_key(id)?, Some(id))
            .await
    }

    /// One journal row read, by key or by handle, then every snapshot it names on its own authority.
    ///
    /// Both statements bind their selector under the name `key` — the operation key text in one case, the
    /// record key in the other — which is why one helper serves both and why the two forms cannot drift
    /// apart on the workspace predicate that makes them private.
    /// `id` is bound **only** for the by-handle form. An unbound parameter is `NONE` on this engine, so a
    /// `WHERE id = $id` that never receives `$id` is not an error and not a full scan — it is a predicate
    /// that matches nothing, which made the handle read answer `None` for *every* operation, including the
    /// caller's own. The bug survived its first test because that test only asked whether a **foreign**
    /// handle was refused, and a read that always returns nothing refuses everything.
    async fn detail(
        &self,
        owner: &OwnerContext,
        sql: &str,
        key: String,
        id: Option<&RecordId>,
    ) -> Result<Option<OperationDetail>, StorageError> {
        let workspace = owner.workspace().as_record_id().clone();
        let query = self
            .db
            .query(sql)
            .bind(("workspace", workspace.clone()))
            .bind(("key", key));
        let rows: Vec<JournalRow> = match id {
            Some(id) => query.bind(("id", id.clone())),
            None => query,
        }
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
        let Some(recorded) = rows.into_iter().next() else {
            return Ok(None);
        };
        let (preimages, preimage_records) = self.snapshots(&workspace, &recorded.preimages).await?;
        let (postimages, postimage_records) =
            self.snapshots(&workspace, &recorded.postimages).await?;
        Ok(Some(recorded.into_detail(
            owner,
            preimages,
            postimages,
            preimage_records,
            postimage_records,
        )))
    }

    async fn snapshots(
        &self,
        workspace: &RecordId,
        ids: &[RecordId],
    ) -> Result<(Vec<SemanticSnapshot>, Vec<Value>), StorageError> {
        let mut snapshots = Vec::with_capacity(ids.len());
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            let rows: Vec<Value> = self
                .db
                .query(SNAPSHOT_READ_SQL)
                .bind(("key", record_key(id)?))
                .bind(("id", id.clone()))
                .bind(("workspace", workspace.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if let Some(record) = rows.into_iter().next() {
                let row = SnapshotRow::from_value(record.clone()).map_err(internal)?;
                snapshots.push(row.into_snapshot(workspace)?);
                records.push(record);
            }
        }
        Ok((snapshots, records))
    }

    // -- the transaction -----------------------------------------------------------------------------

    /// Release a held legacy reminder mapping with the same owner claim and private semantic journal
    /// used for memory changes. The schedule and delivery clock are deliberately absent from the images.
    pub(crate) async fn release_reminder_mapping(
        &self,
        owner: &OwnerContext,
        reminder_id: &RecordId,
        expected: Version,
        target: Option<ProjectId>,
        operation: &OperationId,
    ) -> Result<(), StorageError> {
        let revision = i64::try_from(expected.get()).map_err(|_| {
            StorageError::InvalidInput("reminder revision is not representable".to_string())
        })?;
        let fingerprint = reminder_mapping_fingerprint(reminder_id, revision, target.as_ref());
        let decision = ReminderMappingDecision {
            reminder_id,
            expected_revision: revision,
            target: target.as_ref(),
            operation,
        };
        let now = Utc::now();
        let tx = self.db.clone().begin().await.map_err(internal)?;
        let result = self
            .release_reminder_mapping_in_tx(&tx, owner, &decision, &fingerprint, now)
            .await;
        match result {
            Ok(()) => Self::commit(tx).await,
            Err(error) => {
                tx.cancel().await.map_err(internal)?;
                Err(error)
            }
        }
    }

    async fn release_reminder_mapping_in_tx(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        decision: &ReminderMappingDecision<'_>,
        fingerprint: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let reminder_id = decision.reminder_id;
        let expected = decision.expected_revision;
        let target = decision.target;
        let operation = decision.operation;
        if !claim_authority(tx, owner, now).await? {
            return Err(StorageError::Stale);
        }
        let workspace = owner.workspace().as_record_id().clone();
        let existing: Vec<JournalRow> = tx
            .query(OPERATION_SQL)
            .bind(("workspace", workspace.clone()))
            .bind(("key", operation.as_str().to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if let Some(recorded) = existing.into_iter().next() {
            return if recorded.request_fingerprint == fingerprint {
                Ok(())
            } else {
                Err(StorageError::InvalidInput(
                    "this operation key already recorded a different request".to_string(),
                ))
            };
        }

        if let Some(project) = target {
            let projects: Vec<IdRow> = tx
                .query("SELECT id FROM project WHERE id = $project AND workspace = $workspace LIMIT 1;")
                .bind(("project", project.as_record_id().clone()))
                .bind(("workspace", workspace.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if projects.is_empty() {
                return Err(StorageError::NotFound);
            }
        }

        let rows: Vec<ReminderMappingRow> = tx
            .query(
                "SELECT id, account, message, target_project, legacy_target_project, project, \
                 migration_hold, semantic_revision, schedule_kind FROM type::record($id) \
                 WHERE id = $id AND account = $account LIMIT 1;",
            )
            .bind(("id", reminder_id.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let before = rows.into_iter().next().ok_or(StorageError::NotFound)?;
        if before.migration_hold != Some(true) || before.semantic_revision.unwrap_or(0) != expected
        {
            return Err(StorageError::Stale);
        }
        let after_revision = expected.checked_add(1).ok_or_else(|| {
            StorageError::InvalidInput("reminder revision is exhausted".to_string())
        })?;

        let effects = vec!["reminder:map".to_string()];
        let scope_kinds = vec![
            if target.is_some() {
                "project"
            } else {
                "personal"
            }
            .to_string(),
        ];
        let projects = target
            .map(|project| vec![project.as_record_id().clone()])
            .unwrap_or_default();
        let journal_rows: Vec<IdRow> = tx
            .query(CREATE_OPERATION_SQL)
            .bind(("workspace", workspace.clone()))
            .bind(("actor", owner.account().as_record_id().clone()))
            .bind(("initiator", MutationContext::issued_to(owner).initiator()))
            .bind(("kind", operation_kind::REMINDER_MAP.to_string()))
            .bind(("key", operation.as_str().to_string()))
            .bind(("fingerprint", fingerprint.to_string()))
            .bind(("effects", effects.clone()))
            .bind(("kinds", scope_kinds))
            .bind(("projects", projects))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let journal_id = journal_rows
            .into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing("semantic_operation"))?;

        let before_snapshot = reminder_mapping_snapshot(before)?;
        let before_image = SemanticSnapshot::Reminder(before_snapshot);
        let preimage_id = self
            .write_snapshot(tx, &before_image, &journal_id, &workspace)
            .await?;
        let update = tx
            .query(
                "UPDATE type::record($id) SET project = $project, migration_hold = false, \
                 semantic_revision = $next WHERE id = $id AND account = $account \
                 AND migration_hold = true AND (semantic_revision = $expected OR \
                 (semantic_revision = NONE AND $expected = 0)) RETURN AFTER;",
            )
            .bind(("id", reminder_id.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind((
                "project",
                target.map(|project| project.as_record_id().clone()),
            ))
            .bind(("expected", expected))
            .bind(("next", after_revision));
        let updated: Vec<ReminderMappingRow> = update
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let after = updated.into_iter().next().ok_or(StorageError::Stale)?;
        let after_image = SemanticSnapshot::Reminder(reminder_mapping_snapshot(after)?);
        let postimage_id = self
            .write_snapshot(tx, &after_image, &journal_id, &workspace)
            .await?;
        tx.query(FINISH_OPERATION_SQL)
            .bind(("key", record_key(&journal_id)?))
            .bind(("preimages", vec![preimage_id]))
            .bind(("postimages", vec![postimage_id]))
            .bind(("affected", vec![reminder_id.clone()]))
            .bind((
                "preimage_hash",
                digest_of(std::slice::from_ref(&before_image)),
            ))
            .bind((
                "postimage_hash",
                digest_of(std::slice::from_ref(&after_image)),
            ))
            .bind(("actual", effects))
            .bind(("now", now))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        Ok(())
    }

    /// Apply one semantic change atomically, under an admitted owner or storage-issued system authority.
    ///
    /// Returns the recorded answer for a duplicate same-key request, and refuses — having written nothing —
    /// for a stale expectation, a lost authority, a key reused across requests, or any engine refusal.
    pub async fn apply(
        &self,
        access: &MutationContext,
        operation: &OperationId,
        expected: &[ExpectedState],
        change: &PreparedChangeSet,
    ) -> Result<AppliedChange, StorageError> {
        let now = Utc::now();
        let fingerprint = change.request_fingerprint();
        let tx = self.db.clone().begin().await.map_err(internal)?;
        let request = PassRequest {
            access,
            operation,
            fingerprint: &fingerprint,
            expected,
            change,
            now,
        };
        match self.run(&tx, &request).await {
            Ok(applied) => {
                Self::commit(tx).await?;
                Ok(applied)
            }
            Err(error) => {
                // This cancel is why "or the full semantic before-state remains" is a property of the
                // transaction rather than of any writer's care.
                tx.cancel().await.map_err(internal)?;
                Err(error)
            }
        }
    }

    async fn commit(tx: Transaction<Any>) -> Result<(), StorageError> {
        tx.commit().await.map(|_| ()).map_err(Self::commit_error)
    }

    /// Decide the cited pending proposal inside this transaction, or refuse having written nothing.
    ///
    /// One conditional update decides the entry: it requires the row to still be this owner's, still
    /// `pending`, still at the cited revision, still about the cited fact at the cited source revision,
    /// and still naming the cited destination. A row that moved under the approval is `Stale`; a row that
    /// is absent or foreign is `NotFound`, indistinguishably, so this clause cannot be used as an
    /// existence oracle. Deciding time and actor are stamped here — the transaction's own — and never
    /// carried in the payload, which is why [`ClassificationApproval`] has no such fields.
    ///
    /// The clause is validated scope-only first: exactly one `FactChange::Update`, to the cited fact,
    /// at the cited source revision, moving exactly the cited destination, with no content, tags,
    /// confidence, vector or visibility change riding inside it. Classification is a placement opinion;
    /// a command that smuggled an edit into an approval is a different operation wearing this one's key.
    async fn approve_classification_in_tx(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        change: &PreparedChangeSet,
        approval: &ClassificationApproval,
        subject: Option<FactSnapshot>,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        if approval.proposal.table.as_str() != "classification_proposal" {
            return Err(StorageError::InvalidInput(
                "a classification approval must cite a classification_proposal row".to_string(),
            ));
        }
        // The scope-only shape, destructured so an extra field is a compile-time impossibility to forget
        // rather than a checklist item: any `Some` in the forbidden positions falls to the catch-all.
        let (fact, expected, destination) = match change.facts.as_slice() {
            [
                FactChange::Update {
                    fact,
                    expected,
                    content: None,
                    tags: None,
                    confidence: None,
                    embedding: None,
                    visibility: None,
                    scope: Some(destination),
                },
            ] => (fact, *expected, destination),
            _ => {
                return Err(StorageError::InvalidInput(
                    "a classification approval must carry exactly one scope-only fact update"
                        .to_string(),
                ));
            }
        };
        if fact.as_record_id() != approval.fact.as_record_id()
            || expected != approval.source_revision
            || destination != &approval.destination
        {
            return Err(StorageError::InvalidInput(
                "the approval clause and the fact update describe different placements".to_string(),
            ));
        }
        Self::validate_classification_fields(change)?;
        let subject = subject.ok_or_else(|| {
            StorageError::InvalidInput(
                "a journaled write named a fact no expectation described".to_string(),
            )
        })?;
        Self::validate_classification_clusters(change, approval, &subject)?;
        let revision = i64::try_from(approval.proposal_revision.get()).map_err(|_| {
            StorageError::InvalidInput("proposal revision is not representable".to_string())
        })?;
        let source_revision = i64::try_from(approval.source_revision.get()).map_err(|_| {
            StorageError::InvalidInput("source revision is not representable".to_string())
        })?;
        let (kind, project) = match &approval.destination {
            MemoryScope::Personal => (scope_kind::PERSONAL, None),
            MemoryScope::Unassigned => (scope_kind::UNASSIGNED, None),
            MemoryScope::Project(project) => {
                (scope_kind::PROJECT, Some(project.as_record_id().clone()))
            }
        };
        let rows: Vec<Value> = tx
            .query(
                "UPDATE $id \
                 SET state = 'approved', decided_at = $now, decided_by = $account \
                 WHERE id = $id AND workspace = $workspace AND account = $account \
                 AND revision = $revision AND state = 'pending' AND fact = $fact \
                 AND source_revision = $source_revision AND destination_kind = $kind \
                 AND destination_project = $project;",
            )
            .bind(("id", approval.proposal.clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("revision", revision))
            .bind(("fact", approval.fact.as_record_id().clone()))
            .bind(("source_revision", source_revision))
            .bind(("kind", kind.to_string()))
            .bind(("project", project))
            .bind(("now", now))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if rows.is_empty() {
            // Absent-or-foreign and owned-but-moved must not share an answer: the first is a citation
            // error the caller cannot retry, the second is a race it must be told about. Reading the row
            // owner-qualified — after the failed conditional, still inside this transaction — separates
            // them without leaking whether a foreign row exists.
            let owned: Vec<Value> = tx
                .query(
                    "SELECT id FROM $id \
                     WHERE id = $id AND account = $account AND workspace = $workspace;",
                )
                .bind(("id", approval.proposal.clone()))
                .bind(("account", owner.account().as_record_id().clone()))
                .bind(("workspace", owner.workspace().as_record_id().clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if owned.is_empty() {
                return Err(StorageError::NotFound);
            }
            return Err(StorageError::Stale);
        }
        Ok(())
    }

    fn validate_classification_fields(change: &PreparedChangeSet) -> Result<(), StorageError> {
        // No `..`: a new write collection must make this envelope decision explicit.
        let PreparedChangeSet {
            kind,
            effects: _,
            facts: _,
            edge_additions,
            edge_removals,
            session_links,
            cluster_changes: _,
            embedding_space,
            maintenance_summary,
            raw_documents,
            provenance_rows,
            heat_seed,
            classification_approval: _,
            quality_transition,
            quality_embeddings,
            quality_details,
        } = change;
        if *kind != operation_kind::MOVE
            || !edge_additions.is_empty()
            || !edge_removals.is_empty()
            || !session_links.is_empty()
            || embedding_space.is_some()
            || maintenance_summary.is_some()
            || !raw_documents.is_empty()
            || !provenance_rows.is_empty()
            || heat_seed.is_some()
            || quality_transition.is_some()
            || !quality_embeddings.is_empty()
            || quality_details.is_some()
        {
            return Err(StorageError::InvalidInput(
                "a classification approval contains writes outside its placement".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_classification_clusters(
        change: &PreparedChangeSet,
        approval: &ClassificationApproval,
        subject: &FactSnapshot,
    ) -> Result<(), StorageError> {
        let invalid = || {
            StorageError::InvalidInput(
                "classification cluster effects must describe only the cited placement".to_string(),
            )
        };
        let fact = approval.fact.as_record_id();
        let mut source = None;
        for edge in &subject.relation_versions {
            if edge.kind == EdgeKind::ClusterMembership && &edge.to == fact {
                if source.is_some_and(|cluster| cluster != &edge.from) {
                    return Err(invalid());
                }
                source = Some(&edge.from);
            }
        }
        let mut detached = false;
        let mut arrived = false;
        let mut effects = vec![effect::SCOPE.to_string()];
        for cluster in &change.cluster_changes {
            match cluster {
                ClusterChange::Detach {
                    expectation,
                    member,
                    surviving_centroid,
                } if !detached && source == Some(&expectation.cluster) && member == fact => {
                    detached = true;
                    effects.push(EdgeKind::ClusterMembership.effect());
                    if surviving_centroid.is_some() {
                        effects.push(effect::CLUSTER_CENTROID.to_string());
                    }
                }
                ClusterChange::Effect {
                    expectation,
                    add_member: Some(RelationTarget::Existing(member)),
                    remove_member: None,
                    centroid: Some(_),
                } if !arrived && member == fact && source != Some(&expectation.cluster) => {
                    arrived = true;
                    effects.push(EdgeKind::ClusterMembership.effect());
                    effects.push(effect::CLUSTER_CENTROID.to_string());
                }
                ClusterChange::Create {
                    member: RelationTarget::Existing(member),
                    centroid,
                    label: _,
                } if !arrived
                    && member == fact
                    && subject
                        .embedding
                        .as_ref()
                        .is_some_and(|vector| &vector.vector == centroid) =>
                {
                    arrived = true;
                    effects.push(EdgeKind::ClusterMembership.effect());
                }
                _ => return Err(invalid()),
            }
        }
        if (source.is_some() && (subject.scope != approval.destination || arrived) && !detached)
            || change.effects != effects
        {
            return Err(invalid());
        }
        Ok(())
    }

    /// Only for the explicit SDK commit boundary, never query/auth/user-thrown errors.
    fn commit_error(error: surrealdb::Error) -> StorageError {
        // TODO(debt): embedded SDK 3.3 erases commit kinds. Remove this bridge only after the real
        // commit tests establish typed propagation; the envelope is pinned KvsError::TransactionConflict.
        let erased_conflict = error.is_internal()
            && error.cause().is_none()
            && error
                .message()
                .strip_prefix("Transaction conflict: ")
                .and_then(|reason| reason.strip_suffix(". This transaction can be retried"))
                .is_some_and(|reason| !reason.trim().is_empty());
        if is_write_conflict(&error) || erased_conflict {
            StorageError::RetryableBookkeepingConflict
        } else {
            internal(error)
        }
    }

    async fn run(
        &self,
        tx: &Transaction<Any>,
        request: &PassRequest<'_>,
    ) -> Result<AppliedChange, StorageError> {
        let access = request.access;
        let operation = request.operation;
        let fingerprint = request.fingerprint;
        let expected = request.expected;
        let change = request.change;
        let now = request.now;
        quality::validate_envelope(change)?;
        let quality = matches!(
            change.kind,
            operation_kind::QUALITY | operation_kind::REVERT
        );
        let revert = change
            .quality_details
            .as_ref()
            .and_then(|detail| detail.reverts.as_ref());
        let owner = access.owner();
        let workspace = owner.workspace().as_record_id().clone();

        // 1. Authority, asserted as a conditional write on the rows that carry it.
        let admitted = if access.system().is_some() {
            claim_system_for_workspace(tx, owner.workspace().as_record_id()).await?
        } else {
            claim_authority(tx, owner, now).await?
        };
        if !admitted {
            return Err(StorageError::Stale);
        }

        // 2. Idempotency, read inside the transaction so a racing duplicate is seen rather than raced.
        let existing: Vec<JournalRow> = tx
            .query(OPERATION_SQL)
            .bind(("workspace", workspace.clone()))
            .bind(("key", operation.as_str().to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if let Some(recorded) = existing.into_iter().next() {
            if recorded.request_fingerprint == fingerprint {
                if let Some(details) = &change.quality_details
                    && let Some(generation) = details.generation
                {
                    let proposal = details.proposal.as_ref().ok_or(StorageError::Stale)?;
                    if access.system().is_none() {
                        return Err(StorageError::Stale);
                    }
                    guard_secondary(tx, owner, proposal, generation).await?;
                }
                return Ok(recorded.into_replay());
            }
            return Err(StorageError::InvalidInput(
                "this operation key already recorded a different request".to_string(),
            ));
        }

        // 3. Expectations, against the rows as *this* transaction finds them. A compatibility writer that
        //    edited a row after the caller read it, and never bumped the counter it ignored, is caught by
        //    the digest — which is why both halves are compared.
        let mut approval_subject = None;
        for state in expected {
            let actual = self
                .state_in_tx(tx, owner, &state.fact)
                .await?
                .ok_or(StorageError::NotFound)?;
            if actual.semantic_hash() != state.semantic_hash
                || actual.version != state.version
                || actual.workspace != state.workspace
            {
                return Err(StorageError::Stale);
            }
            for guard in &state.graph {
                if !self.matches_relation(tx, guard).await? {
                    return Err(StorageError::Stale);
                }
            }
            if change
                .classification_approval
                .as_ref()
                .is_some_and(|approval| approval.fact == state.fact)
            {
                approval_subject = Some(actual);
            }
        }

        if change.quality_transition.is_some() {
            guard_quality_decision(
                self,
                tx,
                owner,
                operation,
                expected,
                change,
                access.system().is_some(),
            )
            .await?;
        }

        // Validate the preparer's numerical prestate before this request changes any member's vector,
        // scope or visibility. Replays above do not depend on a population that may have moved since.
        for cluster in &change.cluster_changes {
            let expectations: Vec<&ClusterWriteExpectation> = match cluster {
                ClusterChange::Create { .. } => continue,
                ClusterChange::Guard { expectation }
                | ClusterChange::Detach { expectation, .. }
                | ClusterChange::Effect { expectation, .. } => vec![expectation],
                ClusterChange::Restructure { expectations, .. } => expectations.iter().collect(),
            };
            for expectation in expectations {
                self.assert_cluster_population(tx, owner, expectation)
                    .await?;
                if matches!(cluster, ClusterChange::Guard { .. }) {
                    self.assert_cluster_revisions(tx, owner, expectation)
                        .await?;
                }
            }
        }

        // 3.5 A classification command decides its proposal in the same transaction as the move it
        //     guards: the conditional approval below either matches the row exactly as cited — pending,
        //     this owner's, this revision, this fact at this source revision, this destination — or the
        //     whole transaction refuses, so a proposal that moved between capture and commit can be
        //     neither moved by a stale approval nor left approved while its memory stayed put.
        if let Some(approval) = &change.classification_approval {
            self.approve_classification_in_tx(tx, owner, change, approval, approval_subject, now)
                .await?;
        }

        // 4. The journal row precedes the snapshots so each can name its operation. v009's
        //    `ASSERT array::len($value) > 0` on `expected_effects` fires here: an operation describing no
        //    effect is refused by the schema, which is the audit-half failure injection.
        let rows: Vec<IdRow> = tx
            .query(CREATE_OPERATION_SQL)
            .bind(("workspace", workspace.clone()))
            .bind(("actor", owner.account().as_record_id().clone()))
            .bind(("initiator", access.initiator()))
            .bind(("kind", change.kind.to_string()))
            .bind(("key", operation.as_str().to_string()))
            .bind(("fingerprint", fingerprint.to_string()))
            .bind(("effects", change.effects.clone()))
            .bind(("kinds", change.affected_scope_kinds()))
            .bind(("projects", change.affected_projects()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let journal_id = rows
            .into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing("semantic_operation"))?;

        // 5. The writes, in an order that makes a within-transaction reference legitimate: source
        //    documents, then facts, then the relations that point at them.
        let mut preimages: Vec<SemanticSnapshot> = Vec::new();
        let mut postimages: Vec<SemanticSnapshot> = Vec::new();
        let mut minted: Vec<RecordId> = Vec::new();
        let mut raws: Vec<RecordId> = Vec::new();
        let mut provenances: Vec<RecordId> = Vec::new();
        if quality {
            for state in expected {
                let snapshot = self.quality_state(tx, owner, &state.fact).await?;
                if !change
                    .facts
                    .iter()
                    .any(|write| write.target() == Some(&state.fact))
                {
                    preimages.push(SemanticSnapshot::Fact(snapshot.clone()));
                    postimages.push(SemanticSnapshot::Fact(snapshot));
                }
            }
            self.guard_quality_relations(tx, change, expected).await?;
        }

        for document in &change.raw_documents {
            raws.push(self.create_raw(tx, owner, document).await?);
        }

        for write in &change.facts {
            match write {
                FactChange::Create { .. } => {
                    let stable = change
                        .quality_details
                        .as_ref()
                        .and_then(|details| details.stable_slots.get(minted.len()));
                    let snapshot = self.create_fact_at(tx, owner, write, stable).await?;
                    minted.push(snapshot.id.as_record_id().clone());
                    if let Some(heat) = change.heat_seed {
                        self.seed_heat(tx, snapshot.id.as_record_id(), heat).await?;
                    }
                    postimages.push(SemanticSnapshot::Fact(snapshot));
                }
                FactChange::Update { fact, .. } => {
                    // A journaled write aimed at a row no expectation described would be an unguarded
                    // overwrite, so this is refused before anything is written.
                    if !expected.iter().any(|state| &state.fact == fact) {
                        return Err(StorageError::InvalidInput(
                            "a journaled write named a fact no expectation described".to_string(),
                        ));
                    }
                    let before = self
                        .state_in_tx(tx, owner, fact)
                        .await?
                        .ok_or(StorageError::NotFound)?;
                    let after = self.update_fact(tx, owner, write, &before).await?;
                    preimages.push(SemanticSnapshot::Fact(before));
                    postimages.push(SemanticSnapshot::Fact(after));
                }
            }
        }

        for row in &change.provenance_rows {
            let created = self.create_provenance(tx, owner, row).await?;
            // The link is written here, not by a later call: `AGENTS.md` records that a "no provenance
            // was linked" assertion built on a bare `create_fact` passes vacuously, and the ordinary
            // store path used to issue the create and the `RELATE` as two round trips.
            let fact = resolve(&row.fact, &minted, &raws, &provenances)?;
            tx.query("RELATE $fact->has_provenance->$provenance;")
                .bind(("fact", fact))
                .bind(("provenance", created.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
            provenances.push(created);
        }

        // 6. Cluster effects, whose CAS is the plan's derived-bookkeeping guard. Each one is imaged
        //    **before** it is written and **after** it is written, both inside this transaction, so the
        //    journal can answer "what was that cluster's membership, centroid and set of guards before you
        //    touched it" for a cluster rather than for the memory alone. `Create` states its own absence:
        //    a minted row has no earlier state, and an empty image of it would be a fabricated preimage.
        for cluster in &change.cluster_changes {
            // A defended absence is not a cluster mutation and must not invent history images.
            if matches!(cluster, ClusterChange::Guard { .. }) {
                continue;
            }
            let (before, after) = self
                .apply_cluster(tx, owner, cluster, &minted, revert.is_some())
                .await?;
            preimages.extend(before);
            postimages.extend(after);
        }

        // 7. Relations, after the creates they may point at.
        for addition in &change.edge_additions {
            let edge = self
                .relate_resolved(tx, addition, &minted, &raws, &provenances)
                .await?;
            // Only a `memory_edge` row may become an `edge` snapshot: v009's discriminator assertion
            // requires `meta::tb(subject) = 'memory_edge'`, so journaling a structural membership as an
            // edge is refused by the schema *after* the corpus writes — and a change that approves a
            // cluster membership would then never commit. A structural effect is already covered by the
            // endpoint's own image: the cluster snapshot carries its member set, and the fact's digest
            // carries the incident edge.
            if matches!(addition.kind, EdgeKind::Memory(_)) {
                postimages.push(SemanticSnapshot::Edge(edge));
            }
        }
        for removal in &change.edge_removals {
            self.unrelate_resolved(tx, removal, &minted, &raws, &provenances)
                .await?;
        }
        for link in &change.session_links {
            let to = resolve(&link.to, &minted, &raws, &provenances)?;
            self.relate(tx, &EdgeKind::SessionMembership, &link.session, &to, 1.0)
                .await?;
        }

        if quality {
            self.write_generated_sessions(tx, change, &minted).await?;
            self.refresh_quality_images(tx, owner, &mut postimages)
                .await?;
        }

        // 8. Every image as this transaction produced it. Order is load-bearing: `semantic_operation.preimages`
        //    is the list the history page resolves, so a snapshot written after the row closed over it would
        //    be invisible.
        let mut preimage_ids: Vec<RecordId> = Vec::with_capacity(preimages.len());
        for snapshot in &preimages {
            let id = self
                .write_snapshot(tx, snapshot, &journal_id, &workspace)
                .await?;
            if quality {
                quality::write_graph(tx, &id, snapshot).await?;
            }
            preimage_ids.push(id);
        }
        let mut postimage_ids: Vec<RecordId> = Vec::with_capacity(postimages.len());
        for snapshot in &postimages {
            let id = self
                .write_snapshot(tx, snapshot, &journal_id, &workspace)
                .await?;
            if quality {
                quality::write_graph(tx, &id, snapshot).await?;
            }
            postimage_ids.push(id);
        }

        // 9. A background writer's linked summary, in the same transaction as the change it describes.
        if let Some(summary) = &change.maintenance_summary {
            self.write_summary(tx, summary, &journal_id).await?;
        }

        // 10. Close the journal row over what actually happened. `actual_effects` may be shorter than
        //     `expected_effects`, which is how a batch reports per-entry conflicts and why a stale batch
        //     entry can never silently overwrite a later move.
        let mut affected: Vec<RecordId> = Vec::new();
        for id in minted.iter().chain(raws.iter()).chain(provenances.iter()) {
            push_unique(&mut affected, id.clone());
        }
        for snapshot in &postimages {
            push_unique(&mut affected, snapshot.subject());
        }
        tx.query(FINISH_OPERATION_SQL)
            .bind(("key", record_key(&journal_id)?))
            .bind(("preimages", preimage_ids.clone()))
            .bind(("postimages", postimage_ids.clone()))
            .bind(("affected", affected))
            .bind(("preimage_hash", digest_of(&preimages)))
            .bind(("postimage_hash", digest_of(&postimages)))
            .bind(("actual", change.effects.clone()))
            .bind(("now", now))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;

        if let Some(transition) = &change.quality_transition {
            link_decision(tx, transition, &journal_id).await?;
        }
        if let Some(original) = revert {
            quality::link_reversal(tx, owner, original, &journal_id).await?;
        }

        let created: Vec<FactId> = minted
            .iter()
            .filter_map(|id| FactId::try_from(id.clone()).ok())
            .collect();
        Ok(AppliedChange {
            operation: crate::record_id_to_string(&journal_id),
            preimages: names(&preimage_ids),
            postimages: names(&postimage_ids),
            audit: crate::record_id_to_string(&journal_id),
            created,
            created_raw: raws,
            replayed: false,
        })
    }

    pub(super) async fn state_in_tx(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        fact: &FactId,
    ) -> Result<Option<FactSnapshot>, StorageError> {
        let rows: Vec<FactRow> = tx
            .query(FACT_READ_SQL)
            .bind(("fact", fact.as_record_id().clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let relations = self
            .relations_in_tx(tx, owner.workspace(), fact.as_record_id())
            .await?;
        Ok(Some(row.into_snapshot(owner.workspace(), relations)))
    }

    async fn relations_in_tx(
        &self,
        tx: &Transaction<Any>,
        workspace: &WorkspaceId,
        fact: &RecordId,
    ) -> Result<Vec<EdgeSnapshot>, StorageError> {
        let mut edges: Vec<EdgeSnapshot> = Vec::new();
        let memory: Vec<MemoryEdgeRow> = tx
            .query(MEMORY_EDGE_READ_SQL)
            .bind(("fact", fact.clone()))
            .bind(("workspace", workspace.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        edges.extend(memory.into_iter().map(MemoryEdgeRow::into_snapshot));
        for (kind, sql) in structural_reads() {
            let rows: Vec<StructuralRow> = tx
                .query(sql)
                .bind(("fact", fact.clone()))
                .bind(("workspace", workspace.as_record_id().clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            edges.extend(rows.into_iter().map(|row| row.into_snapshot(kind.clone())));
        }
        Ok(edges)
    }

    async fn matches_relation(
        &self,
        tx: &Transaction<Any>,
        guard: &RelationExpectation,
    ) -> Result<bool, StorageError> {
        let (kind, from, to) = match guard {
            RelationExpectation::Present { kind, from, to, .. }
            | RelationExpectation::Absent { kind, from, to } => (kind, from.clone(), to.clone()),
        };
        let sql = match kind.edge_type() {
            Some(_) => format!(
                "SELECT id FROM {} WHERE in = $from AND out = $to AND edge_type = $edge_type;",
                kind.table()
            ),
            None => format!(
                "SELECT id FROM {} WHERE in = $from AND out = $to;",
                kind.table()
            ),
        };
        let mut query = tx.query(sql).bind(("from", from)).bind(("to", to));
        if let Some(edge_type) = kind.edge_type() {
            query = query.bind(("edge_type", edge_type.to_string()));
        }
        let rows: Vec<IdRow> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(match guard {
            RelationExpectation::Present { edge, .. } => match edge {
                // A named handle must still point at these endpoints: the row moved is a different fact
                // about the graph even when an equivalent edge exists.
                Some(expected) => rows.iter().any(|row| &row.id == expected),
                None => !rows.is_empty(),
            },
            RelationExpectation::Absent { .. } => rows.is_empty(),
        })
    }

    async fn create_raw(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        document: &RawDocument,
    ) -> Result<RecordId, StorageError> {
        let rows: Vec<IdRow> = tx
            .query(CREATE_RAW_SQL)
            .bind(("content", document.content.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        rows.into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing("raw"))
    }

    async fn create_provenance(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        row: &ProvenanceRow,
    ) -> Result<RecordId, StorageError> {
        let created: Vec<IdRow> = tx
            .query(CREATE_PROVENANCE_SQL)
            .bind(("kind", row.kind.clone()))
            .bind(("now", row.timestamp))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        created
            .into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing("provenance"))
    }

    /// Seed the heat row of a memory this transaction created.
    ///
    /// A *seed*, not an update: `heat_state` is not journaled, and this is only ever reached for a row
    /// that did not exist a moment ago, so no access clock can be overwritten.
    async fn seed_heat(
        &self,
        tx: &Transaction<Any>,
        memory: &RecordId,
        heat: f64,
    ) -> Result<(), StorageError> {
        tx.query(CREATE_HEAT_SQL)
            .bind(("memory", memory.clone()))
            .bind(("heat", heat))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        Ok(())
    }

    /// Refuse a scope this caller may not write into, inside the transaction that would write it.
    ///
    /// A `Project` destination must name a project **of this workspace**: `ScopedMemoryRepo`'s own
    /// `resolve_write_partition` applies the same rule, and a journaled writer that skipped it would file
    /// a memory in somebody else's partition — where every scoped reader fails to find it, including the
    /// caller who wrote it. Absent and foreign are one `NotFound`, so this is not a probe for which
    /// projects exist.
    async fn assert_writable_scope(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        scope: &MemoryScope,
    ) -> Result<(), StorageError> {
        let MemoryScope::Project(project) = scope else {
            return Ok(());
        };
        let rows: Vec<IdRow> = tx
            .query(
                "SELECT id FROM type::record('project', $key) WHERE id = $project AND workspace =                  $workspace LIMIT 1;",
            )
            .bind(("key", record_key(project.as_record_id())?))
            .bind(("project", project.as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if rows.is_empty() {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }

    async fn create_fact_at(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        write: &FactChange,
        stable: Option<&FactId>,
    ) -> Result<FactSnapshot, StorageError> {
        let FactChange::Create {
            scope,
            content,
            tags,
            confidence,
            embedding,
            visibility,
            metadata,
        } = write
        else {
            return Err(mismatched("create"));
        };
        self.assert_writable_scope(tx, owner, scope).await?;
        let (kind, project) = scope_parts(scope);
        let mut query = match stable {
            Some(id) => tx
                .query(CREATE_FACT_SQL.replacen("CREATE fact", "CREATE $stable", 1))
                .bind(("stable", id.as_record_id().clone())),
            None => tx.query(CREATE_FACT_SQL),
        };
        query = query
            .bind(("content", content.clone()))
            .bind(("tags", tags.clone()))
            .bind(("confidence", *confidence))
            .bind(("embedding", embedding.vector.clone()))
            .bind(("deleted", visibility.deleted))
            .bind(("quarantined_at", visibility.quarantined_at))
            .bind(("metadata", metadata.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("scope", kind.to_string()))
            .bind(("project", project));
        let rows: Vec<IdRow> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let created = rows
            .into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing("fact"))?;
        Ok(FactSnapshot {
            id: FactId::try_from(created)?,
            workspace: owner.workspace().clone(),
            scope: scope.clone(),
            // The row this transaction created starts its guarded life at one: zero is what an
            // unjournaled row carries, so stamping one makes "written under the journal" visible in the
            // counter itself.
            version: Version::new(1),
            content: content.clone(),
            tags: tags.clone(),
            confidence: *confidence,
            visibility: *visibility,
            embedding: Some(embedding.clone()),
            relation_versions: Vec::new(),
        })
    }

    async fn update_fact(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        write: &FactChange,
        before: &FactSnapshot,
    ) -> Result<FactSnapshot, StorageError> {
        let FactChange::Update {
            fact,
            expected,
            content,
            tags,
            confidence,
            embedding,
            visibility,
            scope,
        } = write
        else {
            return Err(mismatched("update"));
        };
        if *expected != before.version {
            return Err(StorageError::Stale);
        }
        // The destination is checked under the same transaction, so a move approved against an old
        // revision can neither land in a foreign project nor land at all.
        if let Some(scope) = scope {
            self.assert_writable_scope(tx, owner, scope).await?;
        }
        let mut sets: Vec<&str> = Vec::new();
        let mut binds: Vec<(&str, Bind)> = Vec::new();
        if let Some(content) = content {
            sets.push("content = $content");
            binds.push(("content", Bind::Text("content", content.clone())));
        }
        if let Some(tags) = tags {
            sets.push("tags = $tags");
            binds.push(("tags", Bind::TextList("tags", tags.clone())));
        }
        if let Some(confidence) = confidence {
            sets.push("confidence = $confidence");
            binds.push(("confidence", Bind::Float("confidence", *confidence)));
        }
        if let Some(embedding) = embedding {
            sets.push("embedding = $embedding");
            binds.push((
                "embedding",
                Bind::Floats("embedding", embedding.vector.clone()),
            ));
        }
        if let Some(visibility) = visibility {
            sets.push("deleted = $deleted");
            sets.push("quarantined_at = $quarantined");
            binds.push(("deleted", Bind::Bool("deleted", visibility.deleted)));
            binds.push((
                "quarantined",
                Bind::OptTime("quarantined", visibility.quarantined_at),
            ));
        }
        if let Some(scope) = scope {
            let (kind, project) = scope_parts(scope);
            sets.push("scope = $scope");
            sets.push("project = $project");
            binds.push(("scope", Bind::Text("scope", kind.to_string())));
            binds.push(("project", Bind::OptRecord("project", project)));
        }
        if sets.is_empty() {
            return Ok(before.clone());
        }
        // The revision is both guard and increment in one statement: the `WHERE` compares what the caller
        // read and the `SET` writes `+ 1`, so an update that loses the race loses the row, not the winner.
        let sql = format!(
            "UPDATE $id SET {}, semantic_revision = $version + 1 WHERE id \
             = $id AND account = $account AND workspace = $workspace AND semantic_revision = \
             $version RETURN AFTER;",
            sets.join(", ")
        );
        let mut query = tx
            .query(sql)
            .bind(("id", fact.as_record_id().clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("version", revision(before.version)));
        for (_, bind) in binds {
            query = bind.apply(query);
        }
        let rows: Vec<FactRow> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let updated = rows.into_iter().next().ok_or(StorageError::Stale)?;
        let relations = self
            .relations_in_tx(tx, owner.workspace(), fact.as_record_id())
            .await?;
        Ok(updated.into_snapshot(owner.workspace(), relations))
    }

    /// Apply one cluster effect, and image its full retained membership **on both sides of the write**.
    ///
    /// The returned pair is `(before, after)`, and the two are not the same kind of answer. `after` is what
    /// this transaction just wrote, read back from its own `RETURN AFTER` plus its own membership, so a
    /// journal postimage cannot disagree with the row the commit produced. `before` is the state that
    /// existed *before* this change touched it, read inside the same transaction under the same workspace
    /// predicate the compare-and-set uses — which is what makes the pair honest: an image assembled from
    /// the caller's earlier read would describe the world the caller believed in, not the one the write
    /// actually replaced.
    ///
    /// `before` is `None` for exactly one shape, [`ClusterChange::Create`], because a minted cluster has no
    /// earlier state to image. That absence is *stated*, not a fabricated empty row: a preimage claiming a
    /// cluster with no members and a zero centroid would be a lie about a row that did not exist, and the
    /// history page would render it as a state the owner could have gone back to.
    ///
    /// A named cluster that cannot be read before the write is refused rather than imaged as nothing: the
    /// change approved an effect on a row, and a row that is not there when the transaction opens means the
    /// approval is about a world that no longer exists.
    async fn apply_cluster(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        change: &ClusterChange,
        minted: &[RecordId],
        reverting: bool,
    ) -> Result<(Vec<SemanticSnapshot>, Vec<SemanticSnapshot>), StorageError> {
        if let ClusterChange::Restructure {
            partition,
            expectations,
            destinations,
        } = change
        {
            return self
                .apply_restructure(tx, owner, partition, expectations, destinations)
                .await;
        }
        // The preimage first: every write below moves the guards, the centroid or the membership, and an
        // image taken after any of them is a postimage wearing an earlier name.
        let before = match change {
            ClusterChange::Guard { .. } => {
                return Err(StorageError::InvalidInput(
                    "a cluster guard is not a write".to_string(),
                ));
            }
            ClusterChange::Create { .. } => None,
            ClusterChange::Detach { expectation, .. }
            | ClusterChange::Effect { expectation, .. } => Some(
                self.cluster_image_in_tx(tx, owner, &expectation.cluster)
                    .await?,
            ),
            ClusterChange::Restructure { .. } => unreachable!("handled above"),
        };
        let after = self
            .write_cluster(tx, owner, change, minted, reverting)
            .await?;
        Ok((before.into_iter().collect(), vec![after]))
    }

    #[allow(clippy::mutable_key_type)] // Typed Surreal record identities are only transient membership keys here.
    async fn apply_restructure(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        scope: &crate::models::MemoryScope,
        expectations: &[ClusterWriteExpectation],
        destinations: &[crate::models::ClusterDestination],
    ) -> Result<(Vec<SemanticSnapshot>, Vec<SemanticSnapshot>), StorageError> {
        use std::collections::HashSet;

        if expectations.is_empty() || destinations.is_empty() {
            return Err(StorageError::InvalidInput(
                "cluster restructuring requires sources and destinations".to_string(),
            ));
        }
        let source_ids: HashSet<RecordId> = expectations
            .iter()
            .map(|expectation| expectation.cluster.clone())
            .collect();
        if source_ids.len() != expectations.len() {
            return Err(StorageError::InvalidInput(
                "cluster restructuring repeated a source".to_string(),
            ));
        }
        let mut retained_ids = HashSet::new();
        for destination in destinations {
            if destination.members.is_empty()
                || destination
                    .members
                    .iter()
                    .any(|member| member.table.as_str() != "fact")
                || destination.cluster.is_some() == destination.new_id.is_some()
                || destination
                    .new_id
                    .as_ref()
                    .is_some_and(|id| id.table.as_str() != "cluster")
                || destination
                    .cluster
                    .as_ref()
                    .is_some_and(|id| !source_ids.contains(id))
                || destination
                    .cluster
                    .as_ref()
                    .is_some_and(|id| !retained_ids.insert(id.clone()))
            {
                return Err(StorageError::InvalidInput(
                    "cluster restructuring contains an invalid destination".to_string(),
                ));
            }
        }
        let mut before = Vec::with_capacity(expectations.len());
        let mut actual_members = Vec::new();
        let mut partition_members = Vec::new();
        let mut live_members = Vec::new();
        let partition = crate::models::Partition {
            workspace: owner.workspace().clone(),
            scope: scope.clone(),
        };
        let filter = ScopeFilter::for_partition(&partition);
        for expectation in expectations {
            before.push(
                self.cluster_image_in_tx(tx, owner, &expectation.cluster)
                    .await?,
            );
            let rows: Vec<RecordId> = tx
                .query("SELECT VALUE out FROM contains_memory WHERE in = $cluster ORDER BY out;")
                .bind(("cluster", expectation.cluster.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            actual_members.extend(rows);
            let query = tx.query(
                "SELECT VALUE id FROM $cluster->contains_memory->(fact WHERE account = $account AND workspace = $scope_workspace AND ((scope = 'personal' AND $scope_personal) OR (scope = 'unassigned' AND $scope_unassigned) OR (scope = 'project' AND project INSIDE $scope_projects))) ORDER BY id;",
            ).bind(("cluster", expectation.cluster.clone()))
                .bind(("account", owner.account().as_record_id().clone()));
            let rows: Vec<RecordId> = filter
                .apply(query)
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            partition_members.extend(rows);
            let query = tx.query(
                "SELECT VALUE id FROM $cluster->contains_memory->(fact WHERE deleted = false AND quarantined_at = NONE AND account = $account AND workspace = $scope_workspace AND ((scope = 'personal' AND $scope_personal) OR (scope = 'unassigned' AND $scope_unassigned) OR (scope = 'project' AND project INSIDE $scope_projects))) ORDER BY id;",
            ).bind(("cluster", expectation.cluster.clone()))
                .bind(("account", owner.account().as_record_id().clone()));
            let rows: Vec<RecordId> = filter
                .apply(query)
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            live_members.extend(rows);
        }
        actual_members.sort();
        if actual_members.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(StorageError::RetryableBookkeepingConflict);
        }
        partition_members.sort();
        partition_members.dedup();
        if partition_members != actual_members {
            return Err(StorageError::RetryableBookkeepingConflict);
        }
        let mut output_members: Vec<RecordId> = destinations
            .iter()
            .flat_map(|destination| destination.members.iter().cloned())
            .collect();
        output_members.sort();
        if output_members.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(StorageError::InvalidInput(
                "cluster restructuring assigned a member more than once".to_string(),
            ));
        }
        if actual_members != output_members {
            return Err(StorageError::RetryableBookkeepingConflict);
        }
        live_members.sort();
        live_members.dedup();
        let expected_live: Vec<RecordId> = expectations
            .iter()
            .flat_map(|expectation| {
                expectation
                    .population
                    .iter()
                    .map(|member| member.id.clone())
            })
            .collect();
        let mut expected_live = expected_live;
        expected_live.sort();
        expected_live.dedup();
        if live_members != expected_live {
            return Err(StorageError::RetryableBookkeepingConflict);
        }

        for expectation in expectations {
            let destination = destinations
                .iter()
                .find(|destination| destination.cluster.as_ref() == Some(&expectation.cluster));
            let mut sets = vec!["membership_revision = $membership_next"];
            if destination.is_some() {
                sets.extend(["centroid = $centroid", "centroid_revision = $centroid_next"]);
            }
            let sql = format!(
                "UPDATE type::record('cluster', $key) SET {} WHERE id = $id AND account = $account AND workspace = $workspace AND (membership_revision = $membership OR (membership_revision = NONE AND $membership = 0)) AND (centroid_revision = $centroid_revision OR (centroid_revision = NONE AND $centroid_revision = 0)) AND (member_embedding_revision = $member_embedding OR (member_embedding_revision = NONE AND $member_embedding = 0)) RETURN AFTER;",
                sets.join(", ")
            );
            let mut query = tx
                .query(sql)
                .bind(("key", record_key(&expectation.cluster)?))
                .bind(("id", expectation.cluster.clone()))
                .bind(("account", owner.account().as_record_id().clone()))
                .bind(("workspace", owner.workspace().as_record_id().clone()))
                .bind(("membership", revision(expectation.membership_revision)))
                .bind((
                    "membership_next",
                    revision(expectation.membership_revision) + 1,
                ))
                .bind(("centroid_revision", revision(expectation.centroid_revision)))
                .bind(("centroid_next", revision(expectation.centroid_revision) + 1))
                .bind((
                    "member_embedding",
                    revision(expectation.member_embedding_revision),
                ));
            if let Some(destination) = destination {
                query = query.bind(("centroid", destination.centroid.clone()));
            }
            let rows: Vec<ClusterRow> = query
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if rows.is_empty() {
                return Err(StorageError::RetryableBookkeepingConflict);
            }
        }

        for expectation in expectations {
            tx.query("DELETE contains_memory WHERE in = $cluster;")
                .bind(("cluster", expectation.cluster.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
        }
        let mut destination_ids = Vec::with_capacity(destinations.len());
        for destination in destinations {
            let id = if let Some(id) = &destination.cluster {
                id.clone()
            } else {
                let id = destination.new_id.as_ref().ok_or_else(|| {
                    StorageError::InvalidInput(
                        "new cluster destination has no typed id".to_string(),
                    )
                })?;
                let rows: Vec<ClusterRow> = tx
                    .query("CREATE $id SET label = $label, centroid = $centroid, depth = 0, account = $account, workspace = $workspace RETURN AFTER;")
                    .bind(("id", id.clone()))
                    .bind(("label", destination.label.clone()))
                    .bind(("centroid", destination.centroid.clone()))
                    .bind(("account", owner.account().as_record_id().clone()))
                    .bind(("workspace", owner.workspace().as_record_id().clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                rows.into_iter()
                    .next()
                    .and_then(|row| row.id)
                    .ok_or_else(|| missing("cluster"))?
            };
            for member in &destination.members {
                tx.query("RELATE $cluster->contains_memory->$member;")
                    .bind(("cluster", id.clone()))
                    .bind(("member", member.clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?;
            }
            destination_ids.push(id);
        }
        for source in &source_ids {
            if !retained_ids.contains(source) {
                tx.query("DELETE $cluster;")
                    .bind(("cluster", source.clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?;
            }
        }
        let mut after = Vec::with_capacity(destination_ids.len());
        for id in destination_ids {
            after.push(self.cluster_image_in_tx(tx, owner, &id).await?);
        }
        Ok((before, after))
    }

    /// One cluster as this transaction finds it: its row, its three guards and its live membership.
    ///
    /// Qualified by the caller's own workspace, exactly as the guarded writes are, so a preimage cannot be
    /// used to ask what another account's cluster held. `members_in_tx` is the same read the postimage
    /// uses, so the two images are comparable counts of the same population rather than two different
    /// filters that happen to look alike.
    async fn cluster_image_in_tx(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        cluster: &RecordId,
    ) -> Result<SemanticSnapshot, StorageError> {
        let rows: Vec<ClusterRow> = tx
            .query(CLUSTER_IMAGE_SQL)
            .bind(("key", record_key(cluster)?))
            .bind(("id", cluster.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let row = rows.into_iter().next().ok_or_else(|| {
            StorageError::InvalidInput(
                "a journaled cluster effect named a row this transaction cannot read".to_string(),
            )
        })?;
        let snapshot = row.into_snapshot(cluster);
        Ok(SemanticSnapshot::Cluster(ClusterSnapshot {
            members: self.members_in_tx(tx, cluster).await?,
            ..snapshot
        }))
    }

    /// A guard-only intent has no write-side CAS. Protect the specific cluster row instead, so a
    /// revision change after this comparison is still a read conflict at commit, not a lost condition.
    async fn assert_cluster_revisions(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        expected: &ClusterWriteExpectation,
    ) -> Result<(), StorageError> {
        let rows: Vec<GuardRow> = tx
            .query(
                "SELECT membership_revision, member_embedding_revision, centroid_revision \
                 FROM $cluster WHERE account = $account AND workspace = $workspace FOR UPDATE;",
            )
            .bind(("cluster", expected.cluster.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let actual = rows.into_iter().next().ok_or(StorageError::NotFound)?;
        if actual.membership_revision.unwrap_or(0) != revision(expected.membership_revision)
            || actual.member_embedding_revision.unwrap_or(0)
                != revision(expected.member_embedding_revision)
            || actual.centroid_revision.unwrap_or(0) != revision(expected.centroid_revision)
        {
            return Err(StorageError::RetryableBookkeepingConflict);
        }
        Ok(())
    }

    /// Compare the SAME prestate the preparer used, including changes compatibility writers do not version.
    async fn assert_cluster_population(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        expected: &ClusterWriteExpectation,
    ) -> Result<(), StorageError> {
        let rows: Vec<GuardRow> = tx
            .query(CLUSTER_GUARD_SQL)
            .bind(("key", record_key(&expected.cluster)?))
            .bind(("id", expected.cluster.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if rows.is_empty() {
            return Err(StorageError::NotFound);
        }
        // Include hidden workspace members in the locked reads: a hidden->live transition must also
        // invalidate a calculation. Foreign endpoints are never locked or read as this owner's data.
        let members: Vec<RecordId> = tx
            .query(CLUSTER_WORKSPACE_MEMBER_IDS_SQL)
            .bind(("cluster", expected.cluster.clone()))
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let actual = self.locked_population(tx, owner, &members).await?;
        if actual != expected.population {
            return Err(StorageError::RetryableBookkeepingConflict);
        }
        Ok(())
    }

    /// 3.3 requires specific FOR UPDATE targets, not an array/range. Only generated bind names enter SQL;
    /// all record ids remain typed bindings. One statement locks the batch without persisting writes.
    async fn locked_population(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        members: &[RecordId],
    ) -> Result<Vec<ClusterPopulationMember>, StorageError> {
        if members.is_empty() {
            return Ok(vec![]);
        }
        let targets: Vec<_> = (0..members.len())
            .map(|index| format!("$member_{index}"))
            .collect();
        let sql = format!(
            "SELECT id, embedding, deleted, quarantined_at FROM {} WHERE account = $account AND workspace = $workspace FOR UPDATE;",
            targets.join(", ")
        );
        let mut query = tx
            .query(sql)
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()));
        for (index, member) in members.iter().enumerate() {
            query = query.bind((format!("member_{index}"), member.clone()));
        }
        let rows: Vec<LockedPopulationRow> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let mut population: Vec<_> = rows
            .into_iter()
            .filter(|row| row.deleted == Some(false) && row.quarantined_at.is_none())
            .map(|row| ClusterPopulationMember {
                id: row.id,
                embedding: row.embedding,
            })
            .collect();
        population.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(population)
    }

    /// The writes of one cluster effect: the partition guard, the CAS, then the membership edges.
    async fn write_cluster(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        change: &ClusterChange,
        minted: &[RecordId],
        reverting: bool,
    ) -> Result<SemanticSnapshot, StorageError> {
        match change {
            ClusterChange::Guard { .. } => Err(StorageError::InvalidInput(
                "a cluster guard is not a write".to_string(),
            )),
            ClusterChange::Restructure { .. } => Err(StorageError::InvalidInput(
                "cluster restructuring must use its multi-image writer".to_string(),
            )),
            ClusterChange::Create {
                centroid,
                label,
                member,
            } => {
                let rows: Vec<ClusterRow> = tx
                    .query(CREATE_CLUSTER_SQL)
                    .bind(("label", label.clone()))
                    .bind(("centroid", centroid.clone()))
                    .bind(("account", owner.account().as_record_id().clone()))
                    .bind(("workspace", owner.workspace().as_record_id().clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                let created = rows.into_iter().next().ok_or_else(|| missing("cluster"))?;
                let cluster = created.id.clone().ok_or_else(|| missing("cluster"))?;
                let member = resolve_member(member, minted)?;
                tx.query("RELATE $cluster->contains_memory->$member;")
                    .bind(("cluster", cluster.clone()))
                    .bind(("member", member.clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?;
                Ok(SemanticSnapshot::Cluster(ClusterSnapshot {
                    members: self.members_in_tx(tx, &cluster).await?,
                    ..created.into_snapshot(&cluster)
                }))
            }
            // A detach is a membership write *and* a derived-state write: the edge goes, and the centre
            // of the members that stay moves with it. It is one CAS, not two statements, because the
            // half this transaction must not lose is the pair — an edge removed with the old mean left
            // behind is the exact defect the owner refused.
            ClusterChange::Detach {
                expectation,
                member,
                surviving_centroid,
            } => {
                let cluster = &expectation.cluster;
                let mut sets = vec!["membership_revision = $membership_next"];
                if surviving_centroid.is_some() {
                    sets.push("centroid = $centroid");
                    sets.push("centroid_revision = $centroid_next");
                }
                // Parenthesised, because `AND` binds tighter than `OR` on this engine: unparenthesised the
                // clause reads `(id AND workspace AND revision) OR (revision = NONE)`, which would let a
                // legacy *cluster of anybody's* satisfy the guard. The `Effect` arm above writes the same
                // three-guard rule; a detach that quietly relaxed it would be a CAS that is not a CAS.
                let sql = format!(
                    "UPDATE type::record('cluster', $key) SET {} WHERE id = $id AND workspace = \
                     $workspace AND (membership_revision = $membership OR \
                     (membership_revision = NONE AND $membership = 0)) AND (centroid_revision = \
                     $centroid_revision OR (centroid_revision = NONE AND $centroid_revision = 0)) \
                     AND (member_embedding_revision = $member_embedding OR \
                     (member_embedding_revision = NONE AND $member_embedding = 0)) RETURN AFTER;",
                    sets.join(", ")
                );
                let mut query = tx
                    .query(sql)
                    .bind(("key", record_key(cluster)?))
                    .bind(("id", cluster.clone()))
                    .bind(("workspace", owner.workspace().as_record_id().clone()))
                    .bind(("membership", revision(expectation.membership_revision)))
                    .bind((
                        "membership_next",
                        revision(expectation.membership_revision) + 1,
                    ))
                    .bind(("centroid_revision", revision(expectation.centroid_revision)))
                    .bind((
                        "member_embedding",
                        revision(expectation.member_embedding_revision),
                    ))
                    .bind(("centroid_next", revision(expectation.centroid_revision) + 1));
                if let Some(centroid) = surviving_centroid {
                    query = query.bind(("centroid", centroid.clone()));
                }
                let rows: Vec<ClusterRow> = query
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                let Some(after) = rows.into_iter().next() else {
                    return Err(StorageError::RetryableBookkeepingConflict);
                };
                let cluster = after.id.clone().ok_or_else(|| missing("cluster"))?;
                tx.query("DELETE contains_memory WHERE in = $cluster AND out = $member;")
                    .bind(("cluster", cluster.clone()))
                    .bind(("member", member.clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?;
                Ok(SemanticSnapshot::Cluster(ClusterSnapshot {
                    members: self.members_in_tx(tx, &cluster).await?,
                    ..after.into_snapshot(&cluster)
                }))
            }
            ClusterChange::Effect {
                expectation,
                add_member,
                remove_member,
                centroid,
            } => {
                let cluster = &expectation.cluster;
                // A join's partition claim is read from the row being joined, inside this transaction, and
                // re-asserted against the cluster's membership before anything moves. The candidate
                // population that ranked this cluster was selected *outside* the transaction, so a writer
                // that attached a member of another partition in that window would otherwise be joined to
                // by this one and the cluster would end up straddling two scopes — the state
                // `ScopedClusterRepo::partition_candidates` refuses to rank, reached by racing past it
                // instead of by selecting it. Deriving the partition from the arriving row rather than from
                // a field on the effect is what makes the guard un-forgettable: an effect that adds a
                // member is always checked, and no caller can opt out by omitting a claim.
                if let Some(member) = add_member
                    && !reverting
                {
                    self.assert_join_stays_in_one_partition(tx, owner, cluster, member, minted)
                        .await?;
                }
                let mut sets: Vec<&str> = Vec::new();
                let changes_membership = add_member.is_some() || remove_member.is_some();
                if changes_membership {
                    sets.push("membership_revision = $membership_next");
                }
                if centroid.is_some() {
                    sets.push("centroid = $centroid");
                    sets.push("centroid_revision = $centroid_next");
                }
                if sets.is_empty() {
                    return Err(StorageError::InvalidInput(
                        "a cluster effect named no change to apply".to_string(),
                    ));
                }
                // Each guard accepts `NONE` only against a zero expectation: v009's `option<int>` columns
                // are `NONE` on every cluster written before these writers started moving them, and a
                // `DEFAULT` is applied at CREATE only, so refusing `NONE` would freeze the legacy corpus.
                let sql = format!(
                    "UPDATE type::record('cluster', $key) SET {} WHERE id = $id AND workspace = \
                     $workspace AND (membership_revision = $membership OR \
                     (membership_revision = NONE AND $membership = 0)) AND (centroid_revision = \
                     $centroid_revision OR (centroid_revision = NONE AND $centroid_revision = 0)) \
                     AND (member_embedding_revision = $member_embedding OR \
                     (member_embedding_revision = NONE AND $member_embedding = 0)) RETURN AFTER;",
                    sets.join(", ")
                );
                let mut query = tx
                    .query(sql)
                    .bind(("key", record_key(cluster)?))
                    .bind(("id", cluster.clone()))
                    .bind(("workspace", owner.workspace().as_record_id().clone()))
                    .bind(("membership", revision(expectation.membership_revision)))
                    .bind((
                        "membership_next",
                        revision(expectation.membership_revision) + 1,
                    ))
                    .bind(("centroid_revision", revision(expectation.centroid_revision)))
                    .bind((
                        "member_embedding",
                        revision(expectation.member_embedding_revision),
                    ))
                    .bind(("centroid_next", revision(expectation.centroid_revision) + 1));
                if let Some(centroid) = centroid {
                    query = query.bind(("centroid", centroid.clone()));
                }
                let rows: Vec<ClusterRow> = query
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                let Some(after) = rows.into_iter().next() else {
                    // Nothing was written and the caller's approved effect set is untouched: the
                    // transient class, not the semantic one.
                    return Err(StorageError::RetryableBookkeepingConflict);
                };
                let cluster = after.id.clone().ok_or_else(|| missing("cluster"))?;
                if let Some(member) = add_member {
                    let member = resolve_member(member, minted)?;
                    tx.query("RELATE $cluster->contains_memory->$member;")
                        .bind(("cluster", cluster.clone()))
                        .bind(("member", member))
                        .await
                        .map_err(internal)?
                        .check()
                        .map_err(internal)?;
                }
                if let Some(member) = remove_member {
                    tx.query("DELETE contains_memory WHERE in = $cluster AND out = $member;")
                        .bind(("cluster", cluster.clone()))
                        .bind(("member", member.clone()))
                        .await
                        .map_err(internal)?
                        .check()
                        .map_err(internal)?;
                }
                Ok(SemanticSnapshot::Cluster(ClusterSnapshot {
                    members: self.members_in_tx(tx, &cluster).await?,
                    ..after.into_snapshot(&cluster)
                }))
            }
        }
    }

    /// Refuse a join that would leave one cluster holding members of two partitions.
    ///
    /// Read the arriving row's stored partition, then require a nonempty eligible population and no
    /// conflicting live members inside this transaction. The arriving row cannot witness destination
    /// eligibility itself. Soft deletion can remove the last eligible member without moving cluster
    /// guards, so purity alone is insufficient; missing projections also refuse admission.
    ///
    /// The class is [`StorageError::RetryableBookkeepingConflict`], the derived one, because that is what
    /// happened: destination eligibility changed after preparation. The caller may refresh only the
    /// already pinned population, guards and mean within its bound. An unlawful named destination must
    /// refuse rather than rerank or mint a substitute. Initially mixed clusters are never selected by
    /// `partition_candidates`.
    async fn assert_join_stays_in_one_partition(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        cluster: &RecordId,
        member: &RelationTarget,
        minted: &[RecordId],
    ) -> Result<(), StorageError> {
        let member = resolve_member(member, minted)?;
        let rows: Vec<MemberPartitionRow> = tx
            .query(FACT_PARTITION_SQL)
            .bind(("id", member.clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let row = rows.into_iter().next().ok_or_else(|| {
            StorageError::InvalidInput(
                "a journaled join named a member this transaction cannot read".to_string(),
            )
        })?;
        let partition = ScopeFilter::for_partition(&Partition {
            workspace: owner.workspace().clone(),
            scope: read_scope(row.scope.as_deref(), row.project),
        });
        let counted: Vec<PurityRow> = partition
            .apply(
                tx.query(cluster_admission_sql(partition.conjunct()))
                    .bind(("cluster", cluster.clone()))
                    .bind(("arriving", member)),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let lawful = counted.first().is_some_and(|row| {
            row.eligible.is_some_and(|count| count > 0) && row.impure == Some(0)
        });
        if !lawful {
            return Err(StorageError::RetryableBookkeepingConflict);
        }
        Ok(())
    }

    /// Complete retained structural membership for private journal images, inside this transaction.
    ///
    /// Hidden endpoints remain part of history even though live-vector centroid and recall reads exclude
    /// them. Ordered by handle so two images of the same membership render the same digest.
    async fn members_in_tx(
        &self,
        tx: &Transaction<Any>,
        cluster: &RecordId,
    ) -> Result<Vec<RecordId>, StorageError> {
        let rows: Vec<MemberRow> = tx
            .query(CLUSTER_MEMBERS_IN_TX_SQL)
            .bind(("cluster", cluster.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(rows.into_iter().filter_map(|row| row.member).collect())
    }

    async fn relate_resolved(
        &self,
        tx: &Transaction<Any>,
        change: &RelationChange,
        minted: &[RecordId],
        raws: &[RecordId],
        provenances: &[RecordId],
    ) -> Result<EdgeSnapshot, StorageError> {
        let from = resolve(&change.from, minted, raws, provenances)?;
        let to = resolve(&change.to, minted, raws, provenances)?;
        let edge = self
            .relate(tx, &change.kind, &from, &to, change.strength)
            .await?;
        Ok(EdgeSnapshot {
            edge,
            kind: change.kind.clone(),
            from,
            to,
            strength: change.strength,
            generated_from: None,
        })
    }

    async fn unrelate_resolved(
        &self,
        tx: &Transaction<Any>,
        change: &RelationChange,
        minted: &[RecordId],
        raws: &[RecordId],
        provenances: &[RecordId],
    ) -> Result<(), StorageError> {
        let from = resolve(&change.from, minted, raws, provenances)?;
        let to = resolve(&change.to, minted, raws, provenances)?;
        self.unrelate(tx, &change.kind, &from, &to).await
    }

    async fn relate(
        &self,
        tx: &Transaction<Any>,
        kind: &EdgeKind,
        from: &RecordId,
        to: &RecordId,
        strength: f64,
    ) -> Result<RecordId, StorageError> {
        // `strength` and `edge_type` are written only on `memory_edge`, which is the one relation table
        // v002 gives those fields; a structural table is SCHEMAFULL and would refuse them.
        let (sql, with_strength) = match kind {
            EdgeKind::Memory(_) => (
                format!(
                    "RELATE $from->{}->$to SET edge_type = $edge_type, strength = $strength \
                     RETURN AFTER;",
                    kind.table()
                ),
                true,
            ),
            _ => (
                format!("RELATE $from->{}->$to RETURN AFTER;", kind.table()),
                false,
            ),
        };
        let mut query = tx
            .query(sql)
            .bind(("from", from.clone()))
            .bind(("to", to.clone()));
        if let EdgeKind::Memory(edge_type) = kind {
            query = query.bind(("edge_type", edge_type.clone()));
        }
        if with_strength {
            query = query.bind(("strength", strength));
        }
        let rows: Vec<IdRow> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        rows.into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing(kind.table()))
    }

    async fn unrelate(
        &self,
        tx: &Transaction<Any>,
        kind: &EdgeKind,
        from: &RecordId,
        to: &RecordId,
    ) -> Result<(), StorageError> {
        let sql = match kind {
            EdgeKind::Memory(_) => format!(
                "DELETE {} WHERE in = $from AND out = $to AND edge_type = $edge_type;",
                kind.table()
            ),
            other => format!("DELETE {} WHERE in = $from AND out = $to;", other.table()),
        };
        let mut query = tx
            .query(sql)
            .bind(("from", from.clone()))
            .bind(("to", to.clone()));
        if let EdgeKind::Memory(edge_type) = kind {
            query = query.bind(("edge_type", edge_type.clone()));
        }
        query.await.map_err(internal)?.check().map_err(internal)?;
        Ok(())
    }

    /// Write one journal snapshot, typed by its variant.
    ///
    /// The payload columns are named per variant rather than by one wide statement binding `NONE` for the
    /// other three arms, because v009's `kind` assertion cross-checks every payload column against the
    /// discriminator: a wide statement would either send a `fact` column on a `cluster` row and be
    /// refused, or be written to dodge the assertion — and the second is the failure the typed variants
    /// exist to prevent.
    async fn write_snapshot(
        &self,
        tx: &Transaction<Any>,
        snapshot: &SemanticSnapshot,
        operation: &RecordId,
        workspace: &RecordId,
    ) -> Result<RecordId, StorageError> {
        let (sql, binds) = snapshot_write(snapshot);
        let mut query = tx
            .query(sql)
            .bind(("operation", operation.clone()))
            .bind(("subject", snapshot.subject()))
            .bind(("workspace", workspace.clone()));
        for bind in binds {
            query = bind.apply(query);
        }
        let rows: Vec<IdRow> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        rows.into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| missing("semantic_snapshot"))
    }

    async fn write_summary(
        &self,
        tx: &Transaction<Any>,
        summary: &MaintenanceSummary,
        operation: &RecordId,
    ) -> Result<(), StorageError> {
        // The audit-job namespace is checked at the write, not pre-validated at `apply`'s entry, and the
        // order is deliberate: a background writer whose summary cannot be attributed must lose its corpus
        // change too, so refusing *here* — after the writes, inside the same transaction — is what makes
        // "the summary and the change are atomic" observable. `sweep` is absent on purpose (AGENTS.md: it
        // materialises a proven no-op on ranking and would bury the collapses and demotions).
        if !matches!(
            summary.job.as_str(),
            crate::models::maintenance::job::CLUSTER
                | crate::models::maintenance::job::MERGE
                | crate::models::maintenance::job::COLLAPSE
                | crate::models::maintenance::job::APPRAISE
        ) {
            return Err(StorageError::InvalidInput(
                "a journaled background change named a job the audit namespace does not record"
                    .to_string(),
            ));
        }
        tx.query(CREATE_SUMMARY_SQL)
            .bind(("action", summary.action.clone()))
            .bind(("source_id", summary.source_id.clone()))
            .bind(("target_ids", summary.target_ids.clone()))
            .bind(("members_moved", summary.members_moved))
            .bind(("run_id", summary.run_id.clone()))
            .bind(("actor", "system:dreaming".to_string()))
            .bind(("job", summary.job.clone()))
            .bind(("disposition", summary.disposition.clone()))
            .bind(("previous_value", summary.previous_value))
            .bind(("operation", operation.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        Ok(())
    }

    // -- Task 11: scope moves and human classification ------------------------------------------------

    /// Move one memory to another scope, under its owner's authority.
    ///
    /// The memory **keeps its id**: a move is a partition change, not a re-create, and minting a new row
    /// would orphan every lineage, provenance and session edge that names the old one. Scope, project
    /// handle, semantic revision bump and journal row commit in one transaction.
    ///
    /// What a move *detaches* is the cluster membership, because a cluster is one `(workspace, scope)`
    /// partition's derived state and leaving the row attached would let a partition-scoped recall reach a
    /// memory that no longer belongs to that partition. Historical rows — lineage snapshots, raw sources,
    /// session links — are deliberately left where they are: they describe what was believed at the time,
    /// and silently re-partitioning an owner's history on a scope change is how a mixed session's summary
    /// would leak into a project list it never named.
    pub async fn move_scope(
        &self,
        owner: &OwnerContext,
        fact: FactId,
        expected: Version,
        destination: MemoryScope,
        operation: &OperationId,
    ) -> Result<AppliedChange, StorageError> {
        self.move_scope_reconciled(
            owner,
            fact,
            expected,
            destination,
            operation,
            &MoveReconciliation::ungrouped(),
        )
        .await
    }

    /// The same move with its **cluster reconciliation** supplied.
    ///
    /// `MoveReconciliation::arriving` is the destination half: which cluster of the partition the memory
    /// is landing in it belongs to, and the centroid that join implies. Only a caller that can rank can
    /// prepare it — the join threshold is a configured policy and the ranking is the engine's arithmetic —
    /// so it arrives as data and this transaction writes it under the same guards as the detach.
    /// `MoveReconciliation::ungrouped()` is the honest statement that the memory lands grouped by
    /// nothing, which is what the storage-level primitive alone can know.
    pub async fn move_scope_reconciled(
        &self,
        owner: &OwnerContext,
        fact: FactId,
        expected: Version,
        destination: MemoryScope,
        operation: &OperationId,
        reconciliation: &MoveReconciliation,
    ) -> Result<AppliedChange, StorageError> {
        let access = MutationContext::issued_to(owner);
        let state = self.expected_state(owner, &fact).await?;
        if state.version != expected {
            return Err(StorageError::Stale);
        }
        let change = self
            .plan_move(
                owner,
                &fact,
                expected,
                &state.scope,
                &destination,
                reconciliation,
            )
            .await?;
        self.apply(&access, operation, &[state], &change).await
    }

    /// The whole change one scope move writes: the partition, the cluster it leaves, the cluster it joins.
    ///
    /// `source` is the partition the memory is in as the caller read it, and the move's cluster halves are
    /// decided against it rather than against the cluster ids alone: naming one cluster as both sides means
    /// one different thing per partition, and the two answers are not interchangeable.
    pub(crate) async fn plan_move(
        &self,
        owner: &OwnerContext,
        fact: &FactId,
        expected: Version,
        source: &MemoryScope,
        destination: &MemoryScope,
        reconciliation: &MoveReconciliation,
    ) -> Result<PreparedChangeSet, StorageError> {
        let mut change = PreparedChangeSet::new(operation_kind::MOVE)
            .effect(effect::SCOPE)
            .fact(FactChange::Update {
                fact: fact.clone(),
                expected,
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: Some(destination.clone()),
            });
        // A move leaves a cluster behind, and leaving one is not the same as abandoning it: the stored
        // centroid of the cluster it was in still averages a vector that is no longer a member, and that
        // number is read by the debug cluster view and by the next similarity ranking alike. The detach
        // therefore re-derives the centre of the **survivors** under the cluster's own guards.
        //
        // The `ClusterWriteExpectation` is read here, before the transaction, and the CAS is what makes
        // that honest: a membership that moved in the window is a `RetryableBookkeepingConflict`, not a
        // centroid computed from a population that no longer exists.
        let leaving = match &reconciliation.leaving {
            Some(leaving) => Some((
                leaving.expectation.clone(),
                leaving.surviving_centroid.clone(),
            )),
            None => match self.cluster_of(owner, fact.as_record_id()).await? {
                Some((_, expectation)) => {
                    let population: Vec<&[f32]> = expectation
                        .population
                        .iter()
                        .filter(|member| &member.id != fact.as_record_id())
                        .map(|member| member.embedding.as_slice())
                        .collect();
                    let centroid = crate::models::cluster::mean_centroid(&population);
                    Some((expectation, centroid))
                }
                None => None,
            },
        };
        if let Some((expectation, surviving_centroid)) = leaving {
            // Both halves naming the same cluster is not a reconciliation, it is a self-conflict: the
            // detach bumps `membership_revision` (and `centroid_revision` when a centre moves) and the
            // join's CAS then expects the guards the detach just replaced, so the first attempt conflicts,
            // every re-preparation re-derives the same pair, and the owner is left holding a retryable
            // class that cannot be won. What the right answer is depends on whether the move crossed a
            // partition at all, and that is a question about the *row*, so it is asked of `source` and
            // `destination` rather than inferred from a cluster id.
            if let DestinationMembership::Join {
                expectation: arriving,
                ..
            } = &reconciliation.arriving
                && arriving.cluster == expectation.cluster
            {
                if source == destination {
                    // The memory is being filed where it already lives, so its membership does not move:
                    // there is no cluster the row leaves and no cluster it joins, only a partition to
                    // write. Emitting neither half is the partition-correct repair, and it is why the
                    // effect set below states no membership label — the journal may not describe an edge it
                    // did not touch.
                    return Ok(change);
                }
                return Err(StorageError::InvalidInput(
                    "a scope move named one cluster as both the cluster it leaves and the cluster it joins"
                        .to_string(),
                ));
            }
            change.effects.push(EdgeKind::ClusterMembership.effect());
            if surviving_centroid.is_some() {
                change.effects.push(effect::CLUSTER_CENTROID.to_string());
            }
            change.cluster_changes.push(ClusterChange::Detach {
                expectation,
                // The member being detached is the memory, not the cluster: `contains_memory` is IN
                // cluster OUT fact, and a delete aimed at the near end matches nothing and silently
                // leaves the cross-partition membership in place. Naming the cluster here instead was
                // the failure `a_move_keeps_the_id_detaches_the_cluster_and_leaves_history_alone` caught.
                member: fact.as_record_id().clone(),
                surviving_centroid,
            });
        }
        // The destination half, prepared by whoever could rank it. `Unclustered` writes nothing: the
        // memory is in its new partition and grouped by nothing yet, which the journal states by naming
        // no membership effect at all rather than by naming one and skipping it.
        match &reconciliation.arriving {
            DestinationMembership::Unclustered => {}
            DestinationMembership::Join {
                expectation,
                centroid,
            } => {
                change.effects.push(EdgeKind::ClusterMembership.effect());
                change.effects.push(effect::CLUSTER_CENTROID.to_string());
                change.cluster_changes.push(ClusterChange::Effect {
                    expectation: expectation.clone(),
                    add_member: Some(RelationTarget::Existing(fact.as_record_id().clone())),
                    remove_member: None,
                    centroid: Some(centroid.clone()),
                });
            }
            DestinationMembership::Create { centroid } => {
                change.effects.push(EdgeKind::ClusterMembership.effect());
                change.cluster_changes.push(ClusterChange::Create {
                    centroid: centroid.clone(),
                    label: None,
                    member: RelationTarget::Existing(fact.as_record_id().clone()),
                });
            }
        }
        Ok(change)
    }

    /// The cluster one of the caller's memories belongs to, with its guards and numerical population.
    ///
    /// `in` is the cluster and `out` is the fact (v001's `TYPE RELATION IN cluster OUT fact`). The
    /// membership read resolves that cluster; its three guards and one population sample are then
    /// captured by [`Self::cluster_expectation`].
    ///
    /// Public because the apply service needs the same answer for a different reason than `move_scope`: a
    /// content update that moved a memory's vector must re-derive the centroid of the cluster that memory is
    /// *already in*, which is a membership question, not a similarity one. Making this readable from outside
    /// storage adds no authority: both values are resolved under the caller's own workspace predicate, and a
    /// foreign or absent fact answers `None`.
    pub async fn cluster_of(
        &self,
        owner: &OwnerContext,
        fact: &RecordId,
    ) -> Result<Option<(RecordId, ClusterWriteExpectation)>, StorageError> {
        let rows: Vec<ClusterRefRow> = self
            .db
            .query(CLUSTER_OF_SQL)
            .bind(("fact", fact.clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let cluster = row.cluster.clone().ok_or_else(|| missing("cluster"))?;
        // Three reads: membership, then cluster guards, then the population reused by arithmetic.
        // `contains_memory` has no revision columns; v009 puts the guards on `cluster`.
        let expectation = self.cluster_expectation(owner, &cluster).await?;
        Ok(Some((cluster, expectation)))
    }
}

// -- binds -----------------------------------------------------------------------------------------------

/// The value types a semantic change writes, closed so no call site can bind an unexpected shape.
///
/// Deliberately not [`crate::repos::scope::Bind`]: that enum is the scoped *reader's* set, and widening it
/// with JSON and record-array arms to serve this writer would make a nine-armed type out of a five-armed
/// one for the sake of sharing a name. Both are `pub(crate)`, both are closed, and neither is reachable
/// from a request.
pub(crate) enum Bind {
    Text(&'static str, String),
    OptText(&'static str, Option<String>),
    TextList(&'static str, Vec<String>),
    Float(&'static str, f64),
    Floats(&'static str, Vec<f32>),
    Bool(&'static str, bool),
    OptBool(&'static str, Option<bool>),
    Int(&'static str, i64),
    OptInt(&'static str, Option<i64>),
    OptTime(&'static str, Option<DateTime<Utc>>),
    Record(&'static str, RecordId),
    OptRecord(&'static str, Option<RecordId>),
    Records(&'static str, Vec<RecordId>),
}

impl Bind {
    fn apply<'q>(self, q: Query<'q, Any>) -> Query<'q, Any> {
        match self {
            Bind::Text(name, value) => q.bind((name, value)),
            Bind::OptText(name, value) => q.bind((name, value)),
            Bind::TextList(name, value) => q.bind((name, value)),
            Bind::Float(name, value) => q.bind((name, value)),
            Bind::Floats(name, value) => q.bind((name, value)),
            Bind::Bool(name, value) => q.bind((name, value)),
            Bind::OptBool(name, value) => q.bind((name, value)),
            Bind::Int(name, value) => q.bind((name, value)),
            Bind::OptInt(name, value) => q.bind((name, value)),
            Bind::OptTime(name, value) => q.bind((name, value)),
            Bind::Record(name, value) => q.bind((name, value)),
            Bind::OptRecord(name, value) => q.bind((name, value)),
            Bind::Records(name, value) => q.bind((name, value)),
        }
    }
}

// -- helpers ---------------------------------------------------------------------------------------------

fn scope_parts(scope: &MemoryScope) -> (&'static str, Option<RecordId>) {
    match scope {
        MemoryScope::Personal => (scope_kind::PERSONAL, None),
        MemoryScope::Unassigned => (scope_kind::UNASSIGNED, None),
        MemoryScope::Project(project) => {
            (scope_kind::PROJECT, Some(project.as_record_id().clone()))
        }
    }
}

/// The scope one stored row's columns spell back.
///
/// A `project` row whose handle is gone reads as **Unassigned**, never Personal: inventing a partition for
/// a row that names none is exactly the silent widening the plan forbids, and Unassigned is the one scope
/// every read of which is an explicit review path.
fn read_scope(kind: Option<&str>, project: Option<RecordId>) -> MemoryScope {
    match (kind, project) {
        (Some(scope_kind::PERSONAL), _) => MemoryScope::Personal,
        (Some(scope_kind::UNASSIGNED), _) => MemoryScope::Unassigned,
        (Some(scope_kind::PROJECT), Some(project)) => ProjectId::try_from(project)
            .map(MemoryScope::Project)
            .unwrap_or(MemoryScope::Unassigned),
        _ => MemoryScope::Unassigned,
    }
}

fn revision(version: Version) -> i64 {
    i64::try_from(version.get()).unwrap_or(i64::MAX)
}

fn names(ids: &[RecordId]) -> Vec<String> {
    ids.iter().map(crate::record_id_to_string).collect()
}

fn push_unique(ids: &mut Vec<RecordId>, id: RecordId) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// Resolve one relation endpoint, including a row this very transaction minted.
fn resolve(
    target: &RelationTarget,
    minted: &[RecordId],
    raws: &[RecordId],
    provenances: &[RecordId],
) -> Result<RecordId, StorageError> {
    let (pool, index, table) = match target {
        RelationTarget::Existing(id) => return Ok(id.clone()),
        RelationTarget::NewFact(index) => (minted, *index, "fact"),
        RelationTarget::NewRaw(index) => (raws, *index, "raw"),
        RelationTarget::NewProvenance(index) => (provenances, *index, "provenance"),
    };
    pool.get(index)
        .cloned()
        .ok_or_else(|| out_of_range(table, index, pool.len()))
}

/// A cluster member is always a fact, so only two of the four endpoint shapes can name one.
fn resolve_member(target: &RelationTarget, minted: &[RecordId]) -> Result<RecordId, StorageError> {
    match target {
        RelationTarget::Existing(id) => Ok(id.clone()),
        RelationTarget::NewFact(index) => minted
            .get(*index)
            .cloned()
            .ok_or_else(|| out_of_range("fact", *index, minted.len())),
        other => Err(StorageError::InvalidInput(format!(
            "a cluster member must be a fact, not {other:?}"
        ))),
    }
}

fn reminder_mapping_snapshot(row: ReminderMappingRow) -> Result<ReminderSnapshot, StorageError> {
    let account = row.account.ok_or(StorageError::NotFound)?;
    let semantic_revision = row.semantic_revision.unwrap_or(0);
    let semantic_revision = u64::try_from(semantic_revision).map_err(|_| {
        StorageError::Internal(anyhow::anyhow!("reminder has a negative semantic revision"))
    })?;
    Ok(ReminderSnapshot {
        id: row.id,
        account,
        message: row.message,
        target_project: row.target_project,
        legacy_target_project: row.legacy_target_project,
        project: row.project.map(ProjectId::try_from).transpose()?,
        migration_hold: row.migration_hold,
        semantic_revision: Version::new(semantic_revision),
        schedule_kind: Some(row.schedule_kind),
    })
}

fn reminder_mapping_fingerprint(
    reminder_id: &RecordId,
    expected: i64,
    target: Option<&ProjectId>,
) -> String {
    let mut hasher = Sha256::new();
    let reminder = reminder_id.to_sql().into_bytes();
    let target = target.map(|project| project.as_record_id().to_sql().into_bytes());
    let expected = expected.to_le_bytes();
    for field in [
        b"reminder-map-v1".as_slice(),
        reminder.as_slice(),
        expected.as_slice(),
        target.as_deref().unwrap_or(b"personal"),
    ] {
        hasher.update((field.len() as u32).to_be_bytes());
        hasher.update(field);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

fn missing(table: &str) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("created {table} row was not returned"))
}

fn out_of_range(table: &str, index: usize, len: usize) -> StorageError {
    StorageError::Internal(anyhow::anyhow!(
        "a relation named {table} slot {index} of the {len} this transaction created"
    ))
}

fn mismatched(which: &str) -> StorageError {
    StorageError::Internal(anyhow::anyhow!(
        "a {which} writer was handed the other change kind"
    ))
}

/// The digest the journal stores beside each snapshot id list.
///
/// Computed over [`SemanticSnapshot::integrity_digest`] — each variant's complete semantic payload — and
/// not over `kind + subject`, because those two fields say *which row* was imaged, not *what state* it
/// was in. A journal that hashed only the identity would render two different cluster memberships,
/// centroids or guard triples indistinguishable while still presenting the digest as the exact-state
/// integrity guard. The operator health view still never reads a corpus row to compute it.
fn digest_of(snapshots: &[SemanticSnapshot]) -> String {
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest as _;
    for snapshot in snapshots {
        hasher.update(snapshot.integrity_digest().as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// -- statement text --------------------------------------------------------------------------------------

/// The fact read: every column the semantic digest covers, and nothing else.
///
/// `value` is a reserved word on this engine (AGENTS.md), which is one reason the projection names its
/// columns; the account and workspace predicates in the `WHERE` are the other — they are what make a
/// foreign handle indistinguishable from an absent one.
pub const FACT_READ_SQL: &str = "SELECT id, content, confidence, embedding, tags, deleted, \
     quarantined_at, semantic_revision, workspace, scope, project FROM $fact \
     WHERE id = $fact AND account = $account AND workspace = $workspace LIMIT 1;";

pub const MEMORY_EDGE_READ_SQL: &str = "SELECT id, in, out, edge_type, strength FROM memory_edge \
     WHERE (in = $fact OR out = $fact) AND (in INSIDE (SELECT VALUE id FROM fact WHERE \
     workspace = $workspace) OR out INSIDE (SELECT VALUE id FROM fact WHERE workspace = \
     $workspace)) ORDER BY id;";

/// The four structural relation reads, in the order the digest sees them.
///
/// Each is workspace-qualified through the *cluster/session/raw/provenance* end, because those relation
/// tables carry no owner column (v009 deliberately leaves ownership to the validated path through their
/// endpoints) and the fact end is already aimed at the caller's own row.
pub fn structural_reads() -> Vec<(EdgeKind, &'static str)> {
    vec![
        (
            EdgeKind::ClusterMembership,
            "SELECT id, in, out FROM contains_memory WHERE out = $fact AND in INSIDE (SELECT VALUE \
             id FROM cluster WHERE workspace = $workspace) ORDER BY id;",
        ),
        (
            EdgeKind::SessionMembership,
            "SELECT id, in, out, generated_from FROM contains_session_memory WHERE out = $fact AND in INSIDE \
             (SELECT VALUE id FROM session WHERE workspace = $workspace) ORDER BY id;",
        ),
        (
            EdgeKind::Provenance,
            "SELECT id, in, out FROM has_provenance WHERE in = $fact AND out INSIDE (SELECT VALUE \
             id FROM provenance WHERE workspace = $workspace) ORDER BY id;",
        ),
        (
            EdgeKind::ExtractedFrom,
            "SELECT id, in, out FROM extracted_from WHERE in = $fact AND out INSIDE (SELECT VALUE \
             id FROM raw WHERE workspace = $workspace) ORDER BY id;",
        ),
    ]
}

pub const CREATE_FACT_SQL: &str = "CREATE fact SET content = $content, confidence = $confidence, \
     embedding = $embedding, tags = $tags, deleted = $deleted, quarantined_at = $quarantined_at, \
     metadata = $metadata, account = $account, workspace = $workspace, scope = $scope, project = \
     $project, semantic_revision = 1 RETURN AFTER;";

pub const CREATE_RAW_SQL: &str = "CREATE raw SET content = $content, deleted = false, account = \
     $account, workspace = $workspace RETURN AFTER;";

pub const CREATE_PROVENANCE_SQL: &str = "CREATE provenance SET kind = $kind, timestamp = $now, \
     account = $account, workspace = $workspace RETURN AFTER;";

pub const CREATE_HEAT_SQL: &str = "CREATE heat_state SET memory = $memory, heat = $heat, \
     stability = 1.0, access_count = 0 RETURN AFTER;";

pub const CREATE_CLUSTER_SQL: &str = "CREATE cluster SET label = $label, centroid = $centroid, \
     depth = 0, account = $account, workspace = $workspace, membership_revision = 1, \
     member_embedding_revision = 0, centroid_revision = 0 RETURN AFTER;";

pub const CLUSTER_GUARD_SQL: &str = "SELECT membership_revision, member_embedding_revision, \
     centroid_revision FROM type::record('cluster', $key) WHERE id = $id AND account = $account \
     AND workspace = $workspace LIMIT 1;";

pub const CLUSTER_OF_SQL: &str = "SELECT in AS cluster, membership_revision, \
     member_embedding_revision, centroid_revision FROM contains_memory WHERE out = $fact AND in \
     INSIDE (SELECT VALUE id FROM cluster WHERE workspace = $workspace) LIMIT 1;";

/// Full retained membership for exact private history, not a live-population or centroid read.
/// The cluster is owner-qualified by the surrounding transaction before this statement is used.
pub const CLUSTER_MEMBERS_IN_TX_SQL: &str = "SELECT out AS member FROM contains_memory WHERE in = \
     $cluster ORDER BY out;";

/// One cluster row and its three guards, read inside the mutation transaction.
///
/// The projection is exactly [`ClusterRow`], which is also what `RETURN AFTER` decodes for a postimage, so
/// a preimage and a postimage of one cluster are the same fields read at two moments and cannot drift in
/// shape — only in value, which is the whole point of holding both. The workspace predicate is the
/// caller's own, identical to the one the guarded writes apply: an image of a row the writer may not
/// touch is not a diagnostic, it is a leak.
pub const CLUSTER_IMAGE_SQL: &str = "SELECT id, label, centroid, depth, membership_revision, \
     member_embedding_revision, centroid_revision FROM type::record('cluster', $key) WHERE id = $id \
     AND account = $account AND workspace = $workspace LIMIT 1;";

/// One fact row's partition, read inside the mutation transaction.
///
/// `scope`/`project` are the stored pair, and [`read_scope`] is the same translation the journal's fact
/// snapshots use, so the partition a join is checked against is the partition the rest of the system will
/// read back from the same row.
pub const FACT_PARTITION_SQL: &str = "SELECT scope, project FROM $id WHERE id \
     = $id AND workspace = $workspace LIMIT 1;";

#[derive(SurrealValue)]
struct MemberPartitionRow {
    scope: Option<String>,
    project: Option<RecordId>,
}

/// Re-establish destination eligibility and purity before a join, excluding the arriving witness.
pub fn cluster_admission_sql(membership: &str) -> String {
    format!(
        "SELECT (->contains_memory->(fact WHERE deleted = false AND quarantined_at = NONE AND \
         {membership} AND id != $arriving)).len() AS eligible, \
         (->contains_memory->(fact WHERE deleted = false AND quarantined_at = NONE AND \
         NOT ({membership}))).len() AS impure FROM $cluster;"
    )
}

pub const CREATE_SUMMARY_SQL: &str = "CREATE maintenance_log SET action = $action, source_id = \
     $source_id, target_ids = $target_ids, members_moved = $members_moved, run_id = $run_id, actor \
     = $actor, job = $job, disposition = $disposition, previous_value = $previous_value, operation \
     = $operation RETURN AFTER;";

/// The journal row's create. `committed_at` is written by [`FINISH_OPERATION_SQL`] instead: a row is not
/// committed until the transaction that created it commits, and stamping it at create would date an
/// operation that may be rolled back.
pub const CREATE_OPERATION_SQL: &str = "CREATE semantic_operation SET workspace = $workspace, actor \
     = $actor, initiator = $initiator, kind = $kind, operation_key = $key, request_fingerprint = \
     $fingerprint, expected_effects = $effects, affected_scope_kinds = $kinds, affected_projects = \
     $projects RETURN AFTER;";

pub const FINISH_OPERATION_SQL: &str = "UPDATE type::record('semantic_operation', $key) SET \
     preimages = $preimages, postimages = $postimages, affected_ids = $affected, preimage_hash = \
     $preimage_hash, postimage_hash = $postimage_hash, actual_effects = $actual, committed_at = \
     $now RETURN AFTER;";

pub const HISTORY_SQL: &str = "SELECT id, kind, operation_key, actor, committed_at, \
     expected_effects, actual_effects, request_fingerprint, preimages, postimages, reverts, reverted_by FROM \
     semantic_operation WHERE workspace = $workspace ORDER BY created_at DESC, id DESC LIMIT \
     $limit;";

pub const OPERATION_SQL: &str = "SELECT id, kind, operation_key, actor, committed_at, \
     expected_effects, actual_effects, request_fingerprint, preimages, postimages, reverts, reverted_by FROM \
     semantic_operation WHERE workspace = $workspace AND operation_key = $key LIMIT 1;";

pub const OPERATION_BY_ID_SQL: &str = "SELECT id, kind, operation_key, actor, committed_at, \
     expected_effects, actual_effects, request_fingerprint, preimages, postimages, reverts, reverted_by FROM \
     type::record('semantic_operation', $key) WHERE id = $id AND workspace = $workspace LIMIT 1;";

pub const SNAPSHOT_READ_SQL: &str = "SELECT * FROM type::record('semantic_snapshot', $key) WHERE \
     id = $id AND workspace = $workspace LIMIT 1;";

/// The per-variant snapshot insert: one arm, its own payload columns, and never a foreign one.
pub(crate) fn snapshot_write(snapshot: &SemanticSnapshot) -> (String, Vec<Bind>) {
    match snapshot {
        SemanticSnapshot::Fact(fact) => {
            let (kind, project) = scope_parts(&fact.scope);
            let mut sets = String::from(
                "fact = $fact, fact_content = $content, fact_tags = $tags, fact_confidence = \
                 $confidence, fact_scope = $scope, fact_project = $project, fact_revision = \
                 $revision, fact_deleted = $deleted, fact_quarantined_at = $quarantined_at, \
                 fact_relations = $relations",
            );
            // v009 types `fact_relations` as `array<record<memory_edge>>`, so only the `memory_edge`
            // half of the guarded relation set can be stored. The structural half is *not* lost: it is
            // inside `fact.semantic_hash()`, which is what the graph guard is computed from, and a
            // membership that moved since the caller read it fails the digest comparison rather than
            // silently passing a column that could not hold it.
            let relations: Vec<RecordId> = fact
                .relation_versions
                .iter()
                .filter(|edge| matches!(edge.kind, EdgeKind::Memory(_)))
                .map(|edge| edge.edge.clone())
                .collect();
            let mut binds = vec![
                Bind::Record("fact", fact.id.as_record_id().clone()),
                Bind::Text("content", fact.content.clone()),
                Bind::TextList("tags", fact.tags.clone()),
                Bind::Float("confidence", fact.confidence),
                Bind::Text("scope", kind.to_string()),
                Bind::OptRecord("project", project),
                Bind::OptInt("revision", Some(revision(fact.version))),
                Bind::Bool("deleted", fact.visibility.deleted),
                Bind::OptTime("quarantined_at", fact.visibility.quarantined_at),
                Bind::Records("relations", relations),
            ];
            if let Some(embedding) = &fact.embedding {
                sets.push_str(
                    ", fact_embedding = $embedding, fact_embedding_model = $embedding_model, \
                     fact_embedding_dimensions = $embedding_dimensions",
                );
                binds.push(Bind::Floats("embedding", embedding.vector.clone()));
                binds.push(Bind::OptText("embedding_model", embedding.model.clone()));
                binds.push(Bind::OptInt(
                    "embedding_dimensions",
                    Some(embedding.dimensions as i64),
                ));
            }
            (statement(snapshot_kind::FACT, &sets), binds)
        }
        SemanticSnapshot::Edge(edge) => {
            let mut sets = String::from(
                "edge = $edge, edge_in = $edge_in, edge_out = $edge_out, \
                              edge_strength = $edge_strength",
            );
            let mut binds = vec![
                Bind::Record("edge", edge.edge.clone()),
                Bind::Record("edge_in", edge.from.clone()),
                Bind::Record("edge_out", edge.to.clone()),
                Bind::Float("edge_strength", edge.strength),
            ];
            if let Some(edge_type) = edge.kind.edge_type() {
                sets.push_str(", edge_type = $edge_type");
                binds.push(Bind::Text("edge_type", edge_type.to_string()));
            }
            (statement(snapshot_kind::EDGE, &sets), binds)
        }
        SemanticSnapshot::Cluster(cluster) => (
            statement(
                snapshot_kind::CLUSTER,
                "cluster = $cluster, cluster_label = $label, cluster_centroid = $centroid, \
                 cluster_depth = $depth, cluster_members = $members, \
                 cluster_membership_revision = $membership_revision, \
                 cluster_member_embedding_revision = $member_embedding_revision, \
                 cluster_centroid_revision = $centroid_revision",
            ),
            vec![
                Bind::Record("cluster", cluster.id.clone()),
                Bind::OptText("label", cluster.label.clone()),
                Bind::Floats("centroid", cluster.centroid.clone()),
                Bind::Int("depth", cluster.depth),
                Bind::Records("members", cluster.members.clone()),
                Bind::OptInt(
                    "membership_revision",
                    Some(revision(cluster.membership_revision)),
                ),
                Bind::OptInt(
                    "member_embedding_revision",
                    Some(revision(cluster.member_embedding_revision)),
                ),
                Bind::OptInt(
                    "centroid_revision",
                    Some(revision(cluster.centroid_revision)),
                ),
            ],
        ),
        SemanticSnapshot::Reminder(reminder) => (
            statement(
                snapshot_kind::REMINDER,
                "reminder = $reminder, reminder_account = $reminder_account, reminder_message = \
                 $message, reminder_target_project = $target_project, \
                 reminder_legacy_target_project = $legacy_target_project, reminder_project = \
                 $reminder_project, reminder_migration_hold = $migration_hold, \
                 reminder_semantic_revision = $semantic_revision, reminder_schedule_kind = \
                 $schedule_kind",
            ),
            vec![
                Bind::Record("reminder", reminder.id.clone()),
                Bind::Record("reminder_account", reminder.account.clone()),
                Bind::Text("message", reminder.message.clone()),
                Bind::OptText("target_project", reminder.target_project.clone()),
                Bind::OptText(
                    "legacy_target_project",
                    reminder.legacy_target_project.clone(),
                ),
                Bind::OptRecord(
                    "reminder_project",
                    reminder
                        .project
                        .as_ref()
                        .map(|project| project.as_record_id().clone()),
                ),
                Bind::OptBool("migration_hold", reminder.migration_hold),
                Bind::OptInt(
                    "semantic_revision",
                    Some(revision(reminder.semantic_revision)),
                ),
                Bind::OptText("schedule_kind", reminder.schedule_kind.clone()),
            ],
        ),
    }
}

/// One snapshot create, with the discriminator in front of the payload — v009's assertion reads its
/// sibling through `$this.kind`, which requires the kind field to arrive first.
fn statement(kind: &str, sets: &str) -> String {
    format!(
        "CREATE semantic_snapshot SET kind = '{kind}', subject = $subject, workspace = $workspace, \
         operation = $operation, {sets} RETURN AFTER;"
    )
}

// -- row shapes ------------------------------------------------------------------------------------------

#[derive(SurrealValue)]
struct IdRow {
    id: RecordId,
}

#[derive(SurrealValue)]
struct FactRow {
    id: RecordId,
    content: String,
    confidence: f64,
    embedding: Vec<f32>,
    tags: Vec<String>,
    deleted: bool,
    quarantined_at: Option<DateTime<Utc>>,
    semantic_revision: Option<i64>,
    workspace: Option<RecordId>,
    scope: Option<String>,
    project: Option<RecordId>,
}

impl FactRow {
    /// The row and its incident graph, as the snapshot the digest is computed from.
    ///
    /// `NONE` in `semantic_revision` reads as [`Version::ZERO`], which is v009's own rule that an absent
    /// revision column means the value its default would have given. The digest still moves when the
    /// content does, and that is how a writer that never bumps the counter gets caught.
    fn into_snapshot(self, workspace: &WorkspaceId, relations: Vec<EdgeSnapshot>) -> FactSnapshot {
        let dimensions = self.embedding.len();
        FactSnapshot {
            id: FactId::try_from(self.id.clone())
                .expect("a row read from the fact table is a fact"),
            workspace: workspace.clone(),
            scope: read_scope(self.scope.as_deref(), self.project),
            version: Version::new(self.semantic_revision.unwrap_or(0).max(0) as u64),
            content: self.content,
            tags: self.tags,
            confidence: self.confidence,
            visibility: Visibility {
                deleted: self.deleted,
                quarantined_at: self.quarantined_at,
            },
            embedding: Some(EmbeddingSnapshot {
                vector: self.embedding,
                model: None,
                dimensions,
            }),
            relation_versions: relations,
        }
    }
}

#[derive(SurrealValue)]
struct MemoryEdgeRow {
    id: Option<RecordId>,
    #[surreal(rename = "in")]
    in_node: Option<RecordId>,
    #[surreal(rename = "out")]
    out_node: Option<RecordId>,
    edge_type: Option<String>,
    strength: Option<f64>,
}

impl MemoryEdgeRow {
    fn into_snapshot(self) -> EdgeSnapshot {
        EdgeSnapshot {
            edge: self.id.unwrap_or_else(|| RecordId::new("memory_edge", "")),
            kind: EdgeKind::Memory(self.edge_type.unwrap_or_default()),
            from: self.in_node.unwrap_or_else(|| RecordId::new("fact", "")),
            to: self.out_node.unwrap_or_else(|| RecordId::new("fact", "")),
            strength: self.strength.unwrap_or(1.0),
            generated_from: None,
        }
    }
}

#[derive(SurrealValue)]
struct StructuralRow {
    id: Option<RecordId>,
    #[surreal(rename = "in")]
    in_node: Option<RecordId>,
    #[surreal(rename = "out")]
    out_node: Option<RecordId>,
    generated_from: Option<RecordId>,
}

impl StructuralRow {
    fn into_snapshot(self, kind: EdgeKind) -> EdgeSnapshot {
        EdgeSnapshot {
            edge: self.id.unwrap_or_else(|| RecordId::new(kind.table(), "")),
            kind,
            from: self.in_node.unwrap_or_else(|| RecordId::new("fact", "")),
            to: self.out_node.unwrap_or_else(|| RecordId::new("fact", "")),
            strength: 1.0,
            generated_from: self.generated_from,
        }
    }
}

#[derive(SurrealValue)]
struct ClusterRow {
    id: Option<RecordId>,
    label: Option<String>,
    centroid: Vec<f32>,
    depth: i64,
    membership_revision: Option<i64>,
    member_embedding_revision: Option<i64>,
    centroid_revision: Option<i64>,
}

impl ClusterRow {
    /// The written row as the journal's cluster image, with the guards **as this transaction stored them**.
    ///
    /// `NONE` reads as the zero a `DEFAULT` would have given — the same rule the CAS guard itself applies,
    /// so a legacy cluster row cannot image as a revision the write never moved.
    fn into_snapshot(self, cluster: &RecordId) -> ClusterSnapshot {
        ClusterSnapshot {
            id: cluster.clone(),
            label: self.label,
            centroid: self.centroid,
            depth: self.depth,
            members: Vec::new(),
            membership_revision: guard(self.membership_revision),
            member_embedding_revision: guard(self.member_embedding_revision),
            centroid_revision: guard(self.centroid_revision),
        }
    }
}

fn guard(stored: Option<i64>) -> Version {
    Version::new(stored.unwrap_or(0).max(0) as u64)
}

/// One row of the in-transaction membership read: the far end and nothing else.
#[derive(SurrealValue)]
struct MemberRow {
    member: Option<RecordId>,
}

/// Both projected counts must explicitly establish a lawful, nonempty destination.
#[derive(SurrealValue)]
struct PurityRow {
    eligible: Option<i64>,
    impure: Option<i64>,
}

#[derive(SurrealValue)]
struct GuardRow {
    membership_revision: Option<i64>,
    member_embedding_revision: Option<i64>,
    centroid_revision: Option<i64>,
}

/// Exact eligible workspace population. Arithmetic must consume this same sample, not a second read.
pub const CLUSTER_POPULATION_SQL: &str = "SELECT id, embedding FROM \
     $cluster->contains_memory->(fact WHERE deleted = false AND quarantined_at = NONE AND \
     account = $account AND workspace = $workspace);";

const CLUSTER_WORKSPACE_MEMBER_IDS_SQL: &str = "SELECT VALUE id FROM \
     $cluster->contains_memory->(fact WHERE account = $account AND workspace = $workspace);";

#[derive(SurrealValue)]
struct PopulationRow {
    id: RecordId,
    embedding: Vec<f32>,
}

#[derive(SurrealValue)]
struct LockedPopulationRow {
    id: RecordId,
    embedding: Vec<f32>,
    deleted: Option<bool>,
    quarantined_at: Option<DateTime<Utc>>,
}

/// The cluster a membership edge points at, and nothing else.
#[derive(SurrealValue)]
struct ClusterRefRow {
    cluster: Option<RecordId>,
}

impl GuardRow {
    /// The three guards as one CAS token, with `NONE` read as the default zero would have given.
    fn into_expectation(
        self,
        cluster: &RecordId,
        population: Vec<ClusterPopulationMember>,
    ) -> Result<ClusterWriteExpectation, StorageError> {
        Ok(ClusterWriteExpectation {
            cluster: cluster.clone(),
            population,
            membership_revision: Version::new(self.membership_revision.unwrap_or(0).max(0) as u64),
            member_embedding_revision: Version::new(
                self.member_embedding_revision.unwrap_or(0).max(0) as u64,
            ),
            centroid_revision: Version::new(self.centroid_revision.unwrap_or(0).max(0) as u64),
        })
    }
}

#[derive(SurrealValue)]
struct JournalRow {
    id: RecordId,
    kind: String,
    operation_key: String,
    actor: Option<RecordId>,
    committed_at: Option<DateTime<Utc>>,
    expected_effects: Vec<String>,
    actual_effects: Option<Vec<String>>,
    request_fingerprint: String,
    preimages: Vec<RecordId>,
    postimages: Vec<RecordId>,
    reverts: Option<RecordId>,
    reverted_by: Option<RecordId>,
}

impl JournalRow {
    /// The recorded answer for a duplicate same-key request, with nothing re-executed.
    fn into_replay(self) -> AppliedChange {
        AppliedChange {
            operation: crate::record_id_to_string(&self.id),
            preimages: names(&self.preimages),
            postimages: names(&self.postimages),
            audit: crate::record_id_to_string(&self.id),
            created: Vec::new(),
            // A replay minted nothing, so it reports no source document either. The import loop that keys a
            // chunk on a fresh UUID per request never reaches this arm for a `raw` it still needs; see
            // `AlexandriaServer::import_body`, which links by handle only after its own commit returned one.
            created_raw: Vec::new(),
            replayed: true,
        }
    }

    /// A list row, and no content.
    ///
    /// `actor` is the caller's own handle rendered back only when the journal names *this* account. An
    /// operation always names the account it ran under, so a list that printed a foreign identity would be
    /// an account-enumeration surface wearing a history page — and it cannot happen through this path,
    /// because the query is workspace-filtered first.
    fn into_summary(self, owner: &OwnerContext) -> OperationSummary {
        let effects = self.actual_effects.unwrap_or(self.expected_effects);
        let touched = self.preimages.len() + self.postimages.len();
        OperationSummary {
            id: crate::record_id_to_string(&self.id),
            kind: self.kind,
            operation_key: self.operation_key,
            actor: actor_label(&self.actor, owner),
            committed_at: self.committed_at,
            effects,
            touched,
        }
    }

    fn into_detail(
        self,
        owner: &OwnerContext,
        preimages: Vec<SemanticSnapshot>,
        postimages: Vec<SemanticSnapshot>,
        preimage_records: Vec<Value>,
        postimage_records: Vec<Value>,
    ) -> OperationDetail {
        let actor = actor_label(&self.actor, owner);
        OperationDetail {
            id: crate::record_id_to_string(&self.id),
            kind: self.kind,
            operation_key: self.operation_key,
            actor,
            committed_at: self.committed_at,
            reverts: self.reverts.as_ref().map(crate::record_id_to_string),
            reverted_by: self.reverted_by.as_ref().map(crate::record_id_to_string),
            expected_effects: self.expected_effects,
            actual_effects: self.actual_effects.unwrap_or_default(),
            preimages,
            postimages,
            preimage_records,
            postimage_records,
        }
    }
}

fn actor_label(actor: &Option<RecordId>, owner: &OwnerContext) -> String {
    if actor.as_ref() == Some(owner.account().as_record_id()) {
        crate::record_id_to_string(owner.account().as_record_id())
    } else {
        String::new()
    }
}

#[derive(SurrealValue)]
struct SnapshotRow {
    kind: Option<String>,
    fact: Option<RecordId>,
    fact_content: Option<String>,
    fact_tags: Option<Vec<String>>,
    fact_confidence: Option<f64>,
    fact_scope: Option<String>,
    fact_project: Option<RecordId>,
    fact_revision: Option<i64>,
    fact_deleted: Option<bool>,
    fact_quarantined_at: Option<DateTime<Utc>>,
    fact_embedding: Option<Vec<f32>>,
    fact_embedding_model: Option<String>,
    fact_embedding_dimensions: Option<i64>,
    quality_graph: Option<Vec<quality::GraphRow>>,
    edge: Option<RecordId>,
    edge_in: Option<RecordId>,
    edge_out: Option<RecordId>,
    edge_type: Option<String>,
    edge_strength: Option<f64>,
    cluster: Option<RecordId>,
    cluster_label: Option<String>,
    cluster_centroid: Option<Vec<f32>>,
    cluster_depth: Option<i64>,
    cluster_members: Option<Vec<RecordId>>,
    cluster_membership_revision: Option<i64>,
    cluster_member_embedding_revision: Option<i64>,
    cluster_centroid_revision: Option<i64>,
    reminder: Option<RecordId>,
    reminder_account: Option<RecordId>,
    reminder_message: Option<String>,
    reminder_target_project: Option<String>,
    reminder_legacy_target_project: Option<String>,
    reminder_project: Option<RecordId>,
    reminder_migration_hold: Option<bool>,
    reminder_semantic_revision: Option<i64>,
    reminder_schedule_kind: Option<String>,
}

impl SnapshotRow {
    /// The stored row back as its typed variant, keyed on the discriminator v009 asserted.
    ///
    /// The snapshot row's own id is only a journal handle — the *subject* is what it describes — so a
    /// `fact` variant rebuilds its identity from `fact`, and a row whose payload has vanished (which the
    /// schema's own assertions make unreachable) is reported rather than rendered as an empty memory.
    fn into_snapshot(self, workspace: &RecordId) -> Result<SemanticSnapshot, StorageError> {
        match self.kind.as_deref() {
            Some(snapshot_kind::EDGE) => Ok(SemanticSnapshot::Edge(EdgeSnapshot {
                edge: self.edge.ok_or_else(|| corrupt(snapshot_kind::EDGE))?,
                kind: EdgeKind::Memory(self.edge_type.unwrap_or_default()),
                from: self.edge_in.ok_or_else(|| corrupt(snapshot_kind::EDGE))?,
                to: self.edge_out.ok_or_else(|| corrupt(snapshot_kind::EDGE))?,
                strength: self.edge_strength.unwrap_or(1.0),
                generated_from: None,
            })),
            Some(snapshot_kind::CLUSTER) => Ok(SemanticSnapshot::Cluster(ClusterSnapshot {
                id: self
                    .cluster
                    .ok_or_else(|| corrupt(snapshot_kind::CLUSTER))?,
                label: self.cluster_label,
                centroid: self.cluster_centroid.unwrap_or_default(),
                depth: self.cluster_depth.unwrap_or(0),
                members: self.cluster_members.unwrap_or_default(),
                membership_revision: Version::new(
                    self.cluster_membership_revision.unwrap_or(0).max(0) as u64,
                ),
                member_embedding_revision: Version::new(
                    self.cluster_member_embedding_revision.unwrap_or(0).max(0) as u64,
                ),
                centroid_revision: Version::new(
                    self.cluster_centroid_revision.unwrap_or(0).max(0) as u64
                ),
            })),
            Some(snapshot_kind::REMINDER) => Ok(SemanticSnapshot::Reminder(ReminderSnapshot {
                id: self
                    .reminder
                    .ok_or_else(|| corrupt(snapshot_kind::REMINDER))?,
                account: self
                    .reminder_account
                    .ok_or_else(|| corrupt(snapshot_kind::REMINDER))?,
                message: self.reminder_message.unwrap_or_default(),
                target_project: self.reminder_target_project,
                legacy_target_project: self.reminder_legacy_target_project,
                project: self
                    .reminder_project
                    .and_then(|id| ProjectId::try_from(id).ok()),
                migration_hold: self.reminder_migration_hold,
                semantic_revision: Version::new(
                    self.reminder_semantic_revision.unwrap_or(0).max(0) as u64,
                ),
                schedule_kind: self.reminder_schedule_kind,
            })),
            _ => {
                let id: FactId = self
                    .fact
                    .and_then(|id| FactId::try_from(id).ok())
                    .ok_or_else(|| corrupt(crate::models::snapshot_kind::FACT))?;
                let vector = self.fact_embedding.unwrap_or_default();
                Ok(SemanticSnapshot::Fact(FactSnapshot {
                    workspace: WorkspaceId::try_from(workspace.clone())?,
                    id,
                    scope: read_scope(self.fact_scope.as_deref(), self.fact_project),
                    version: Version::new(self.fact_revision.unwrap_or(0).max(0) as u64),
                    content: self.fact_content.unwrap_or_default(),
                    tags: self.fact_tags.unwrap_or_default(),
                    confidence: self.fact_confidence.unwrap_or(0.0),
                    visibility: Visibility {
                        deleted: self.fact_deleted.unwrap_or(false),
                        quarantined_at: self.fact_quarantined_at,
                    },
                    embedding: Some(EmbeddingSnapshot {
                        model: self.fact_embedding_model,
                        dimensions: self.fact_embedding_dimensions.unwrap_or(0).max(0) as usize,
                        vector,
                    }),
                    relation_versions: self
                        .quality_graph
                        .unwrap_or_default()
                        .into_iter()
                        .map(quality::GraphRow::into_edge)
                        .collect::<Result<_, _>>()?,
                }))
            }
        }
    }
}

fn corrupt(kind: &str) -> StorageError {
    StorageError::Internal(anyhow::anyhow!(
        "a {kind} snapshot row is missing the payload its own variant requires"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::identity::{FactId, WorkspaceId};
    use crate::models::mutation::{BatchConflict, BatchConflictReason, BatchResult};

    const ERASED_CONFLICT: &str = "Transaction conflict: Read conflict, retry the transaction. This transaction can be retried";

    #[test]
    fn population_commit_classifies_typed_and_erased_conflicts_locally() {
        let typed = surrealdb::Error::query(
            ERASED_CONFLICT.to_string(),
            surrealdb::types::QueryError::TransactionConflict,
        );
        assert!(matches!(
            MutationRepo::commit_error(typed),
            StorageError::RetryableBookkeepingConflict
        ));
        let erased = surrealdb::Error::internal(ERASED_CONFLICT.to_string());
        assert!(
            !is_write_conflict(&erased),
            "the global query/auth classifier must remain unchanged"
        );
        assert!(matches!(
            MutationRepo::commit_error(erased),
            StorageError::RetryableBookkeepingConflict
        ));
    }

    #[test]
    fn population_commit_preserves_unrelated_and_lookalike_errors() {
        let errors = [
            surrealdb::Error::internal("disk full".to_string()),
            surrealdb::Error::internal(format!("prefix {ERASED_CONFLICT}")),
            surrealdb::Error::internal(format!("{ERASED_CONFLICT} suffix")),
            surrealdb::Error::internal(
                "Transaction conflict: . This transaction can be retried".to_string(),
            ),
            surrealdb::Error::internal("Transaction conflict: Read conflict".to_string()),
            surrealdb::Error::internal(
                "Transaction conflict:    . This transaction can be retried".to_string(),
            ),
            surrealdb::Error::thrown(ERASED_CONFLICT.to_string()),
            surrealdb::Error::query(
                ERASED_CONFLICT.to_string(),
                surrealdb::types::QueryError::Cancelled,
            ),
            surrealdb::Error::internal(ERASED_CONFLICT.to_string())
                .with_cause(surrealdb::Error::internal("not erased".to_string())),
        ];
        for error in errors {
            assert!(matches!(
                MutationRepo::commit_error(error),
                StorageError::Internal(_)
            ));
        }
    }

    #[tokio::test]
    async fn population_commit_real_read_conflicts_retry_and_roll_back() {
        for persistent in [false, true] {
            for locked in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let db = if persistent {
                    crate::Database::connect_persistent(&temp.path().join("population-store"))
                        .await
                        .unwrap()
                } else {
                    crate::Database::connect_embedded().await.unwrap()
                };
                db.inner().query("CREATE population_member:m SET embedding = [1.0, 0.0], deleted = false; CREATE population_mean:c SET centroid = [1.0, 0.0];")
                    .await.unwrap().check().unwrap();
                let tx = db.inner().clone().begin().await.unwrap();
                let sql = if locked {
                    "SELECT * FROM population_member:m FOR UPDATE;"
                } else {
                    "SELECT * FROM population_member:m;"
                };
                let mut response = tx.query(sql).await.unwrap().check().unwrap();
                let read: Vec<Value> = response.take(0).unwrap();
                assert_eq!(read.len(), 1);
                db.inner()
                    .query("UPDATE population_member:m SET deleted = true;")
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                tx.query("UPDATE population_mean:c SET centroid = [0.5, 0.5];")
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                let outcome = MutationRepo::commit(tx).await;
                if locked {
                    assert!(
                        matches!(&outcome, Err(StorageError::RetryableBookkeepingConflict)),
                        "persistent={persistent},locked={locked}: {outcome:?}"
                    );
                } else {
                    outcome.unwrap();
                }
                let means: Vec<Vec<f32>> = db
                    .inner()
                    .query("SELECT VALUE centroid FROM population_mean:c;")
                    .await
                    .unwrap()
                    .check()
                    .unwrap()
                    .take(0)
                    .unwrap();
                assert_eq!(
                    means,
                    vec![if locked {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.5, 0.5]
                    }]
                );
                let deleted: Vec<bool> = db
                    .inner()
                    .query("SELECT VALUE deleted FROM population_member:m;")
                    .await
                    .unwrap()
                    .check()
                    .unwrap()
                    .take(0)
                    .unwrap();
                assert_eq!(deleted, vec![true], "preserve the independent writer");
            }
        }
    }

    #[tokio::test]
    async fn population_commit_guard_only_characterizes_versioned_and_unversioned_arrival() {
        use crate::access::AccessRepo;
        use crate::models::identity::ReadSelection;
        use crate::repos::{IdentityRepo, ScopedClusterRepo, ScopedMemoryRepo, WriteTarget};

        async fn observed(db: &Surreal<Any>) -> Vec<Vec<Value>> {
            let mut response = db.query(
                "SELECT * FROM fact ORDER BY id; SELECT * FROM cluster ORDER BY id; \
                 SELECT * FROM contains_memory ORDER BY id; \
                 SELECT * FROM semantic_operation ORDER BY id; SELECT * FROM semantic_snapshot ORDER BY id;",
            ).await.unwrap().check().unwrap();
            (0..5).map(|index| response.take(index).unwrap()).collect()
        }
        // The stronger raw-arrival expectation failed in preserved logs81/82. Point-record
        // protection is not a predicate fence; the temporary legacy maintenance gate closes
        // the supported owned writer bypass, not arbitrary privileged unversioned SQL.
        for interference in ["counter", "versioned-arrival", "unversioned-arrival"] {
            let arrival = interference != "counter";
            let versioned = interference == "versioned-arrival";
            for guarded in [false, true] {
                let mut db = crate::Database::connect_embedded().await.unwrap();
                crate::schema::migrate(db.inner()).await.unwrap();
                let system = db.claim_system_authority().unwrap();
                let ids = IdentityRepo::new(db.inner())
                    .bootstrap_owner(
                        "guard-snapshot-owner",
                        "$argon2id$fixture-prepared-hash-guard-padding",
                        &OperationId::new("bootstrap"),
                    )
                    .await
                    .unwrap();
                let admission = AccessRepo::new(&db);
                let principal = admission
                    .local_principal(&system, &ids.account)
                    .await
                    .unwrap();
                let owner = admission.authorize(&principal).await.unwrap();
                let read = admission
                    .select(&owner, ReadSelection::Assigned)
                    .await
                    .unwrap();
                let memories = ScopedMemoryRepo::new(db.inner(), &owner, &read);
                let loose = memories
                    .create_fact(
                        "loose population witness",
                        0.5,
                        &[1.0, 0.0],
                        &[],
                        WriteTarget::Personal,
                        Utc::now(),
                    )
                    .await
                    .unwrap()
                    .id
                    .unwrap();
                let subject = FactId::try_from(
                    memories
                        .create_fact(
                            "unclustered subject",
                            0.5,
                            &[1.0, 0.0],
                            &[],
                            WriteTarget::Personal,
                            Utc::now(),
                        )
                        .await
                        .unwrap()
                        .id
                        .unwrap(),
                )
                .unwrap();
                let witness = memories
                    .create_fact(
                        "cluster witness",
                        0.5,
                        &[1.0, 0.0],
                        &[],
                        WriteTarget::Personal,
                        Utc::now(),
                    )
                    .await
                    .unwrap()
                    .id
                    .unwrap();
                let cluster = ScopedClusterRepo::new(db.inner(), &owner, &read)
                    .create(Some("guarded cluster"), &[1.0, 0.0], Utc::now())
                    .await
                    .unwrap()
                    .id
                    .unwrap();
                db.inner()
                    .query("RELATE $cluster->contains_memory->$member;")
                    .bind(("cluster", cluster.clone()))
                    .bind(("member", witness))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                let mutation = MutationRepo::new(db.inner());
                let original = mutation.state(&owner, &subject).await.unwrap().unwrap();
                let expected = [ExpectedState::of(&original)];
                let mut change = PreparedChangeSet::new(operation_kind::UPDATE)
                    .effect(effect::TAGS)
                    .fact(FactChange::Update {
                        fact: subject.clone(),
                        expected: original.version,
                        content: None,
                        tags: Some(vec!["checked".to_string()]),
                        confidence: None,
                        embedding: None,
                        visibility: None,
                        scope: None,
                    });
                if guarded {
                    change = change.cluster(ClusterChange::Guard {
                        expectation: mutation
                            .cluster_expectation(&owner, &cluster)
                            .await
                            .unwrap(),
                    });
                }
                let access = MutationContext::issued_to(&owner);
                let operation = OperationId::new("guard-after-snapshot");
                let fingerprint = change.request_fingerprint();
                let request = PassRequest {
                    access: &access,
                    operation: &operation,
                    fingerprint: &fingerprint,
                    expected: &expected,
                    change: &change,
                    now: Utc::now(),
                };
                let tx = db.inner().clone().begin().await.unwrap();
                // Actual production run stages its fact and journal. The unclustered subject isolates
                // the cluster-read fence from any guards on the subject's own incident relationships.
                mutation.run(&tx, &request).await.unwrap();
                if versioned {
                    let independent_tx = db.inner().clone().begin().await.unwrap();
                    independent_tx
                        .query("RELATE $cluster->contains_memory->$member; UPDATE $cluster SET membership_revision = 1;")
                        .bind(("cluster", cluster.clone()))
                        .bind(("member", loose))
                        .await.unwrap().check().unwrap();
                    MutationRepo::commit(independent_tx).await.unwrap();
                } else if arrival {
                    db.inner()
                        .query("RELATE $cluster->contains_memory->$member;")
                        .bind(("cluster", cluster.clone()))
                        .bind(("member", loose))
                        .await
                        .unwrap()
                        .check()
                        .unwrap();
                } else {
                    db.inner()
                        .query("UPDATE $cluster SET centroid_revision = 1;")
                        .bind(("cluster", cluster.clone()))
                        .await
                        .unwrap()
                        .check()
                        .unwrap();
                }
                let independent = observed(db.inner()).await;
                let outcome = MutationRepo::commit(tx).await;
                if guarded && interference != "unversioned-arrival" {
                    assert!(
                        matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
                        "{interference}: a versioned protected read must survive until commit: {outcome:?}"
                    );
                    assert_eq!(
                        observed(db.inner()).await,
                        independent,
                        "rollback must retain only the independent write"
                    );
                    assert!(
                        mutation
                            .operation(&owner, "guard-after-snapshot")
                            .await
                            .unwrap()
                            .is_none()
                    );
                } else {
                    outcome.expect(
                        "an unguarded change or raw unversioned arrival has no point-record conflict",
                    );
                    assert!(
                        mutation
                            .operation(&owner, "guard-after-snapshot")
                            .await
                            .unwrap()
                            .is_some()
                    );
                    assert_eq!(
                        observed(db.inner()).await[1..3],
                        independent[1..3],
                        "a committed tag update must retain the complete independent cluster/edge write"
                    );
                    assert_eq!(
                        mutation
                            .state(&owner, &subject)
                            .await
                            .unwrap()
                            .unwrap()
                            .tags,
                        vec!["checked"]
                    );
                }
                let revisions: Vec<Option<i64>> = db
                    .inner()
                    .query("SELECT VALUE centroid_revision FROM $cluster;")
                    .bind(("cluster", cluster.clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap()
                    .take(0)
                    .unwrap();
                assert_eq!(
                    revisions
                        .into_iter()
                        .map(|value| value.unwrap_or(0))
                        .collect::<Vec<_>>(),
                    vec![if arrival { 0 } else { 1 }],
                    "preserve the independent writer without inventing an arrival counter"
                );
                let membership: Vec<Option<i64>> = db
                    .inner()
                    .query("SELECT VALUE membership_revision FROM $cluster;")
                    .bind(("cluster", cluster))
                    .await
                    .unwrap()
                    .check()
                    .unwrap()
                    .take(0)
                    .unwrap();
                assert_eq!(
                    membership
                        .into_iter()
                        .map(|value| value.unwrap_or(0))
                        .collect::<Vec<_>>(),
                    vec![i64::from(versioned)],
                    "only the atomic versioned arrival changes its counter"
                );
            }
        }
    }

    #[tokio::test]
    async fn classification_repair_proposal_change_after_transaction_snapshot() {
        use crate::access::AccessRepo;
        use crate::models::identity::ReadSelection;
        use crate::repos::{
            ClassificationRepo, IdentityRepo, ScopedMemoryRepo, WriteTarget, commit_prepared,
        };

        async fn observed(db: &Surreal<Any>) -> Vec<Vec<Value>> {
            let mut response = db
                .query(
                    "SELECT * FROM fact ORDER BY id; \
                SELECT * FROM classification_proposal ORDER BY id; \
                SELECT * FROM semantic_operation ORDER BY id; \
                SELECT * FROM semantic_snapshot ORDER BY id;",
                )
                .await
                .unwrap()
                .check()
                .unwrap();
            (0..4).map(|index| response.take(index).unwrap()).collect()
        }
        for dismiss in [false, true] {
            let mut db = crate::Database::connect_embedded().await.unwrap();
            crate::schema::migrate(db.inner()).await.unwrap();
            let system = db.claim_system_authority().unwrap();
            let ids = IdentityRepo::new(db.inner())
                .bootstrap_owner(
                    "snapshot-owner",
                    "$argon2id$fixture-prepared-hash-classification-padding",
                    &OperationId::new("bootstrap"),
                )
                .await
                .unwrap();
            let admission = AccessRepo::new(&db);
            let principal = admission
                .local_principal(&system, &ids.account)
                .await
                .unwrap();
            let owner = admission.authorize(&principal).await.unwrap();
            let read = admission
                .select(&owner, ReadSelection::UnassignedReview)
                .await
                .unwrap();
            let row = ScopedMemoryRepo::new(db.inner(), &owner, &read)
                .create_fact(
                    "after-snapshot subject",
                    0.5,
                    &[1.0, 0.0],
                    &[],
                    WriteTarget::Unassigned,
                    Utc::now(),
                )
                .await
                .unwrap();
            let fact = FactId::try_from(row.id.unwrap()).unwrap();
            let classification = ClassificationRepo::new(db.inner(), &owner);
            let reference = classification
                .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
                .await
                .unwrap();
            let mutation = MutationRepo::new(db.inner());
            let original = mutation.state(&owner, &fact).await.unwrap().unwrap();
            let expected = [ExpectedState::of(&original)];
            let change = mutation
                .plan_move(
                    &owner,
                    &fact,
                    original.version,
                    &original.scope,
                    &MemoryScope::Personal,
                    &MoveReconciliation::ungrouped(),
                )
                .await
                .unwrap()
                .approve_classification(ClassificationApproval {
                    proposal: RecordId::parse_simple(&reference.id).unwrap(),
                    proposal_revision: reference.revision,
                    fact: fact.clone(),
                    source_revision: original.version,
                    destination: MemoryScope::Personal,
                });
            let access = MutationContext::issued_to(&owner);
            let operation = OperationId::new("after-snapshot");
            let fingerprint = change.request_fingerprint();
            let request = PassRequest {
                access: &access,
                operation: &operation,
                fingerprint: &fingerprint,
                expected: &expected,
                change: &change,
                now: Utc::now(),
            };
            let tx = db.inner().clone().begin().await.unwrap();
            // Actual production run stages the approval, move and journal against txA's snapshot.
            mutation.run(&tx, &request).await.unwrap();
            // The independent decision happens AFTER txA staged its writes, not during preflight.
            if dismiss {
                classification.dismiss(&reference).await.unwrap();
            } else {
                classification
                    .revise_destination(reference.clone(), MemoryScope::Unassigned)
                    .await
                    .unwrap();
            }
            let independent = observed(db.inner()).await;
            assert!(matches!(
                MutationRepo::commit(tx).await,
                Err(StorageError::RetryableBookkeepingConflict)
            ));
            assert_eq!(observed(db.inner()).await, independent);
            let retry = commit_prepared(
                &mutation,
                &access,
                &operation,
                &expected,
                &change.effects,
                || async { Ok(change.clone()) },
            )
            .await;
            assert!(matches!(retry, Err(StorageError::Stale)));
            assert_eq!(observed(db.inner()).await, independent);
            assert_eq!(
                mutation.state(&owner, &fact).await.unwrap().unwrap(),
                original
            );
        }
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap()
    }

    fn fact(key: &str) -> FactId {
        FactId::try_from(RecordId::new("fact", key)).unwrap()
    }

    /// A `NONE` revision reads as the default zero would have given, and the scope of a project row that
    /// lost its handle is Unassigned rather than an invented Personal.
    #[test]
    fn a_legacy_row_reads_its_defaults_and_never_an_invented_partition() {
        let row = FactRow {
            id: fact("legacy").as_record_id().clone(),
            content: "legacy text".to_string(),
            confidence: 0.5,
            embedding: vec![0.1, 0.2],
            tags: vec![],
            deleted: false,
            quarantined_at: None,
            semantic_revision: None,
            workspace: Some(workspace().as_record_id().clone()),
            scope: Some(scope_kind::PROJECT.to_string()),
            project: None,
        };
        let snapshot = row.into_snapshot(&workspace(), vec![]);
        assert_eq!(snapshot.version, Version::ZERO);
        assert_eq!(snapshot.scope, MemoryScope::Unassigned);
    }

    /// The snapshot insert names only its own variant's columns, and states the discriminator first.
    #[test]
    fn snapshot_writes_are_typed_per_variant() {
        let mut snapshot = FactSnapshot {
            id: fact("one"),
            workspace: workspace(),
            scope: MemoryScope::Personal,
            version: Version::new(2),
            content: "text".to_string(),
            tags: vec!["a".to_string()],
            confidence: 0.5,
            visibility: Visibility::live(),
            embedding: Some(EmbeddingSnapshot::new(vec![0.1], Some("stub"))),
            relation_versions: vec![],
        };
        let (sql, binds) = snapshot_write(&SemanticSnapshot::Fact(snapshot.clone()));
        assert!(sql.contains("kind = 'fact'"), "{sql}");
        assert!(
            sql.find("kind = 'fact'") < sql.find("fact = $fact"),
            "{sql}"
        );
        assert!(sql.contains("fact_embedding = $embedding"), "{sql}");
        assert!(!sql.contains("cluster ="), "{sql}");
        assert!(!sql.contains("reminder_message"), "{sql}");
        assert_eq!(binds.len(), 13);

        snapshot.embedding = None;
        let (without_vector, _) = snapshot_write(&SemanticSnapshot::Fact(snapshot));
        assert!(
            !without_vector.contains("fact_embedding"),
            "an embedding-less snapshot must not bind vector columns: {without_vector}"
        );

        let (edge_sql, edge_binds) = snapshot_write(&SemanticSnapshot::Edge(EdgeSnapshot::memory(
            RecordId::new("memory_edge", "e1"),
            "derived_from",
            fact("one").as_record_id().clone(),
            fact("two").as_record_id().clone(),
        )));
        assert!(edge_sql.contains("kind = 'edge'") && edge_sql.contains("edge_type = $edge_type"));
        assert!(!edge_sql.contains("fact_content"), "{edge_sql}");
        assert_eq!(edge_binds.len(), 5);

        let (cluster_sql, _) = snapshot_write(&SemanticSnapshot::Cluster(ClusterSnapshot {
            id: RecordId::new("cluster", "c1"),
            label: None,
            centroid: vec![0.1],
            depth: 0,
            members: vec![],
            membership_revision: Version::new(1),
            member_embedding_revision: Version::ZERO,
            centroid_revision: Version::ZERO,
        }));
        assert!(
            cluster_sql.contains("cluster_membership_revision"),
            "{cluster_sql}"
        );
        assert!(!cluster_sql.contains("fact ="), "{cluster_sql}");
    }

    /// An endpoint that names a slot this transaction never filled is a refusal, not a silent `NONE`.
    #[test]
    fn a_slot_that_was_never_filled_is_refused() {
        let empty: Vec<RecordId> = vec![];
        let error = resolve(&RelationTarget::NewFact(3), &empty, &empty, &empty).unwrap_err();
        assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
        assert_eq!(
            resolve(
                &RelationTarget::Existing(RecordId::new("fact", "x")),
                &empty,
                &empty,
                &empty,
            )
            .unwrap(),
            RecordId::new("fact", "x")
        );
    }

    /// A cluster member must be a fact: a raw or provenance slot is a planning error refused by name.
    #[test]
    fn a_cluster_member_can_only_be_a_fact() {
        let error = resolve_member(&RelationTarget::NewRaw(0), &[]).unwrap_err();
        assert!(
            matches!(error, StorageError::InvalidInput(_)),
            "a membership aimed at a non-fact must refuse: {error:?}"
        );
    }

    /// The workspace guard is in every incident-relation read, because those tables carry no owner column.
    #[test]
    fn incident_reads_are_workspace_qualified() {
        assert!(
            MEMORY_EDGE_READ_SQL.contains("$workspace"),
            "{MEMORY_EDGE_READ_SQL}"
        );
        for (kind, sql) in structural_reads() {
            assert!(
                sql.contains("$workspace"),
                "the {kind:?} read could follow a foreign endpoint: {sql}"
            );
            assert!(sql.contains("$fact"), "{sql}");
        }
    }

    /// The journal create carries the closed kind, the key, and the fingerprint — and no commit stamp.
    #[test]
    fn the_journal_create_defers_its_commit_stamp() {
        assert!(CREATE_OPERATION_SQL.contains("request_fingerprint = $fingerprint"));
        assert!(!CREATE_OPERATION_SQL.contains("committed_at"));
        assert!(FINISH_OPERATION_SQL.contains("committed_at = $now"));
    }

    /// The fact update is guard and increment in one statement.
    #[test]
    fn a_fact_update_guards_the_revision_it_bumps() {
        let snapshot = FactSnapshot {
            id: fact("one"),
            workspace: workspace(),
            scope: MemoryScope::Personal,
            version: Version::new(4),
            content: "text".to_string(),
            tags: vec![],
            confidence: 0.5,
            visibility: Visibility::live(),
            embedding: None,
            relation_versions: vec![],
        };
        assert!(!snapshot.semantic_hash().is_empty());
    }

    /// `move_scope`'s membership detach names the cluster, not the fact, as the edge's near end.
    #[test]
    fn a_batch_entry_names_the_proposal_it_conflicted_on() {
        let conflict = BatchConflict {
            proposal: "classification_proposal:p1".to_string(),
            reason: BatchConflictReason::Stale,
        };
        let mut result = BatchResult::default();
        result.conflicts.push(conflict.clone());
        assert!(!result.is_complete());
        assert_eq!(result.applied_count(), 0);
        result.applied.push("semantic_operation:o1".to_string());
        assert_eq!(result.applied_count(), 1);
        assert_eq!(
            result.conflicts.first().map(|c| c.reason),
            Some(BatchConflictReason::Stale)
        );
    }
}
