//! Account-bound refinement policy persistence and admission. No inference or corpus writes.

use std::collections::BTreeSet;
use std::ops::AsyncFn;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use surrealdb::engine::any::Any;
use surrealdb::method::Transaction;
use surrealdb::types::{AlreadyExistsError, RecordId, SurrealValue, ToSql, Value};

use super::scope::{GUARD_CONFLICT_ATTEMPTS, claim_authority, is_write_conflict};
use crate::Database;
use crate::access::{OwnerContext, SystemContext, WriteAuthorization, credential_entitles_read};
use crate::error::StorageError;
use crate::models::auth::{Account, Credential};
use crate::models::identity::{AccountId, FactId, MemoryScope, OperationId, Partition, Version};
use crate::models::mutation::{EdgeKind, ExpectedState, RelationExpectation};
use crate::models::refinement::{
    ApprovalMode, ApprovalSetting, ApprovedRelationshipEffect, Candidate, ConfiguredPolicy,
    Consent, ConsentChoice, EffectivePolicy, EvidenceBundle, EvidenceSelectionKind,
    InferencePolicyId, PolicyRevision, PolicyVersion, ProposalCategory, ProposalRevision,
    ProposalState, QualityOperation, RouteDestinationId,
};

pub struct RefinementRepo<'a> {
    db: &'a Database,
}

impl<'a> RefinementRepo<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Persist a pending proposal, reusing an existing identical evidence assessment.
    pub async fn persist_proposal(
        &self,
        owner: &OwnerContext,
        proposal: ProposalRevision,
    ) -> Result<ProposalRevision, StorageError> {
        self.persist(owner, proposal, false).await
    }

    pub async fn persist_proposal_revision(
        &self,
        owner: &OwnerContext,
        proposal: ProposalRevision,
    ) -> Result<ProposalRevision, StorageError> {
        self.persist_proposal(owner, proposal).await
    }

    async fn persist(
        &self,
        owner: &OwnerContext,
        proposal: ProposalRevision,
        force_revision: bool,
    ) -> Result<ProposalRevision, StorageError> {
        validate_proposal(owner, &proposal)?;
        let evidence_fingerprint = evidence_fingerprint(&proposal.evidence)?;
        let source_fingerprint = source_fingerprint(&proposal)?;
        let proposal_payload = proposal_payload(&proposal)?;
        self.write(async |tx| {
            claim_owner(tx, owner).await?;
            let matching: Vec<StoredProposal> = tx
                .query("SELECT * FROM refinement_proposal WHERE account = $account AND evidence_fingerprint = $evidence AND source_fingerprint = $sources ORDER BY revision DESC LIMIT 1")
                .bind(("account", owner.account().as_record_id().clone()))
                .bind(("evidence", evidence_fingerprint.clone()))
                .bind(("sources", source_fingerprint.clone()))
                .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
            if !force_revision && let Some(existing) = matching.first() {
                let mut result = proposal.clone();
                result.id = existing.id.clone().ok_or_else(|| {
                    StorageError::Internal(anyhow::anyhow!("proposal row is missing its record id"))
                })?;
                result.revision = Version::new(existing.revision);
                result.state = existing.state;
                result.needs_human = existing.needs_human;
                result.deferred = existing.deferred;
                return Ok(result);
            }
            let revision = if force_revision {
                next_revision(matching.first().ok_or(StorageError::NotFound)?.revision)?
            } else {
                1
            };
            let mut stored_proposal = proposal.clone();
            stored_proposal.revision = Version::new(revision);
            stored_proposal.state = ProposalState::Pending;
            stored_proposal.needs_human = true;
            stored_proposal.deferred = false;
            let existing_evidence = matching.first().map(|row| row.evidence.clone());
            let evidence_id = existing_evidence.clone().unwrap_or_else(|| {
                let evidence_key = sha256(format!("{}:{}:{}", owner.account().as_record_id().to_sql(), evidence_fingerprint, source_fingerprint).as_bytes());
                RecordId::new("refinement_evidence", evidence_key)
            });
            let record = proposal_record(owner, &stored_proposal, evidence_id.clone(), evidence_fingerprint.clone(), source_fingerprint.clone(), proposal_payload.clone());
            if existing_evidence.is_none() {
                let evidence_row = evidence_row(owner, &stored_proposal)?;
                tx.query("CREATE $evidence_id CONTENT $evidence")
                    .bind(("evidence_id", evidence_id))
                    .bind(("evidence", evidence_row.into_value()))
                    .await.map_err(internal)?.check().map_err(internal)?;
            }
            tx.query("CREATE $id CONTENT $record")
                .bind(("id", stored_proposal.id.clone()))
                .bind(("record", record.into_value()))
                .await.map_err(internal)?.check().map_err(internal)?;
            Ok(stored_proposal)
        }).await
    }

    /// Dismiss only the cited pending revision; terminal decisions cannot be overwritten.
    pub async fn dismiss_proposal_revision(
        &self,
        owner: &OwnerContext,
        id: &RecordId,
        revision: Version,
    ) -> Result<(), StorageError> {
        if id.table.as_str() != "refinement_proposal" {
            return Err(StorageError::NotFound);
        }
        let revision = i64::try_from(revision.get())
            .map_err(|_| StorageError::InvalidInput("proposal revision is out of range".into()))?;
        self.write(async |tx| {
            claim_owner(tx, owner).await?;
            let rows: Vec<Value> = tx.query("UPDATE $id SET state = 'dismissed' WHERE account = $account AND revision = $revision AND state = 'pending' RETURN AFTER")
                .bind(("id", id.clone())).bind(("account", owner.account().as_record_id().clone())).bind(("revision", revision))
                .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
            if rows.len() == 1 { Ok(()) } else { Err(StorageError::Stale) }
        }).await
    }

    /// A reviewer alternative is stored as a new pending revision, never as approval.
    pub async fn create_review_alternative(
        &self,
        owner: &OwnerContext,
        id: &RecordId,
        revision: Version,
        mut alternative: ProposalRevision,
    ) -> Result<ProposalRevision, StorageError> {
        if id.table.as_str() != "refinement_proposal" {
            return Err(StorageError::NotFound);
        }
        let revision_i64 = i64::try_from(revision.get())
            .map_err(|_| StorageError::InvalidInput("proposal revision is out of range".into()))?;
        let rows: Vec<StoredProposal> = self.db.inner().query("SELECT * FROM $id WHERE account = $account AND revision = $revision AND state = 'pending'")
            .bind(("id", id.clone())).bind(("account", owner.account().as_record_id().clone())).bind(("revision", revision_i64))
            .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
        let base = rows.into_iter().next().ok_or(StorageError::Stale)?;
        if alternative.id.table.as_str() != "refinement_proposal"
            || alternative.partition.workspace != *owner.workspace()
        {
            return Err(StorageError::InvalidInput(
                "review alternative is outside the owner partition".into(),
            ));
        }
        alternative.revision = Version::new(next_revision(base.revision)?);
        alternative.state = ProposalState::Pending;
        alternative.needs_human = true;
        alternative.deferred = false;
        self.persist(owner, alternative, true).await
    }

    /// Publish operator configuration. A disable is durable but does not delete the last
    /// destinations, consent, or evidence. Even an A -> B -> A transition advances the generation.
    pub async fn reconcile_policy(
        &self,
        system: &SystemContext,
        configuration: Option<&ConfiguredPolicy>,
    ) -> Result<Option<PolicyRevision>, StorageError> {
        self.require_system(system)?;
        if let Some(configuration) = configuration {
            validate_configuration(configuration)?;
        }
        self.write(async |tx| {
            let previous: Option<PolicyRow> = select_record(tx, policy_id()).await?;
            let mut row = previous.clone().unwrap_or_default();
            if row.configured().as_ref().map(|p| &p.configuration) == configuration {
                return Ok(row.configured());
            }
            row.advance(configuration)?;
            let result = row.configured();
            write_record(tx, policy_id(), row, previous.map(|p| p.revision)).await?;
            Ok(result)
        })
        .await
    }

    pub async fn consent(&self, owner: &OwnerContext) -> Result<Option<Consent>, StorageError> {
        Ok(self.effective_policy(owner).await?.consent)
    }

    /// Both revisions are checked inside the write transaction: the current grant/revocation
    /// revision, and (for a grant) the configuration that was actually disclosed to this owner.
    pub async fn set_consent(
        &self,
        owner: &OwnerContext,
        expected_revision: u64,
        choice: ConsentChoice,
        now: DateTime<Utc>,
    ) -> Result<Consent, StorageError> {
        self.write(async |tx| {
            claim_owner(tx, owner).await?;
            let account = owner.account();
            let id = owned_id("refinement_consent", account);
            let previous: Option<ConsentRow> = select_record(tx, id.clone()).await?;
            if let Some(row) = &previous {
                require_row_owner(account, &row.account)?;
            }
            let seen = previous.as_ref().map(|row| row.revision);
            if seen.unwrap_or_default() != expected_revision {
                return Err(StorageError::Stale);
            }
            let row = match choice {
                ConsentChoice::Grant {
                    disclosed_policy_revision,
                } => {
                    disclosed_grant(
                        tx,
                        account,
                        disclosed_policy_revision,
                        expected_revision,
                        now,
                    )
                    .await?
                }
                ConsentChoice::Revoke => {
                    let mut row = previous.ok_or(StorageError::NotFound)?;
                    if row.revoked_at.is_some() {
                        return row.into_consent();
                    }
                    row.revoked_at = Some(now);
                    row.revision = next_revision(row.revision)?;
                    row
                }
            };
            let result = row.clone().into_consent()?;
            write_record(tx, id, row, seen).await?;
            Ok(result)
        })
        .await
    }

    pub async fn approval_mode(
        &self,
        owner: &OwnerContext,
    ) -> Result<ApprovalSetting, StorageError> {
        Ok(self.effective_policy(owner).await?.approval)
    }

    /// The capability selects the account. There is no target parameter or admin override.
    /// A preference is not an admission: selecting SecondaryLlm still cannot send text until
    /// this account explicitly consents, or authorize an already-generated human-mode revision.
    pub async fn set_approval_mode(
        &self,
        owner: &OwnerContext,
        expected_policy: &PolicyVersion,
        mode: ApprovalMode,
    ) -> Result<ApprovalSetting, StorageError> {
        self.write(async |tx| {
            claim_owner(tx, owner).await?;
            let account = owner.account();
            let snapshot = PolicySnapshot::read(tx, account).await?;
            let effective = snapshot.effective()?;
            if effective.version() != *expected_policy {
                return Err(StorageError::Stale);
            }
            if mode == ApprovalMode::SecondaryLlm
                && !effective
                    .configured
                    .as_ref()
                    .is_some_and(|p| p.configuration.allow_automatic)
            {
                return Err(StorageError::Unavailable);
            }
            if mode == effective.approval.mode {
                return Ok(effective.approval);
            }
            pin_configuration(tx, snapshot.policy.as_ref()).await?;
            let setting = ApprovalSetting {
                mode,
                revision: next_revision(effective.approval.revision)?,
            };
            let row = ApprovalRow {
                account: account.as_record_id().clone(),
                mode,
                revision: setting.revision,
            };
            write_record(
                tx,
                owned_id("refinement_approval_mode", account),
                row,
                snapshot.approval.map(|row| row.revision),
            )
            .await?;
            Ok(setting)
        })
        .await
    }

    pub async fn effective_policy(
        &self,
        owner: &OwnerContext,
    ) -> Result<EffectivePolicy, StorageError> {
        self.resolve(owner.account(), Some(owner)).await
    }

    pub async fn scheduled_policy(
        &self,
        system: &SystemContext,
        account: &AccountId,
    ) -> Result<EffectivePolicy, StorageError> {
        self.require_system(system)?;
        self.resolve(account, None).await
    }

    fn require_system(&self, system: &SystemContext) -> Result<(), StorageError> {
        if self.db.holds_authority(system) {
            Ok(())
        } else {
            Err(StorageError::NotFound)
        }
    }

    async fn resolve(
        &self,
        account: &AccountId,
        owner: Option<&OwnerContext>,
    ) -> Result<EffectivePolicy, StorageError> {
        let tx = self.db.inner().clone().begin().await.map_err(internal)?;
        let result = async {
            require_active(&tx, account, owner).await?;
            PolicySnapshot::read(&tx, account).await?.effective()
        }
        .await;
        tx.cancel().await.map_err(internal)?;
        result
    }

    async fn write<T>(
        &self,
        action: impl AsyncFn(&Transaction<Any>) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        for _ in 0..GUARD_CONFLICT_ATTEMPTS {
            let tx = self.db.inner().clone().begin().await.map_err(internal)?;
            let error = match action(&tx).await {
                Ok(output) => match tx.commit().await {
                    Ok(_) => return Ok(output),
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
            // A racing first grant has the same deterministic record key. Retrying the whole
            // transaction re-reads revision 1, so its revision-0 form returns Stale, never upserts.
            tokio::task::yield_now().await;
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }
}

#[derive(Clone, SurrealValue)]
struct ProposalRecord {
    account: RecordId,
    workspace: RecordId,
    project: Option<RecordId>,
    scope: String,
    proposal_key: String,
    revision: u64,
    state: ProposalState,
    needs_human: bool,
    deferred: bool,
    evidence: RecordId,
    origin_policy: AuthorityRecord,
    evidence_fingerprint: String,
    source_fingerprint: String,
    proposal_payload: String,
}

#[derive(Clone, SurrealValue)]
struct StoredProposal {
    id: Option<RecordId>,
    revision: u64,
    state: ProposalState,
    needs_human: bool,
    deferred: bool,
    evidence: RecordId,
}

#[derive(Clone, SurrealValue)]
struct AuthorityRecord {
    account: RecordId,
    consent_revision: u64,
    owner_mode_revision: u64,
    inference_policy_id: String,
    configured_revision: u64,
    mode: ApprovalMode,
}

#[derive(Clone, SurrealValue)]
struct EvidenceRecord {
    account: RecordId,
    workspace: RecordId,
    project: Option<RecordId>,
    scope: String,
    focal: RecordId,
    related: Vec<RecordId>,
    inference_policy_id: String,
    payload: String,
}

#[derive(Clone, Default, SurrealValue)]
struct PolicyRow {
    revision: u64,
    destination_revision: u64,
    destination_set: BTreeSet<RouteDestinationId>,
    inference_policy_id: Option<String>,
    allow_automatic: bool,
    write_fence: bool,
}

impl PolicyRow {
    fn configured(&self) -> Option<PolicyRevision> {
        Some(PolicyRevision {
            revision: self.revision,
            configuration: ConfiguredPolicy {
                destination_set: self.destination_set.clone(),
                inference_policy_id: InferencePolicyId(self.inference_policy_id.clone()?),
                allow_automatic: self.allow_automatic,
            },
        })
    }

    fn advance(&mut self, configuration: Option<&ConfiguredPolicy>) -> Result<(), StorageError> {
        self.revision = next_revision(self.revision)?;
        if let Some(configuration) = configuration {
            if self.destination_set != configuration.destination_set {
                self.destination_revision = next_revision(self.destination_revision)?;
                self.destination_set = configuration.destination_set.clone();
            }
            self.inference_policy_id = Some(configuration.inference_policy_id.0.clone());
            self.allow_automatic = configuration.allow_automatic;
        } else {
            self.inference_policy_id = None;
            self.allow_automatic = false;
        }
        Ok(())
    }
}

#[derive(Clone, SurrealValue)]
struct ConsentRow {
    account: RecordId,
    destination_set: BTreeSet<RouteDestinationId>,
    destination_revision: u64,
    revision: u64,
    granted_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

impl ConsentRow {
    fn into_consent(self) -> Result<Consent, StorageError> {
        Ok(Consent {
            account: AccountId::try_from(self.account)?,
            destination_set: self.destination_set,
            revision: self.revision,
            granted_at: self.granted_at,
            revoked_at: self.revoked_at,
        })
    }
}

#[derive(SurrealValue)]
struct ApprovalRow {
    account: RecordId,
    mode: ApprovalMode,
    revision: u64,
}

struct PolicySnapshot {
    policy: Option<PolicyRow>,
    consent: Option<ConsentRow>,
    approval: Option<ApprovalRow>,
}

impl PolicySnapshot {
    async fn read(tx: &Transaction<Any>, account: &AccountId) -> Result<Self, StorageError> {
        let mut response = tx
            .query("SELECT * FROM $policy; SELECT * FROM $consent; SELECT * FROM $approval;")
            .bind(("policy", policy_id()))
            .bind(("consent", owned_id("refinement_consent", account)))
            .bind(("approval", owned_id("refinement_approval_mode", account)))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let policy: Vec<PolicyRow> = response.take(0).map_err(internal)?;
        let consent: Vec<ConsentRow> = response.take(1).map_err(internal)?;
        let approval: Vec<ApprovalRow> = response.take(2).map_err(internal)?;
        let consent = consent.into_iter().next();
        let approval = approval.into_iter().next();
        for recorded in consent
            .iter()
            .map(|r| &r.account)
            .chain(approval.iter().map(|r| &r.account))
        {
            require_row_owner(account, recorded)?;
        }
        Ok(Self {
            policy: policy.into_iter().next(),
            consent,
            approval,
        })
    }

    fn effective(&self) -> Result<EffectivePolicy, StorageError> {
        let configured = self.policy.as_ref().and_then(PolicyRow::configured);
        let inference_admitted =
            self.policy
                .as_ref()
                .zip(self.consent.as_ref())
                .is_some_and(|(policy, consent)| {
                    configured.is_some()
                        && consent.revoked_at.is_none()
                        && consent.destination_revision == policy.destination_revision
                        && consent.destination_set == policy.destination_set
                });
        let approval = self.approval.as_ref().map_or(
            ApprovalSetting {
                mode: ApprovalMode::Human,
                revision: 0,
            },
            |row| ApprovalSetting {
                mode: row.mode,
                revision: row.revision,
            },
        );
        Ok(EffectivePolicy {
            configured,
            consent: self
                .consent
                .clone()
                .map(ConsentRow::into_consent)
                .transpose()?,
            approval,
            inference_admitted,
        })
    }
}

async fn disclosed_grant(
    tx: &Transaction<Any>,
    account: &AccountId,
    disclosed_revision: u64,
    consent_revision: u64,
    now: DateTime<Utc>,
) -> Result<ConsentRow, StorageError> {
    let policy: PolicyRow = select_record::<PolicyRow>(tx, policy_id())
        .await?
        .ok_or(StorageError::Stale)?;
    if policy.revision != disclosed_revision || policy.inference_policy_id.is_none() {
        return Err(StorageError::Stale);
    }
    pin_configuration(tx, Some(&policy)).await?;
    Ok(ConsentRow {
        account: account.as_record_id().clone(),
        destination_set: policy.destination_set,
        destination_revision: policy.destination_revision,
        revision: next_revision(consent_revision)?,
        granted_at: now,
        revoked_at: None,
    })
}

/// A snapshot read alone does not serialize with a concurrent configuration update. Force a
/// write on the config row for a form that depends on it; the transaction retries on conflict.
async fn pin_configuration(
    tx: &Transaction<Any>,
    policy: Option<&PolicyRow>,
) -> Result<(), StorageError> {
    let seen = policy.map(|row| row.revision);
    let mut fence = policy.cloned().unwrap_or_default();
    fence.write_fence = !fence.write_fence;
    write_record(tx, policy_id(), fence, seen).await
}

async fn require_active(
    tx: &Transaction<Any>,
    account: &AccountId,
    owner: Option<&OwnerContext>,
) -> Result<(), StorageError> {
    let mut response = tx
        .query("SELECT * OMIT password_hash FROM $account")
        .bind(("account", account.as_record_id().clone()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    let accounts: Vec<Account> = response.take(0).map_err(internal)?;
    let row = accounts.into_iter().next().ok_or(StorageError::NotFound)?;
    if !row.is_active() {
        return Err(StorageError::Stale);
    }
    let Some(owner) = owner else {
        return Ok(());
    };
    let (epoch, credential) = match owner.write_authorization() {
        WriteAuthorization::Credential {
            credential_id,
            auth_epoch,
        } => (*auth_epoch, Some(credential_id)),
        WriteAuthorization::TrustedLocal { auth_epoch } => (*auth_epoch, None),
    };
    if row.auth_epoch != Some(epoch) {
        return Err(StorageError::Stale);
    }
    if let Some(credential) = credential {
        require_live_credential(tx, account.as_record_id(), &row, credential.as_record_id())
            .await?;
    }
    Ok(())
}

async fn require_live_credential(
    tx: &Transaction<Any>,
    account_id: &RecordId,
    account: &Account,
    id: &RecordId,
) -> Result<(), StorageError> {
    let mut response = tx
        .query("SELECT * OMIT secret_digest, csrf_digest FROM $id")
        .bind(("id", id.clone()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    let rows: Vec<Credential> = response.take(0).map_err(internal)?;
    let credential = rows.into_iter().next().ok_or(StorageError::Stale)?;
    if credential.account.as_ref() != Some(account_id)
        || !credential_entitles_read(&credential, account, Utc::now())
    {
        return Err(StorageError::Stale);
    }
    Ok(())
}

async fn claim_owner(tx: &Transaction<Any>, owner: &OwnerContext) -> Result<(), StorageError> {
    if claim_authority(tx, owner, Utc::now()).await? {
        Ok(())
    } else {
        Err(StorageError::Stale)
    }
}

async fn select_record<T: SurrealValue>(
    tx: &Transaction<Any>,
    id: RecordId,
) -> Result<Option<T>, StorageError> {
    let rows: Vec<T> = tx
        .query("SELECT * FROM $id")
        .bind(("id", id))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    Ok(rows.into_iter().next())
}

async fn write_record(
    tx: &Transaction<Any>,
    id: RecordId,
    row: impl SurrealValue,
    previous: Option<u64>,
) -> Result<(), StorageError> {
    let query = match previous {
        Some(revision) => tx
            .query("UPDATE $id CONTENT $row WHERE revision = $revision RETURN AFTER")
            .bind(("revision", revision)),
        None => tx.query("CREATE $id CONTENT $row RETURN AFTER"),
    };
    let affected: Vec<Value> = query
        .bind(("id", id))
        .bind(("row", row.into_value()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    if affected.len() == 1 {
        Ok(())
    } else {
        Err(StorageError::Stale)
    }
}

fn operation_is_grounded(
    category: crate::models::refinement::ProposalCategory,
    operation: &QualityOperation,
    visible: &[FactId],
    focal: &FactId,
) -> bool {
    use crate::models::refinement::ProposalCategory as Category;
    use QualityOperation::*;
    match (category, operation) {
        (Category::Rewrite, Rewrite { source, content }) => {
            source == focal && !content.trim().is_empty()
        }
        (Category::Consolidate, Consolidate { canonical, sources }) => {
            let mut unique = sources.clone();
            unique.sort();
            unique.dedup();
            visible.contains(canonical)
                && !sources.is_empty()
                && unique.len() == sources.len()
                && sources
                    .iter()
                    .all(|source| visible.contains(source) && source != canonical)
        }
        (Category::Contradiction, Contradiction { sources }) => {
            let mut unique = sources.clone();
            unique.sort();
            unique.dedup();
            sources.len() >= 2
                && unique.len() == sources.len()
                && sources.iter().all(|source| visible.contains(source))
        }
        (Category::Contradiction, NoSafeChange) => true,
        (Category::Noise, Noise { source }) => source == focal,
        (
            Category::Supersede,
            Supersede {
                source,
                replacement,
            },
        ) => source == focal && replacement != focal && visible.contains(replacement),
        (Category::Split, Split { source, parts }) => {
            source == focal && parts.len() >= 2 && parts.iter().all(|part| !part.trim().is_empty())
        }
        _ => false,
    }
}

fn validate_proposal(
    owner: &OwnerContext,
    proposal: &ProposalRevision,
) -> Result<(), StorageError> {
    if proposal.id.table.as_str() != "refinement_proposal"
        || proposal.partition.workspace != *owner.workspace()
        || proposal.origin_policy.account != *owner.account()
        || proposal.state != ProposalState::Pending
        || proposal.origin_policy.consent_revision == 0
        || proposal.origin_policy.configured_revision == 0
        || proposal.origin_policy.inference_policy_id != proposal.evidence.inference_policy
        || !proposal.evidence.coverage.focal_text_complete
        || proposal.evidence.focal.complete_content.len() > 32 * 1024
        || (matches!(&proposal.partition.scope, MemoryScope::Unassigned)
            && proposal.evidence.coverage.selection != EvidenceSelectionKind::OwnerSelected)
        || proposal.rationale.trim().is_empty()
        || proposal.rationale.len() > 4096
        || proposal.uncertainty.len() > 4096
    {
        return Err(StorageError::InvalidInput(
            "proposal is outside the owner or is not pending".into(),
        ));
    }
    if proposal.evidence.focal.partition != proposal.partition
        || proposal
            .evidence
            .related
            .iter()
            .any(|candidate| candidate.partition != proposal.partition)
        || proposal.sources.iter().any(|source| {
            source.workspace != proposal.partition.workspace
                || source.scope != proposal.partition.scope
        })
    {
        return Err(StorageError::InvalidInput(
            "proposal evidence crosses its authorized partition".into(),
        ));
    }
    let mut visible: Vec<FactId> = std::iter::once(&proposal.evidence.focal)
        .chain(proposal.evidence.related.iter())
        .map(|candidate| candidate.id.clone())
        .collect();
    visible.sort();
    visible.dedup();
    if !operation_is_grounded(
        proposal.category,
        &proposal.proposed,
        &visible,
        &proposal.evidence.focal.id,
    ) {
        return Err(StorageError::InvalidInput(
            "proposal operation is unsupported or names unseen evidence".into(),
        ));
    }
    let mut omitted = BTreeSet::new();
    let omitted_invalid = proposal
        .evidence
        .coverage
        .omitted_neighbors
        .iter()
        .any(|neighbor| {
            let id = neighbor.id.as_record_id().to_sql();
            visible
                .iter()
                .any(|seen| seen.as_record_id().to_sql() == id)
                || !omitted.insert(id)
        });
    if visible.len() != proposal.evidence.related.len() + 1
        || omitted_invalid
        || proposal
            .sources
            .iter()
            .any(|source| !visible.contains(&source.fact))
    {
        return Err(StorageError::InvalidInput(
            "proposal sources are not uniquely present in its evidence".into(),
        ));
    }
    let mut generated = BTreeSet::new();
    let mut unique_effects = BTreeSet::new();
    for effect in &proposal.approved_relationship_effects {
        if !unique_effects.insert(format!("{effect:?}")) {
            return Err(StorageError::InvalidInput(
                "relationship effect is duplicated".into(),
            ));
        }
        match effect {
            ApprovedRelationshipEffect::LinkSourceToSource { source, target } => {
                if !visible.contains(source) || !visible.contains(target) || source == target {
                    return Err(StorageError::InvalidInput(
                        "relationship effect names an unseen source".into(),
                    ));
                }
            }
            ApprovedRelationshipEffect::RemoveSourceMembership { source, cluster }
            | ApprovedRelationshipEffect::AddSourceMembership { source, cluster } => {
                if !visible.contains(source)
                    || cluster.table.as_str() != "cluster"
                    || !cluster_is_in_source_graph(&proposal.sources, source, cluster)
                {
                    return Err(StorageError::InvalidInput(
                        "membership effect names an unseen endpoint".into(),
                    ));
                }
            }
            ApprovedRelationshipEffect::GeneratedEndpoint { operation, slot } => {
                if operation.as_str().trim().is_empty()
                    || !generated.insert((operation.as_str().to_owned(), *slot))
                {
                    return Err(StorageError::InvalidInput(
                        "generated relationship slot is duplicated".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn cluster_is_in_source_graph(
    sources: &[ExpectedState],
    source_id: &FactId,
    cluster: &RecordId,
) -> bool {
    sources.iter().any(|source| {
        source.graph.iter().any(|relation| {
            let (kind, from, to) = match relation {
                RelationExpectation::Present { kind, from, to, .. }
                | RelationExpectation::Absent { kind, from, to } => (kind, from, to),
            };
            matches!(kind, EdgeKind::ClusterMembership)
                && ((from == source_id.as_record_id() && to == cluster)
                    || (to == source_id.as_record_id() && from == cluster))
        })
    })
}

fn evidence_fingerprint(evidence: &EvidenceBundle) -> Result<String, StorageError> {
    let mut related = evidence
        .related
        .iter()
        .map(candidate_payload)
        .collect::<Result<Vec<_>, _>>()?;
    related.sort_by_key(|candidate| candidate["id"].as_str().unwrap_or_default().to_owned());
    let mut omitted = evidence.coverage.omitted_neighbors.iter().map(|item| {
        serde_json::json!({ "id": item.id.as_record_id().to_sql(), "reason": omission_name(item.reason) })
    }).collect::<Vec<_>>();
    omitted.sort_by_key(|item| item["id"].as_str().unwrap_or_default().to_owned());
    let value = serde_json::json!({
        "focal": candidate_payload(&evidence.focal)?,
        "related": related,
        "focal_text_complete": evidence.coverage.focal_text_complete,
        "omitted": omitted,
        "selection": selection_name(evidence.coverage.selection),
        "inference_policy": evidence.inference_policy.0,
    });
    Ok(sha256(&serde_json::to_vec(&value).map_err(|error| {
        StorageError::Internal(anyhow::Error::new(error))
    })?))
}

fn source_fingerprint(proposal: &ProposalRevision) -> Result<String, StorageError> {
    let mut sources = proposal
        .sources
        .iter()
        .map(|source| {
            serde_json::json!({
                "fact": source.fact.as_record_id().to_sql(),
                "workspace": source.workspace.as_record_id().to_sql(),
                "scope": scope_value(&source.scope),
                "version": source.version.get(),
                "semantic_hash": source.semantic_hash,
                "graph": source.graph.iter().map(relation_value).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    sources.sort_by_key(|source| source["fact"].as_str().unwrap_or_default().to_owned());
    let value = serde_json::json!({ "partition": partition_value(&proposal.partition), "sources": sources });
    Ok(sha256(&serde_json::to_vec(&value).map_err(|error| {
        StorageError::Internal(anyhow::Error::new(error))
    })?))
}

fn candidate_payload(candidate: &Candidate) -> Result<serde_json::Value, StorageError> {
    Ok(serde_json::json!({
        "id": candidate.id.as_record_id().to_sql(),
        "partition": partition_value(&candidate.partition),
        "version": candidate.version.get(),
        "complete_content": candidate.complete_content,
        "tags": candidate.tags,
        "provenance": candidate.provenance,
        "semantic_hash": candidate.semantic_hash,
    }))
}

fn proposal_payload(proposal: &ProposalRevision) -> Result<String, StorageError> {
    let value = serde_json::json!({
        "category": category_name(proposal.category),
        "operation": operation_value(&proposal.proposed),
        "sources": proposal.sources.iter().map(source_value).collect::<Vec<_>>(),
        "evidence": {
            "focal": candidate_payload(&proposal.evidence.focal)?,
            "related": proposal.evidence.related.iter().map(candidate_payload).collect::<Result<Vec<_>, _>>()?,
            "coverage": {
                "complete": proposal.evidence.coverage.focal_text_complete,
                "selection": selection_name(proposal.evidence.coverage.selection),
                "omitted": proposal.evidence.coverage.omitted_neighbors.iter().map(|item| serde_json::json!({"id": item.id.as_record_id().to_sql(), "reason": omission_name(item.reason)})).collect::<Vec<_>>(),
            },
            "inference_policy": proposal.evidence.inference_policy.0,
        },
        "relationship_effects": proposal.approved_relationship_effects.iter().map(|effect| effect_value(effect, &proposal.partition.workspace.as_record_id().to_sql())).collect::<Vec<_>>(),
        "rationale": proposal.rationale,
        "uncertainty": proposal.uncertainty,
        "origin_policy": {
            "account": proposal.origin_policy.account.as_record_id().to_sql(),
            "consent_revision": proposal.origin_policy.consent_revision,
            "owner_mode_revision": proposal.origin_policy.owner_mode_revision,
            "inference_policy_id": proposal.origin_policy.inference_policy_id.0,
            "configured_revision": proposal.origin_policy.configured_revision,
            "mode": format!("{:?}", proposal.origin_policy.mode),
        },
    });
    serde_json::to_string(&value).map_err(|error| StorageError::Internal(anyhow::Error::new(error)))
}

fn category_name(category: ProposalCategory) -> &'static str {
    match category {
        ProposalCategory::Rewrite => "rewrite",
        ProposalCategory::Consolidate => "consolidate",
        ProposalCategory::Contradiction => "contradiction",
        ProposalCategory::Noise => "noise",
        ProposalCategory::Supersede => "supersede",
        ProposalCategory::Split => "split",
    }
}

fn source_value(source: &ExpectedState) -> serde_json::Value {
    serde_json::json!({
        "fact": source.fact.as_record_id().to_sql(),
        "workspace": source.workspace.as_record_id().to_sql(),
        "scope": scope_value(&source.scope),
        "version": source.version.get(),
        "semantic_hash": source.semantic_hash,
        "graph": source.graph.iter().map(relation_value).collect::<Vec<_>>(),
    })
}

fn relation_value(relation: &RelationExpectation) -> serde_json::Value {
    let (kind, from, to, edge, present) = match relation {
        RelationExpectation::Present {
            kind,
            from,
            to,
            edge,
        } => (kind, from, to, edge.as_ref(), true),
        RelationExpectation::Absent { kind, from, to } => (kind, from, to, None, false),
    };
    serde_json::json!({
        "kind": kind.effect(),
        "from": from.to_sql(),
        "to": to.to_sql(),
        "edge": edge.map(|id| id.to_sql()),
        "present": present,
    })
}

fn operation_value(operation: &QualityOperation) -> serde_json::Value {
    use QualityOperation::*;
    match operation {
        Rewrite { source, content } => {
            serde_json::json!({"kind":"rewrite","source":source.as_record_id().to_sql(),"content":content})
        }
        Consolidate { canonical, sources } => {
            serde_json::json!({"kind":"consolidate","canonical":canonical.as_record_id().to_sql(),"sources":sources.iter().map(|id| id.as_record_id().to_sql()).collect::<Vec<_>>()})
        }
        Contradiction { sources } => {
            serde_json::json!({"kind":"contradiction","sources":sources.iter().map(|id| id.as_record_id().to_sql()).collect::<Vec<_>>()})
        }
        Noise { source } => {
            serde_json::json!({"kind":"noise","source":source.as_record_id().to_sql()})
        }
        Supersede {
            source,
            replacement,
        } => {
            serde_json::json!({"kind":"supersede","source":source.as_record_id().to_sql(),"replacement":replacement.as_record_id().to_sql()})
        }
        Split { source, parts } => {
            serde_json::json!({"kind":"split","source":source.as_record_id().to_sql(),"parts":parts})
        }
        NoSafeChange => serde_json::json!({"kind":"no_safe_change"}),
    }
}

fn effect_value(effect: &ApprovedRelationshipEffect, workspace: &str) -> serde_json::Value {
    use ApprovedRelationshipEffect::*;
    match effect {
        LinkSourceToSource { source, target } => {
            serde_json::json!({"kind":"link_source_to_source","source":source.as_record_id().to_sql(),"target":target.as_record_id().to_sql()})
        }
        RemoveSourceMembership { source, cluster } => {
            serde_json::json!({"kind":"remove_source_membership","source":source.as_record_id().to_sql(),"cluster":cluster.to_sql()})
        }
        AddSourceMembership { source, cluster } => {
            serde_json::json!({"kind":"add_source_membership","source":source.as_record_id().to_sql(),"cluster":cluster.to_sql()})
        }
        GeneratedEndpoint { operation, slot } => {
            serde_json::json!({"kind":"generated_endpoint","operation":operation.as_str(),"slot":slot,"endpoint":generated_endpoint_id(operation, *slot, workspace).to_sql()})
        }
    }
}

fn generated_endpoint_id(operation: &OperationId, slot: u16, workspace: &str) -> RecordId {
    let material = format!("{workspace}:{}:{slot}", operation.as_str());
    let digest = sha256(material.as_bytes());
    RecordId::new("fact", format!("refinement-{}", &digest[..32]))
}

fn partition_value(partition: &Partition) -> serde_json::Value {
    serde_json::json!({
        "workspace": partition.workspace.as_record_id().to_sql(),
        "scope": scope_value(&partition.scope),
    })
}

fn selection_name(selection: EvidenceSelectionKind) -> &'static str {
    match selection {
        EvidenceSelectionKind::Scheduled => "scheduled",
        EvidenceSelectionKind::OwnerSelected => "owner_selected",
    }
}

fn omission_name(reason: crate::models::refinement::OmissionReason) -> &'static str {
    use crate::models::refinement::OmissionReason::*;
    match reason {
        OutsideSelection => "outside_selection",
        NeighborLimit => "neighbor_limit",
        ByteLimit => "byte_limit",
        Ineligible => "ineligible",
    }
}

fn scope_value(scope: &MemoryScope) -> serde_json::Value {
    match scope {
        MemoryScope::Personal => serde_json::json!({"kind":"personal"}),
        MemoryScope::Project(project) => {
            serde_json::json!({"kind":"project","project":project.as_record_id().to_sql()})
        }
        MemoryScope::Unassigned => serde_json::json!({"kind":"unassigned"}),
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn evidence_row(
    owner: &OwnerContext,
    proposal: &ProposalRevision,
) -> Result<EvidenceRecord, StorageError> {
    let (scope, project) = scope_fields(&proposal.partition.scope);
    let payload = serde_json::json!({
        "focal": candidate_payload(&proposal.evidence.focal)?,
        "related": proposal.evidence.related.iter().map(candidate_payload).collect::<Result<Vec<_>, _>>()?,
        "coverage": {
            "focal_text_complete": proposal.evidence.coverage.focal_text_complete,
            "selection": format!("{:?}", proposal.evidence.coverage.selection),
            "omitted": proposal.evidence.coverage.omitted_neighbors.iter().map(|item| serde_json::json!({"id":item.id.as_record_id().to_sql(),"reason":format!("{:?}",item.reason)})).collect::<Vec<_>>(),
        },
    });
    let payload = serde_json::to_string(&payload)
        .map_err(|error| StorageError::Internal(anyhow::Error::new(error)))?;
    Ok(EvidenceRecord {
        account: owner.account().as_record_id().clone(),
        workspace: owner.workspace().as_record_id().clone(),
        project,
        scope,
        focal: proposal.evidence.focal.id.as_record_id().clone(),
        related: proposal
            .evidence
            .related
            .iter()
            .map(|candidate| candidate.id.as_record_id().clone())
            .collect(),
        inference_policy_id: proposal.evidence.inference_policy.0.clone(),
        payload,
    })
}

fn proposal_record(
    owner: &OwnerContext,
    proposal: &ProposalRevision,
    evidence: RecordId,
    evidence_fingerprint: String,
    source_fingerprint: String,
    proposal_payload: String,
) -> ProposalRecord {
    let (scope, project) = scope_fields(&proposal.partition.scope);
    let proposal_key = sha256(format!("{evidence_fingerprint}:{source_fingerprint}").as_bytes());
    ProposalRecord {
        account: owner.account().as_record_id().clone(),
        workspace: owner.workspace().as_record_id().clone(),
        project,
        scope,
        proposal_key,
        revision: proposal.revision.get(),
        state: proposal.state,
        needs_human: proposal.needs_human,
        deferred: proposal.deferred,
        evidence,
        origin_policy: AuthorityRecord {
            account: proposal.origin_policy.account.as_record_id().clone(),
            consent_revision: proposal.origin_policy.consent_revision,
            owner_mode_revision: proposal.origin_policy.owner_mode_revision,
            inference_policy_id: proposal.origin_policy.inference_policy_id.0.clone(),
            configured_revision: proposal.origin_policy.configured_revision,
            mode: proposal.origin_policy.mode,
        },
        evidence_fingerprint,
        source_fingerprint,
        proposal_payload,
    }
}

fn scope_fields(scope: &MemoryScope) -> (String, Option<RecordId>) {
    match scope {
        MemoryScope::Personal => ("personal".into(), None),
        MemoryScope::Project(project) => ("project".into(), Some(project.as_record_id().clone())),
        MemoryScope::Unassigned => ("unassigned".into(), None),
    }
}

fn owned_id(table: &str, account: &AccountId) -> RecordId {
    RecordId::new(table, account.as_record_id().key.clone())
}

fn policy_id() -> RecordId {
    RecordId::new("refinement_policy", "current")
}

fn require_row_owner(expected: &AccountId, recorded: &RecordId) -> Result<(), StorageError> {
    if expected.as_record_id() == recorded {
        Ok(())
    } else {
        Err(StorageError::Internal(anyhow::anyhow!(
            "refinement policy row does not belong to its account key"
        )))
    }
}

fn validate_configuration(configuration: &ConfiguredPolicy) -> Result<(), StorageError> {
    let fingerprint = &configuration.inference_policy_id.0;
    if !(1..=2).contains(&configuration.destination_set.len())
        || fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StorageError::InvalidInput(
            "invalid refinement destination set or inference fingerprint".into(),
        ));
    }
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, StorageError> {
    // Surreal `int` is a signed 64-bit value. Never wrap onto an earlier authority generation.
    revision
        .checked_add(1)
        .filter(|next| i64::try_from(*next).is_ok())
        .ok_or_else(|| {
            StorageError::Internal(anyhow::anyhow!("refinement revision space exhausted"))
        })
}

fn internal(error: surrealdb::Error) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

fn can_retry(error: &StorageError) -> bool {
    if let StorageError::Internal(cause) = error
        && let Some(error) = cause.downcast_ref::<surrealdb::Error>()
    {
        return is_write_conflict(error)
            || matches!(
                error.already_exists_details(),
                Some(AlreadyExistsError::Record { .. })
            );
    }
    false
}
