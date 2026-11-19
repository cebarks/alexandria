//! Account-bound refinement policy persistence and admission. No inference or corpus writes.

use std::collections::BTreeSet;
use std::ops::AsyncFn;

use chrono::{DateTime, Utc};
use surrealdb::engine::any::Any;
use surrealdb::method::Transaction;
use surrealdb::types::{AlreadyExistsError, RecordId, SurrealValue, Value};

use super::scope::{GUARD_CONFLICT_ATTEMPTS, claim_authority, is_write_conflict};
use crate::Database;
use crate::access::{OwnerContext, SystemContext, WriteAuthorization, credential_entitles_read};
use crate::error::StorageError;
use crate::models::auth::{Account, Credential};
use crate::models::identity::AccountId;
use crate::models::refinement::{
    ApprovalMode, ApprovalSetting, ConfiguredPolicy, Consent, ConsentChoice, EffectivePolicy,
    InferencePolicyId, PolicyRevision, PolicyVersion, RouteDestinationId,
};

pub struct RefinementRepo<'a> {
    db: &'a Database,
}

impl<'a> RefinementRepo<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
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
