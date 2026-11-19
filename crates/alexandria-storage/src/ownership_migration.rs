//! Offline adoption of a legacy corpus by an explicitly nominated owner. The live
//! connection is the exclusive migration writer; a verified kit is opened only
//! as a separate scratch copy, never to probe the live store's lock.

mod snapshot;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::method::Transaction;
use surrealdb::types::{Datetime, SurrealValue, Value};
use uuid::Uuid;

use crate::access::SystemContext;
use crate::models::PrincipalIds;
use crate::offline_copy::{SourceManifest, VerifiedBackupCopy, read_source_manifest};
use crate::repos::scope::claim_system_for_workspace;
use crate::{Database, RecordId, schema, system_config};

use self::snapshot::{
    Attestation, Owner, Population, Snapshot, adoption_patch, capture, is_sha256, page, row_id,
};

const JOURNAL: &str = "SELECT * FROM ownership_migration LIMIT 2";
const FORMAT_VERSION: u32 = 1;

/// A backup directory and its original offline-copy digest manifest. Loading the
/// sidecar alone does not verify its bytes or its logical match to the live corpus.
#[derive(Debug, Clone)]
pub struct BackupReceipt {
    backup_dir: PathBuf,
    original_files: SourceManifest,
}

impl BackupReceipt {
    pub fn read(backup_dir: &Path) -> Result<Self> {
        Ok(Self {
            backup_dir: backup_dir.to_path_buf(),
            original_files: read_source_manifest(backup_dir).context("Read backup receipt")?,
        })
    }

    pub fn backup_dir(&self) -> &Path {
        &self.backup_dir
    }

    pub fn original_files(&self) -> &SourceManifest {
        &self.original_files
    }

    async fn stage(&self) -> Result<VerifiedBackupCopy> {
        let receipt = self.clone();
        tokio::task::spawn_blocking(move || {
            VerifiedBackupCopy::stage(&receipt.backup_dir, &receipt.original_files)
        })
        .await
        .context("Join backup verification")?
    }
}

/// The stable singleton record and the specific offline run it belongs to. The
/// run nonce prevents a handle for another store's singleton from acting here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationId(RecordId, Uuid);

impl MigrationId {
    pub fn as_record_id(&self) -> &RecordId {
        &self.0
    }
}

#[derive(Debug)]
pub struct Progress {
    pub ready_to_verify: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub rows_migrated: i64,
    pub held_reminders: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReceiptAttestation {
    format_version: u32,
    run: Uuid,
    manifest_sha256: String,
    original: Snapshot,
    expected: Snapshot,
    expected_changes: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    population: Option<Population>,
    /// v009 has dedicated columns for the three primary populations. This is the
    /// typed seek position for the current population, including auxiliary tables.
    cursor: Option<RecordId>,
}

#[derive(SurrealValue)]
struct Journal {
    id: RecordId,
    state: String,
    nominated_owner: Option<RecordId>,
    nominated_workspace: Option<RecordId>,
    backup_receipt: Option<String>,
    fact_cursor: Option<RecordId>,
    session_cursor: Option<RecordId>,
    reminder_cursor: Option<RecordId>,
    rows_migrated: Option<i64>,
    report: Option<String>,
    started_at: Option<Datetime>,
    finished_at: Option<Datetime>,
}

impl Journal {
    fn read(mut rows: Vec<Self>) -> Result<Option<Self>> {
        ensure!(
            rows.len() <= 1,
            "Multiple ownership migration journals; refusing to choose one"
        );
        Ok(rows.pop())
    }

    fn owner(&self) -> Result<Owner> {
        let account = self
            .nominated_owner
            .clone()
            .context("Missing nominated account")?;
        let workspace = self
            .nominated_workspace
            .clone()
            .context("Missing nominated workspace")?;
        ensure!(
            account.table.as_str() == "account"
                && workspace.table.as_str() == "workspace"
                && !account.key.is_range()
                && !workspace.key.is_range(),
            "Invalid nominated owner pair"
        );
        Ok(Owner { account, workspace })
    }

    fn receipt(&self) -> Result<ReceiptAttestation> {
        let receipt: ReceiptAttestation = serde_json::from_str(
            self.backup_receipt
                .as_deref()
                .context("Missing ownership backup attestation")?,
        )
        .context("Malformed ownership backup attestation")?;
        ensure!(
            self.id == journal_id()
                && self.started_at.is_some()
                && receipt.format_version == FORMAT_VERSION
                && !receipt.run.is_nil()
                && is_sha256(&receipt.manifest_sha256)
                && receipt.original.valid()
                && receipt.expected.valid()
                && receipt.expected.rows <= receipt.original.rows
                && u64::try_from(receipt.expected_changes)
                    .is_ok_and(|n| n <= receipt.original.rows)
                && receipt.expected_changes >= self.count()?,
            "Invalid ownership migration attestation"
        );
        Ok(receipt)
    }

    fn count(&self) -> Result<i64> {
        self.rows_migrated
            .filter(|n| *n >= 0)
            .context("Invalid ownership migration row count")
    }

    fn checkpoint(&self) -> Result<Checkpoint> {
        ensure!(
            self.finished_at.is_none()
                && matches!(self.state.as_str(), "running" | "paused" | "verifying"),
            "Ownership migration does not have a resumable checkpoint"
        );
        let checkpoint: Checkpoint = serde_json::from_str(
            self.report
                .as_deref()
                .context("Missing ownership migration checkpoint")?,
        )
        .context("Malformed ownership migration checkpoint")?;
        ensure!(
            (self.state == "verifying") == checkpoint.population.is_none()
                && checkpoint.cursor.as_ref().is_none_or(|id| {
                    !id.key.is_range()
                        && checkpoint.population.is_some_and(|p| {
                            id.table.as_str() == p.table()
                                && match p {
                                    Population::Facts => self.fact_cursor.as_ref() == Some(id),
                                    Population::Sessions => {
                                        self.session_cursor.as_ref() == Some(id)
                                    }
                                    Population::Reminders => {
                                        self.reminder_cursor.as_ref() == Some(id)
                                    }
                                    _ => true,
                                }
                        })
                }),
            "Invalid ownership migration checkpoint"
        );
        Ok(checkpoint)
    }

    fn completed_report(&self, receipt: &ReceiptAttestation) -> Result<Report> {
        ensure!(
            self.state == "complete" && self.finished_at.is_some(),
            "Ownership migration is not complete"
        );
        let report: Report = serde_json::from_str(
            self.report
                .as_deref()
                .context("Missing completed ownership report")?,
        )
        .context("Malformed completed ownership report")?;
        ensure!(
            report.rows_migrated == receipt.expected_changes
                && report.rows_migrated == self.count()?
                && report.held_reminders == receipt.expected.held_reminders,
            "Ownership report disagrees with its attestation"
        );
        Ok(report)
    }
}

pub struct OwnershipMigration<'db> {
    db: &'db Database,
}

impl<'db> OwnershipMigration<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Create the singleton migration only after both backup attestations match,
    /// or resume the exact prior nomination and receipt. Requires exclusive offline access.
    pub async fn begin(
        &self,
        system: &SystemContext,
        nominated: PrincipalIds,
        backup_receipt: BackupReceipt,
    ) -> Result<MigrationId> {
        ensure!(
            self.db.holds_authority(system),
            "Ownership migration requires this store's system authority"
        );
        system_config::stored_embedding_space(self.db.inner()).await?;
        let owner = Owner {
            account: nominated.account.as_record_id().clone(),
            workspace: nominated.workspace.as_record_id().clone(),
        };
        let manifest = &backup_receipt.original_files.sha256;
        // A matching replay still requires the original receipt and verifies the bytes, but never
        // recaptures a logical postimage from the partly adopted live store.
        if let Some(prior) = Journal::read(self.db.inner().query(JOURNAL).await?.check()?.take(0)?)?
        {
            let id = resume(&prior, &owner, manifest)?;
            backup_receipt.stage().await?;
            return Ok(id);
        }
        let stage = backup_receipt.stage().await?;
        let pristine = read_backup(&stage, &owner)
            .await
            .context("Read the verified backup snapshot")?;
        let tx = self.db.inner().clone().begin().await?;
        let result = begin_run(&tx, &owner, manifest, pristine).await;
        finish(tx, result).await
    }

    /// Apply at most `max_rows` changes and checkpoint them in the same transaction.
    pub async fn step(&self, id: &MigrationId, max_rows: usize) -> Result<Progress> {
        ensure!(
            max_rows != 0,
            "Ownership migration requires a nonzero page size"
        );
        let tx = self.db.inner().clone().begin().await?;
        let result = step_run(&tx, id, i64::try_from(max_rows).unwrap_or(i64::MAX)).await;
        finish(tx, result).await
    }

    /// Compare the live corpus to the frozen postimage before publishing Complete.
    /// On mismatch the checkpoint stays unreadable by serving paths.
    pub async fn verify_and_complete(&self, id: &MigrationId) -> Result<Report> {
        system_config::stored_embedding_space(self.db.inner()).await?;
        let tx = self.db.inner().clone().begin().await?;
        let result = verify_run(&tx, id).await;
        finish(tx, result).await
    }
}

fn journal_id() -> RecordId {
    RecordId::new("ownership_migration", "legacy")
}

fn resume(prior: &Journal, owner: &Owner, manifest: &str) -> Result<MigrationId> {
    let seen = prior.owner()?;
    let receipt = prior.receipt()?;
    ensure!(
        seen.account == owner.account && seen.workspace == owner.workspace,
        "Existing ownership migration has a different nominated owner or workspace"
    );
    ensure!(
        receipt.manifest_sha256 == manifest,
        "Existing ownership migration has a different backup receipt"
    );
    if prior.state == "complete" {
        prior.completed_report(&receipt)?;
    } else {
        prior.checkpoint()?;
    }
    Ok(MigrationId(prior.id.clone(), receipt.run))
}

async fn read_backup(stage: &VerifiedBackupCopy, owner: &Owner) -> Result<Attestation> {
    let backup = Database::connect_persistent(stage.path()).await?;
    system_config::stored_embedding_space(backup.inner()).await?;
    // Only the scratch copy is schema-bootstrapped. Compare both logical snapshots
    // in the same schema, without requiring the pre-v009 backup to contain owners.
    schema::migrate(backup.inner()).await?;
    let tx = backup.inner().clone().begin().await?;
    let result = capture(&tx, Some(owner)).await;
    finish(tx, result).await
}

async fn begin_run(
    tx: &Transaction<Any>,
    owner: &Owner,
    manifest: &str,
    pristine: Attestation,
) -> Result<MigrationId> {
    if let Some(prior) = Journal::read(tx.query(JOURNAL).await?.check()?.take(0)?)? {
        return resume(&prior, owner, manifest);
    }
    let live = capture(tx, None)
        .await
        .context("Read live corpus for backup snapshot comparison")?;
    ensure!(
        live.original == pristine.original,
        "Backup snapshot does not match the live legacy corpus"
    );
    claim_owner(tx, owner).await?;
    let id = journal_id();
    let run = Uuid::new_v4();
    let receipt = ReceiptAttestation {
        format_version: FORMAT_VERSION,
        run,
        manifest_sha256: manifest.into(),
        original: pristine.original,
        expected: pristine
            .expected
            .context("Backup snapshot has no ownership postimage")?,
        expected_changes: pristine.changes,
    };
    let checkpoint = Checkpoint {
        population: Some(Population::Facts),
        cursor: None,
    };
    tx.query("CREATE $journal SET state = 'running', nominated_owner = $owner, nominated_workspace = $space, \
        backup_receipt = $receipt, report = $checkpoint, rows_migrated = 0, started_at = time::now() RETURN NONE")
        .bind(("journal", id.clone()))
        .bind(("owner", owner.account.clone()))
        .bind(("space", owner.workspace.clone()))
        .bind(("receipt", serde_json::to_string(&receipt)?))
        .bind(("checkpoint", serde_json::to_string(&checkpoint)?))
        .await?.check()?;
    Ok(MigrationId(id, run))
}

async fn load(tx: &Transaction<Any>, id: &MigrationId) -> Result<(Journal, ReceiptAttestation)> {
    let journal = Journal::read(tx.query(JOURNAL).await?.check()?.take(0)?)?
        .context("Ownership migration journal is missing")?;
    let receipt = journal.receipt()?;
    ensure!(
        id.0 == journal.id && id.1 == receipt.run,
        "Ownership migration handle does not match this run"
    );
    Ok((journal, receipt))
}

async fn claim_owner(tx: &Transaction<Any>, owner: &Owner) -> Result<()> {
    // Pin both the private workspace's owner and that account's active status in the
    // same transaction as the page, rather than trusting a separate lookup.
    let workspace: Vec<Value> = tx
        .query(
            "UPDATE workspace SET updated_at = time::now() \
        WHERE id = $space AND owner = $owner RETURN AFTER",
        )
        .bind(("space", owner.workspace.clone()))
        .bind(("owner", owner.account.clone()))
        .await?
        .check()?
        .take(0)?;
    ensure!(
        workspace.len() == 1 && claim_system_for_workspace(tx, &owner.workspace).await?,
        "Nominated ownership migration principal is inactive or no longer owns the workspace"
    );
    Ok(())
}

async fn step_run(tx: &Transaction<Any>, id: &MigrationId, max_rows: i64) -> Result<Progress> {
    let (journal, receipt) = load(tx, id).await?;
    let owner = journal.owner()?;
    if journal.state == "complete" {
        journal.completed_report(&receipt)?;
        return Ok(Progress {
            ready_to_verify: true,
        });
    }
    let mut checkpoint = journal.checkpoint()?;
    if checkpoint.population.is_none() {
        return Ok(Progress {
            ready_to_verify: true,
        });
    }
    claim_owner(tx, &owner).await?;
    let mut cursors = [
        journal.fact_cursor.clone(),
        journal.session_cursor.clone(),
        journal.reminder_cursor.clone(),
    ];
    let mut remaining = max_rows;
    while let Some(population) = checkpoint.population.filter(|_| remaining > 0) {
        let sql = format!(
            "SELECT * FROM $records WHERE {} ORDER BY id LIMIT $limit",
            population.legacy()
        );
        let rows = page(
            tx,
            &sql,
            population.table(),
            checkpoint.cursor.as_ref(),
            remaining,
        )
        .await
        .with_context(|| format!("Read ownership page for {}", population.table()))?;
        if rows.is_empty() {
            checkpoint = Checkpoint {
                population: population.next(),
                cursor: None,
            };
            continue;
        }
        for row in rows {
            let record = row_id(&row)?;
            write_row(tx, population, record, &row, &owner)
                .await
                .with_context(|| format!("Write ownership page for {}", population.table()))?;
            checkpoint.cursor = Some(record.clone());
            if let Some(index) = match population {
                Population::Facts => Some(0),
                Population::Sessions => Some(1),
                Population::Reminders => Some(2),
                _ => None,
            } {
                cursors[index] = checkpoint.cursor.clone();
            }
            remaining -= 1;
        }
    }
    let count = journal
        .count()?
        .checked_add(max_rows - remaining)
        .context("Ownership migration row count overflow")?;
    ensure!(
        count <= receipt.expected_changes,
        "Ownership migration exceeded its backup snapshot row count"
    );
    let ready_to_verify = checkpoint.population.is_none();
    save(
        tx,
        &journal,
        if ready_to_verify {
            "verifying"
        } else {
            "running"
        },
        count,
        cursors,
        serde_json::to_string(&checkpoint)?,
    )
    .await?;
    Ok(Progress { ready_to_verify })
}

async fn write_row(
    tx: &Transaction<Any>,
    population: Population,
    record: &RecordId,
    row: &surrealdb::types::Object,
    owner: &Owner,
) -> Result<()> {
    let rows: Vec<Value> = if population.is_edge() {
        let sql = format!(
            "DELETE $row WHERE id = $row AND {} RETURN BEFORE",
            population.legacy()
        );
        tx.query(sql)
            .bind(("row", record.clone()))
            .await?
            .check()?
            .take(0)?
    } else {
        let patch = adoption_patch(population, row, owner)?
            .context("Ownership page selected an already owned row")?;
        let sql = format!(
            "UPDATE $row MERGE $patch WHERE id = $row AND {} RETURN AFTER",
            population.legacy()
        );
        tx.query(sql)
            .bind(("row", record.clone()))
            .bind(("patch", patch))
            .await?
            .check()?
            .take(0)?
    };
    ensure!(
        rows.len() == 1,
        "Ownership migration row changed before its guarded write"
    );
    Ok(())
}

async fn verify_run(tx: &Transaction<Any>, id: &MigrationId) -> Result<Report> {
    let (journal, receipt) = load(tx, id).await?;
    let owner = journal.owner()?;
    if journal.state == "complete" {
        return journal.completed_report(&receipt);
    }
    ensure!(
        journal.checkpoint()?.population.is_none() && journal.count()? == receipt.expected_changes,
        "Ownership migration is not ready for verification"
    );
    claim_owner(tx, &owner).await?;
    let actual = capture(tx, None).await?;
    ensure!(
        actual.original == receipt.expected,
        "Ownership migration verification differs from the attested backup postimage"
    );
    let report = Report {
        rows_migrated: receipt.expected_changes,
        held_reminders: receipt.expected.held_reminders,
    };
    save(
        tx,
        &journal,
        "complete",
        report.rows_migrated,
        [
            journal.fact_cursor.clone(),
            journal.session_cursor.clone(),
            journal.reminder_cursor.clone(),
        ],
        serde_json::to_string(&report)?,
    )
    .await?;
    Ok(report)
}

/// The journal is the serialization point: edits and checkpoint/publication commit together.
/// The owner claim also participates in commit conflict detection. As with offline copying,
/// this is not coordination with arbitrary SQL writers; no server may be running on this store.
async fn save(
    tx: &Transaction<Any>,
    prior: &Journal,
    state: &'static str,
    count: i64,
    cursors: [Option<RecordId>; 3],
    payload: String,
) -> Result<()> {
    let [fact, sess, reminder] = cursors;
    let updated: Vec<Value> = tx.query("UPDATE ownership_migration SET state = $next_state, rows_migrated = $next_count, \
        fact_cursor = $fact, session_cursor = $sess, reminder_cursor = $reminder, report = $payload, \
        updated_at = time::now(), finished_at = $finished_at \
        WHERE id = $journal AND state = $seen_state AND rows_migrated = $seen_count AND report = $seen_report \
        AND backup_receipt = $seen_receipt AND nominated_owner = $seen_owner AND nominated_workspace = $seen_space RETURN AFTER")
        .bind(("journal", prior.id.clone()))
        .bind(("next_state", state)).bind(("next_count", count))
        .bind(("finished_at", (state == "complete").then(chrono::Utc::now)))
        .bind(("fact", fact)).bind(("sess", sess)).bind(("reminder", reminder)).bind(("payload", payload))
        .bind(("seen_state", prior.state.clone())).bind(("seen_count", prior.count()?))
        .bind(("seen_report", prior.report.clone())).bind(("seen_receipt", prior.backup_receipt.clone()))
        .bind(("seen_owner", prior.nominated_owner.clone())).bind(("seen_space", prior.nominated_workspace.clone()))
        .await?.check()?.take(0)?;
    ensure!(
        updated.len() == 1,
        "Ownership migration checkpoint is stale"
    );
    Ok(())
}

async fn finish<T>(tx: Transaction<Any>, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            tx.commit()
                .await
                .context("Commit ownership migration transaction")?;
            Ok(value)
        }
        Err(error) => {
            if let Err(cancel) = tx.cancel().await {
                tracing::warn!(%cancel, "Could not cancel ownership migration transaction");
            }
            Err(error)
        }
    }
}

/// Read-only boot admission for a clean fresh/owned store or the single verified migration.
/// Call before provider loading, heat/index writes, or accepting requests.
pub async fn check_ownership_ready(db: &Surreal<Any>) -> Result<()> {
    system_config::stored_embedding_space(db).await?;
    let info: Option<serde_json::Value> =
        db.query("INFO FOR DB STRUCTURE").await?.check()?.take(0)?;
    let info = info.context("Missing database information")?;
    let tables = info["tables"]
        .as_array()
        .context("Missing database table inventory")?;
    let has_table = |name: &str| tables.iter().any(|table| table["name"] == name);
    if has_table("ownership_migration")
        && let Some(journal) = Journal::read(db.query(JOURNAL).await?.check()?.take(0)?)?
    {
        ensure!(
            journal.state == "complete",
            "Ownership migration incomplete; resume it offline before serving"
        );
        journal.owner()?;
        journal.completed_report(&journal.receipt()?)?;
        return Ok(());
    }
    for (table, predicate) in [
        ("fact", "account = NONE OR workspace = NONE OR scope = NONE"),
        ("session", "account = NONE OR workspace = NONE"),
        ("raw", "account = NONE OR workspace = NONE"),
        ("provenance", "account = NONE OR workspace = NONE"),
        ("cluster", "account = NONE OR workspace = NONE"),
        ("reminder", "account = NONE"),
    ] {
        if has_table(table) {
            let rows: Vec<Value> = db
                .query(format!(
                    "SELECT id FROM `{table}` WHERE {predicate} LIMIT 1"
                ))
                .await?
                .check()?
                .take(0)?;
            if !rows.is_empty() {
                bail!("Legacy ownership migration required before serving (unowned {table})");
            }
        }
    }
    Ok(())
}
