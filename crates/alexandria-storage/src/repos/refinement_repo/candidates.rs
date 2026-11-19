use std::ops::Bound;

use chrono::{DateTime, Utc};
use surrealdb::engine::any::Any;
use surrealdb::method::{Query, Transaction};
use surrealdb::types::{RecordId, RecordIdKeyRange, SurrealValue};

use super::{
    PolicyRow, RefinementRepo, candidate_payload, evidence_fingerprint, internal, next_revision,
    partition_value, policy_id, require_active, scope_fields, select_record, sha256, write_record,
};
use crate::access::{OwnerContext, ScopedContext, SystemContext};
use crate::error::StorageError;
use crate::models::Provenance;
use crate::models::identity::{AccountId, FactId, MemoryScope, Partition, Version};
use crate::models::refinement::{
    Candidate, EvidenceBundle, EvidenceCoverage, EvidenceSelectionKind, InferencePolicyId,
    OmissionReason, OmittedNeighbor, PolicyRevision,
};

const MAX_PAGE_ROWS: usize = 100;
const SCHEDULED_NEIGHBORS: usize = 8;
const MAX_SELECTED_NEIGHBORS: usize = 64;
const MAX_EVIDENCE_BYTES: usize = 32 * 1024;
const PROVENANCE_PROBE_LIMIT: usize = 33;
const LIVE_PARTITION: &str = "account = $account AND workspace = $workspace AND scope = $scope \
    AND project = $project AND deleted = false AND quarantined_at = NONE";
const UNASSIGNED_REVIEW_PARTITION: &str = "account = $account AND workspace = $workspace \
    AND scope = 'unassigned' AND project = $project AND deleted = false";

/// A checkpointable position in one partition. `after` is the exclusive lower bound of
/// the next typed fact-key range; it is not a display handle or an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateCursor {
    pub partition: Partition,
    pub after: FactId,
}

#[derive(Debug, PartialEq)]
pub struct CandidatePage {
    pub outcomes: Vec<EvidenceOutcome>,
    /// Advance past scanned rows even when they yield no assessment. `None` means exhausted.
    pub next: Option<CandidateCursor>,
}

/// The last acknowledged position, separate from the read so an interrupted consumer can replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateCheckpoint {
    pub position: Option<CandidateCursor>,
    pub revision: Version,
}

/// Explicit owner selections: no automatic neighbor search may expand this set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceSelection {
    pub focal: FactId,
    pub related: Vec<FactId>,
}

#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // The bundle is the common result; keep it inline.
pub enum EvidenceOutcome {
    Ready(EvidenceBundle),
    NeedsContext {
        focal: FactId,
        coverage: EvidenceCoverage,
    },
}

impl RefinementRepo<'_> {
    /// Walk one active account's assigned partition without recording retrieval or access.
    /// `limit` bounds focal rows visited, not assessments produced. Recent unchanged assessments
    /// or a reverse pair can yield an empty page with a non-empty continuation.
    pub async fn page_candidates(
        &self,
        partition: &Partition,
        cursor: Option<&CandidateCursor>,
        limit: usize,
        eligible_before: DateTime<Utc>,
    ) -> Result<CandidatePage, StorageError> {
        validate_cursor(partition, cursor)?;
        let tx = self.db.inner().clone().begin().await.map_err(internal)?;
        let result = page_in_tx(&tx, partition, cursor, limit, eligible_before).await;
        tx.cancel().await.map_err(internal)?;
        result
    }

    /// Read explicitly selected Unassigned facts through the owner's review context only.
    pub async fn evidence(
        &self,
        (owner, review): (&OwnerContext, &ScopedContext),
        selection: &EvidenceSelection,
        neighbor_limit: usize,
        max_bytes: usize,
    ) -> Result<EvidenceOutcome, StorageError> {
        let [partition] = review.partitions() else {
            return Err(StorageError::NotFound);
        };
        if partition.scope != MemoryScope::Unassigned || &partition.workspace != owner.workspace() {
            return Err(StorageError::NotFound);
        }
        let ids = selected_ids(selection)?;
        let tx = self.db.inner().clone().begin().await.map_err(internal)?;
        let result = evidence_in_tx(
            &tx,
            owner,
            partition,
            selection,
            ids,
            (neighbor_limit, max_bytes),
        )
        .await;
        tx.cancel().await.map_err(internal)?;
        result
    }

    /// Load the durable position for an assigned partition. Only an explicit acknowledgement writes it.
    pub async fn candidate_checkpoint(
        &self,
        system: &SystemContext,
        partition: &Partition,
    ) -> Result<CandidateCheckpoint, StorageError> {
        self.require_system(system)?;
        let tx = self.db.inner().clone().begin().await.map_err(internal)?;
        let result = load_checkpoint(&tx, partition).await;
        tx.cancel().await.map_err(internal)?;
        result
    }

    /// Compare-and-swap the caller's last acknowledged position. `None` closes the current walk.
    pub async fn save_candidate_checkpoint(
        &self,
        system: &SystemContext,
        partition: &Partition,
        expected_revision: Version,
        position: Option<&CandidateCursor>,
    ) -> Result<CandidateCheckpoint, StorageError> {
        self.require_system(system)?;
        validate_cursor(partition, position)?;
        let db = self.db.inner().clone();
        let partition = partition.clone();
        let position = position.cloned();
        for _ in 0..super::GUARD_CONFLICT_ATTEMPTS {
            let tx = db.clone().begin().await.map_err(internal)?;
            let result =
                save_checkpoint(&tx, &partition, expected_revision, position.as_ref()).await;
            match super::finish_write(tx, result.is_ok()).await.and(result) {
                Ok(checkpoint) => return Ok(checkpoint),
                Err(error) if super::can_retry(&error) => tokio::task::yield_now().await,
                Err(error) => return Err(error),
            }
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }
}

async fn page_in_tx(
    tx: &Transaction<Any>,
    partition: &Partition,
    cursor: Option<&CandidateCursor>,
    limit: usize,
    eligible_before: DateTime<Utc>,
) -> Result<CandidatePage, StorageError> {
    let account = assigned_account(tx, partition).await?;
    let Some(policy) = configured_policy(tx).await? else {
        return Ok(CandidatePage {
            outcomes: vec![],
            next: None,
        });
    };
    Population {
        tx,
        account: &account,
        partition,
    }
    .page(cursor, limit, eligible_before, &policy)
    .await
}

async fn evidence_in_tx(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    partition: &Partition,
    selection: &EvidenceSelection,
    ids: Vec<FactId>,
    limits: (usize, usize),
) -> Result<EvidenceOutcome, StorageError> {
    let account = workspace_owner(tx, partition).await?;
    if &account != owner.account() {
        return Err(StorageError::NotFound);
    }
    require_active(tx, &account, Some(owner)).await?;
    let population = Population {
        tx,
        account: &account,
        partition,
    };
    population.require_selected(&ids).await?;
    let policy = configured_policy(tx)
        .await?
        .ok_or(StorageError::Unavailable)?;
    population
        .project(
            &selection.focal,
            &selection.related,
            limits.0,
            limits.1.min(MAX_EVIDENCE_BYTES),
            EvidenceSelectionKind::OwnerSelected,
            &policy.configuration.inference_policy_id,
        )
        .await
}

async fn load_checkpoint(
    tx: &Transaction<Any>,
    partition: &Partition,
) -> Result<CandidateCheckpoint, StorageError> {
    let account = assigned_account(tx, partition).await?;
    let (_, checkpoint) = checkpoint(tx, &account, partition).await?;
    Ok(checkpoint)
}

async fn save_checkpoint(
    tx: &Transaction<Any>,
    partition: &Partition,
    expected_revision: Version,
    position: Option<&CandidateCursor>,
) -> Result<CandidateCheckpoint, StorageError> {
    let account = assigned_account(tx, partition).await?;
    let (id, previous) = checkpoint(tx, &account, partition).await?;
    if previous.revision != expected_revision {
        return Err(StorageError::Stale);
    }
    let revision = next_revision(previous.revision.get())?;
    let (scope, project) = scope_fields(&partition.scope);
    let row = CheckpointRow {
        account: account.as_record_id().clone(),
        workspace: partition.workspace.as_record_id().clone(),
        scope,
        project,
        after_fact: position.map(|cursor| cursor.after.as_record_id().clone()),
        revision,
    };
    let seen = (previous.revision != Version::ZERO).then_some(previous.revision.get());
    write_record(tx, id, row, seen).await?;
    Ok(CandidateCheckpoint {
        position: position.cloned(),
        revision: Version::new(revision),
    })
}

struct Population<'a> {
    tx: &'a Transaction<Any>,
    account: &'a AccountId,
    partition: &'a Partition,
}

impl Population<'_> {
    fn query(&self, statement: impl Into<String>) -> Query<'_, Any> {
        let (scope, project) = scope_fields(&self.partition.scope);
        let statement = statement.into();
        self.tx
            .query(statement)
            .bind(("account", self.account.as_record_id().clone()))
            .bind(("workspace", self.partition.workspace.as_record_id().clone()))
            .bind(("scope", scope))
            .bind(("project", project))
    }

    async fn page(
        &self,
        cursor: Option<&CandidateCursor>,
        limit: usize,
        eligible_before: DateTime<Utc>,
        policy: &PolicyRevision,
    ) -> Result<CandidatePage, StorageError> {
        let limit = limit.min(MAX_PAGE_ROWS);
        if limit == 0 {
            return Err(StorageError::InvalidInput(
                "refinement candidate page limit must be positive".into(),
            ));
        }
        let query = self
            .query(format!(
                "SELECT VALUE id FROM $records WHERE {LIVE_PARTITION} ORDER BY id LIMIT $limit"
            ))
            .bind(("records", fact_range(cursor)))
            .bind(("limit", limit + 1));
        let mut ids = fact_ids(query).await?;
        let has_more = ids.len() > limit;
        ids.truncate(limit);
        let next = if has_more {
            ids.last().cloned().map(|after| CandidateCursor {
                partition: self.partition.clone(),
                after,
            })
        } else {
            None
        };
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(outcome) = self.assess(id, eligible_before, policy).await? {
                outcomes.push(outcome);
            }
        }
        Ok(CandidatePage { outcomes, next })
    }

    async fn assess(
        &self,
        id: FactId,
        eligible_before: DateTime<Utc>,
        policy: &PolicyRevision,
    ) -> Result<Option<EvidenceOutcome>, StorageError> {
        let kind = EvidenceSelectionKind::Scheduled;
        let Some(focal) = self.candidate(&id, MAX_EVIDENCE_BYTES).await? else {
            return Ok(Some(needs_context(&id, kind)));
        };
        let neighbors = self.neighbors(&id).await?;
        let outcome = attach_related(
            self,
            focal,
            &neighbors,
            SCHEDULED_NEIGHBORS,
            MAX_EVIDENCE_BYTES,
            kind,
            &policy.configuration.inference_policy_id,
        )
        .await?;
        if let EvidenceOutcome::Ready(bundle) = &outcome
            && (self.reverse_pair(bundle).await?
                || self
                    .recent(bundle, eligible_before, policy.revision)
                    .await?)
        {
            return Ok(None);
        }
        Ok(Some(outcome))
    }

    async fn project(
        &self,
        id: &FactId,
        neighbors: &[FactId],
        neighbor_limit: usize,
        max_bytes: usize,
        kind: EvidenceSelectionKind,
        policy: &InferencePolicyId,
    ) -> Result<EvidenceOutcome, StorageError> {
        let Some(focal) = self.candidate(id, max_bytes).await? else {
            return Ok(needs_context(id, kind));
        };
        attach_related(
            self,
            focal,
            neighbors,
            neighbor_limit,
            max_bytes,
            kind,
            policy,
        )
        .await
    }

    async fn candidate(
        &self,
        id: &FactId,
        budget: usize,
    ) -> Result<Option<SizedCandidate>, StorageError> {
        // Length in SQL is a cheap prefilter. The UTF-8/metadata budget below is exact for the
        // projected payload, and does not depend on SurrealQL's string length units.
        // The only Unassigned population is the owner-selected review context. Assigned
        // candidates must still exclude quarantine, regardless of how they were selected.
        let predicate = if self.partition.scope == MemoryScope::Unassigned {
            UNASSIGNED_REVIEW_PARTITION
        } else {
            LIVE_PARTITION
        };
        let rows: Vec<CandidateRow> = rows(
            self.query(format!(
                "SELECT id, content, tags, semantic_revision FROM $fact WHERE {predicate} \
             AND string::len(content) <= $budget AND array::len(tags) <= 256"
            ))
            .bind(("fact", id.as_record_id().clone()))
            .bind(("budget", budget)),
        )
        .await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        if row.content.len() > budget {
            return Ok(None);
        }
        let provenance: Vec<Provenance> = rows_for_provenance(self, id).await?;
        if provenance.len() == PROVENANCE_PROBE_LIMIT {
            return Ok(None);
        }
        let mut candidate = Candidate {
            id: FactId::try_from(row.id)?,
            partition: self.partition.clone(),
            version: Version::new(row.semantic_revision.unwrap_or_default()),
            complete_content: row.content,
            tags: row.tags,
            provenance,
            semantic_hash: String::new(),
        };
        // Fingerprint the evidence projection, not the mutation journal's full preimage; embeddings
        // and unrelated graph rows are not evidence and must not be loaded just to hash it.
        let fingerprint = payload_bytes(&candidate)?;
        candidate.semantic_hash = sha256(&fingerprint);
        let bytes = fingerprint.len() + candidate.semantic_hash.len();
        Ok((bytes <= budget).then_some(SizedCandidate { candidate, bytes }))
    }

    async fn require_selected(&self, ids: &[FactId]) -> Result<(), StorageError> {
        let records: Vec<_> = ids.iter().map(|id| id.as_record_id().clone()).collect();
        let found = fact_ids(
            self.query(format!(
                "SELECT VALUE id FROM $facts WHERE {UNASSIGNED_REVIEW_PARTITION}"
            ))
            .bind(("facts", records)),
        )
        .await?;
        if found.len() == ids.len() {
            Ok(())
        } else {
            Err(StorageError::NotFound)
        }
    }

    async fn reverse_pair(&self, evidence: &EvidenceBundle) -> Result<bool, StorageError> {
        // Canonicalize only actually reciprocal pairs with no omitted context. A lower-ID
        // neighbor that doesn't select us back must not erase this assessment.
        let [related] = evidence.related.as_slice() else {
            return Ok(false);
        };
        if related.id >= evidence.focal.id || !evidence.coverage.omitted_neighbors.is_empty() {
            return Ok(false);
        }
        Ok(self.neighbors(&related.id).await? == [evidence.focal.id.clone()])
    }

    async fn recent(
        &self,
        evidence: &EvidenceBundle,
        before: DateTime<Utc>,
        generation: u64,
    ) -> Result<bool, StorageError> {
        let found: Vec<RecordId> = rows(self.query(
            "SELECT VALUE id FROM refinement_proposal WHERE account = $account AND workspace = $workspace \
             AND scope = $scope AND project = $project AND evidence_fingerprint = $evidence \
             AND origin_policy.configured_revision = $generation AND created_at > $before LIMIT 1")
            .bind(("evidence", evidence_fingerprint(evidence)?))
            .bind(("generation", generation)).bind(("before", before))).await?;
        Ok(!found.is_empty())
    }

    async fn neighbors(&self, focal: &FactId) -> Result<Vec<FactId>, StorageError> {
        // One extra visible ID discloses a limit without pretending the whole neighborhood was read.
        let probe = SCHEDULED_NEIGHBORS + 1;
        let mut related = self.graph_and_session_neighbors(focal, probe).await?;
        if related.len() < probe {
            let similar = self
                .similar_neighbors(focal, &related, probe - related.len())
                .await?;
            related.extend(similar);
        }
        Ok(related)
    }

    async fn graph_and_session_neighbors(
        &self,
        focal: &FactId,
        limit: usize,
    ) -> Result<Vec<FactId>, StorageError> {
        fact_ids(self.query(format!(
            "SELECT VALUE id FROM fact WHERE {LIVE_PARTITION} AND id != $focal AND (\
                id INSIDE (SELECT VALUE out FROM memory_edge WHERE in = $focal) OR \
                id INSIDE (SELECT VALUE in FROM memory_edge WHERE out = $focal) OR \
                id INSIDE (SELECT VALUE out FROM contains_session_memory WHERE in INSIDE (\
                    SELECT VALUE id FROM `session` WHERE account = $account AND workspace = $workspace \
                    AND id INSIDE (SELECT VALUE in FROM contains_session_memory WHERE out = $focal)))) \
             ORDER BY id LIMIT $limit"))
            .bind(("focal", focal.as_record_id().clone())).bind(("limit", limit))).await
    }

    async fn similar_neighbors(
        &self,
        focal: &FactId,
        excluded: &[FactId],
        limit: usize,
    ) -> Result<Vec<FactId>, StorageError> {
        let vectors: Vec<Vec<f32>> = rows(
            self.query(format!(
                "SELECT VALUE embedding FROM $focal WHERE {LIVE_PARTITION}"
            ))
            .bind(("focal", focal.as_record_id().clone())),
        )
        .await?;
        let Some(vector) = vectors.into_iter().next() else {
            return Ok(vec![]);
        };
        if !vector.iter().any(|value| *value != 0.0)
            || vector.iter().any(|value| !value.is_finite())
        {
            return Ok(vec![]);
        }
        let dimensions = vector.len();
        let skip: Vec<_> = std::iter::once(focal)
            .chain(excluded)
            .map(|id| id.as_record_id().clone())
            .collect();
        let hits: Vec<NeighborScore> = rows(
            self.query(format!(
                "SELECT id, vector::similarity::cosine(embedding, $vector) AS score FROM fact \
             WHERE {LIVE_PARTITION} AND NOT (id INSIDE $skip) AND array::len(embedding) = $dimensions \
             AND vector::magnitude(embedding) > 0 ORDER BY score DESC, id ASC LIMIT $limit"
            ))
            .bind(("skip", skip))
            .bind(("dimensions", dimensions))
            .bind(("vector", vector))
            .bind(("limit", limit)),
        )
        .await?;
        hits.into_iter()
            .map(|hit| FactId::try_from(hit.id))
            .collect()
    }
}

async fn attach_related(
    population: &Population<'_>,
    focal: SizedCandidate,
    neighbors: &[FactId],
    limit: usize,
    max_bytes: usize,
    kind: EvidenceSelectionKind,
    policy: &InferencePolicyId,
) -> Result<EvidenceOutcome, StorageError> {
    let mut remaining = max_bytes - focal.bytes;
    let mut related = Vec::with_capacity(neighbors.len().min(limit));
    let mut omitted_neighbors = Vec::new();
    for (index, id) in neighbors.iter().enumerate() {
        let reason = if index >= limit {
            OmissionReason::NeighborLimit
        } else if let Some(projected) = population.candidate(id, remaining).await? {
            remaining -= projected.bytes;
            related.push(projected.candidate);
            continue;
        } else {
            OmissionReason::ByteLimit
        };
        omitted_neighbors.push(OmittedNeighbor {
            id: id.clone(),
            reason,
        });
    }
    Ok(EvidenceOutcome::Ready(EvidenceBundle {
        focal: focal.candidate,
        related,
        coverage: EvidenceCoverage {
            focal_text_complete: true,
            omitted_neighbors,
            selection: kind,
        },
        inference_policy: policy.clone(),
    }))
}

#[derive(SurrealValue)]
struct CandidateRow {
    id: RecordId,
    content: String,
    tags: Vec<String>,
    semantic_revision: Option<u64>,
}

#[derive(SurrealValue)]
struct NeighborScore {
    id: RecordId,
    #[surreal(rename = "score")]
    _score: f64,
}

struct SizedCandidate {
    candidate: Candidate,
    bytes: usize,
}

#[derive(SurrealValue)]
struct CheckpointRow {
    account: RecordId,
    workspace: RecordId,
    scope: String,
    project: Option<RecordId>,
    after_fact: Option<RecordId>,
    revision: u64,
}

async fn checkpoint(
    tx: &Transaction<Any>,
    account: &AccountId,
    partition: &Partition,
) -> Result<(RecordId, CandidateCheckpoint), StorageError> {
    let fingerprint = serde_json::to_vec(&partition_value(partition))
        .map_err(|error| StorageError::Internal(error.into()))?;
    let id = RecordId::new("refinement_candidate_progress", sha256(&fingerprint));
    let Some(row) = select_record::<CheckpointRow>(tx, id.clone()).await? else {
        return Ok((
            id,
            CandidateCheckpoint {
                position: None,
                revision: Version::ZERO,
            },
        ));
    };
    let (scope, project) = scope_fields(&partition.scope);
    if &row.account != account.as_record_id()
        || &row.workspace != partition.workspace.as_record_id()
        || row.scope != scope
        || row.project != project
    {
        return Err(StorageError::Stale);
    }
    let position = row
        .after_fact
        .map(FactId::try_from)
        .transpose()?
        .map(|after| CandidateCursor {
            partition: partition.clone(),
            after,
        });
    validate_cursor(partition, position.as_ref())?;
    Ok((
        id,
        CandidateCheckpoint {
            position,
            revision: Version::new(row.revision),
        },
    ))
}

async fn rows_for_provenance(
    population: &Population<'_>,
    id: &FactId,
) -> Result<Vec<Provenance>, StorageError> {
    rows(population.query(
        "SELECT id, kind, session_id, agent_id, model, timestamp FROM provenance \
         WHERE account = $account AND workspace = $workspace \
         AND id INSIDE (SELECT VALUE out FROM has_provenance WHERE in = $fact) ORDER BY id LIMIT $limit")
        .bind(("fact", id.as_record_id().clone())).bind(("limit", PROVENANCE_PROBE_LIMIT))).await
}

fn payload_bytes(candidate: &Candidate) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(&candidate_payload(candidate)?)
        .map_err(|error| StorageError::Internal(error.into()))
}

fn needs_context(id: &FactId, selection: EvidenceSelectionKind) -> EvidenceOutcome {
    EvidenceOutcome::NeedsContext {
        focal: id.clone(),
        coverage: EvidenceCoverage {
            focal_text_complete: false,
            omitted_neighbors: vec![],
            selection,
        },
    }
}

async fn assigned_account(
    tx: &Transaction<Any>,
    partition: &Partition,
) -> Result<AccountId, StorageError> {
    if partition.scope == MemoryScope::Unassigned {
        return Err(StorageError::NotFound);
    }
    let account = workspace_owner(tx, partition).await?;
    require_active(tx, &account, None).await?;
    if let MemoryScope::Project(project) = &partition.scope {
        let matches: Vec<RecordId> = rows(
            tx.query("SELECT VALUE id FROM $project WHERE workspace = $workspace")
                .bind(("project", project.as_record_id().clone()))
                .bind(("workspace", partition.workspace.as_record_id().clone())),
        )
        .await?;
        if matches.is_empty() {
            return Err(StorageError::NotFound);
        }
    }
    Ok(account)
}

async fn workspace_owner(
    tx: &Transaction<Any>,
    partition: &Partition,
) -> Result<AccountId, StorageError> {
    let owners: Vec<RecordId> = rows(
        tx.query("SELECT VALUE owner FROM $workspace")
            .bind(("workspace", partition.workspace.as_record_id().clone())),
    )
    .await?;
    AccountId::try_from(owners.into_iter().next().ok_or(StorageError::NotFound)?)
}

async fn configured_policy(tx: &Transaction<Any>) -> Result<Option<PolicyRevision>, StorageError> {
    Ok(select_record::<PolicyRow>(tx, policy_id())
        .await?
        .and_then(|row| row.configured()))
}

fn validate_cursor(
    partition: &Partition,
    cursor: Option<&CandidateCursor>,
) -> Result<(), StorageError> {
    if let Some(cursor) = cursor
        && (&cursor.partition != partition || cursor.after.as_record_id().key.is_range())
    {
        return Err(StorageError::InvalidInput(
            "invalid refinement partition cursor".into(),
        ));
    }
    Ok(())
}

fn fact_range(cursor: Option<&CandidateCursor>) -> RecordId {
    RecordId::new(
        "fact",
        RecordIdKeyRange {
            start: cursor.map_or(Bound::Unbounded, |cursor| {
                Bound::Excluded(cursor.after.as_record_id().key.clone())
            }),
            end: Bound::Unbounded,
        },
    )
}

fn selected_ids(selection: &EvidenceSelection) -> Result<Vec<FactId>, StorageError> {
    if selection.related.len() > MAX_SELECTED_NEIGHBORS {
        return Err(StorageError::InvalidInput(
            "too many explicit neighbors".into(),
        ));
    }
    // Sort only the IDs used for validation; projection uses the owner's original order.
    let mut ids: Vec<_> = std::iter::once(&selection.focal)
        .chain(&selection.related)
        .cloned()
        .collect();
    let requested = ids.len();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != requested || ids.iter().any(|id| id.as_record_id().key.is_range()) {
        return Err(StorageError::InvalidInput(
            "evidence requires unique point IDs".into(),
        ));
    }
    Ok(ids)
}

async fn fact_ids(query: Query<'_, Any>) -> Result<Vec<FactId>, StorageError> {
    let ids: Vec<RecordId> = rows(query).await?;
    ids.into_iter().map(FactId::try_from).collect()
}

async fn rows<T: SurrealValue>(query: Query<'_, Any>) -> Result<Vec<T>, StorageError> {
    query
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)
}
