//! Exact logical attestation of the legacy corpus, separate from the file receipt.

use std::ops::Bound;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use surrealdb::engine::any::Any;
use surrealdb::method::Transaction;
use surrealdb::types::{Object, RecordIdKeyRange, ToSql, Value};

use crate::RecordId;

const ATTESTATION_PAGE: i64 = 64;
// Coordination and enrollment tables (including the bootstrap journal entries) aren't legacy
// content. Retain the system_config payload except for the schema version stamped on upgrade.
const PRESERVED: &[&str] = &[
    "heat_state",
    "has_provenance",
    "extracted_from",
    "contains_session_memory",
    "memory_edge",
    "maintenance_log",
    "embedding_config",
    "system_config",
    "cluster",
];

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) enum Population {
    Facts,
    Sessions,
    Reminders,
    Raw,
    Provenance,
    Memberships,
    Hierarchy,
}

impl Population {
    pub(super) const ALL: [Self; 7] = [
        Self::Facts,
        Self::Sessions,
        Self::Reminders,
        Self::Raw,
        Self::Provenance,
        Self::Memberships,
        Self::Hierarchy,
    ];

    pub(super) fn table(self) -> &'static str {
        match self {
            Self::Facts => "fact",
            Self::Sessions => "session",
            Self::Reminders => "reminder",
            Self::Raw => "raw",
            Self::Provenance => "provenance",
            Self::Memberships => "contains_memory",
            Self::Hierarchy => "contains_cluster",
        }
    }

    pub(super) fn next(self) -> Option<Self> {
        Self::ALL
            .into_iter()
            .skip_while(|phase| *phase != self)
            .nth(1)
    }

    pub(super) fn is_edge(self) -> bool {
        matches!(self, Self::Memberships | Self::Hierarchy)
    }

    pub(super) fn legacy(self) -> &'static str {
        match self {
            Self::Reminders => "account = NONE",
            Self::Memberships | Self::Hierarchy => "in.account = NONE AND in.workspace = NONE",
            _ => "account = NONE AND workspace = NONE",
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    pub(super) sha256: String,
    pub(super) rows: u64,
    pub(super) held_reminders: u64,
}

impl Snapshot {
    pub(super) fn valid(&self) -> bool {
        is_sha256(&self.sha256) && self.held_reminders <= self.rows
    }
}

pub(super) fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) struct Owner {
    pub(super) account: RecordId,
    pub(super) workspace: RecordId,
}

pub(super) fn row_id(row: &Object) -> Result<&RecordId> {
    match row.get("id") {
        Some(Value::RecordId(id)) => Ok(id),
        _ => bail!("Ownership migration row has no typed record ID"),
    }
}

fn absent(row: &Object, field: &str) -> bool {
    matches!(row.get(field), None | Some(Value::None))
}

/// Only allowlisted fields are ever written. Existing owned rows are never rewritten, and
/// an incomplete owner pair is an error rather than evidence for either account's authority.
pub(super) fn adoption_patch(
    population: Population,
    row: &Object,
    owner: &Owner,
) -> Result<Option<Object>> {
    let account_missing = absent(row, "account");
    let workspace_missing = absent(row, "workspace");
    if population != Population::Reminders {
        ensure!(
            account_missing == workspace_missing,
            "Ownership snapshot has a partial owner pair"
        );
    }
    if !account_missing {
        if population == Population::Facts {
            ensure!(
                !absent(row, "scope"),
                "Owned fact in ownership snapshot has no scope"
            );
        }
        return Ok(None);
    }

    let mut patch = Object::new();
    patch.insert("account", owner.account.clone());
    if population != Population::Reminders {
        patch.insert("workspace", owner.workspace.clone());
    }
    if population == Population::Facts {
        ensure!(
            absent(row, "project")
                && (absent(row, "scope")
                    || row.get("scope") == Some(&Value::String("unassigned".into()))),
            "Unowned fact in backup snapshot carries an assigned scope"
        );
        patch.insert("scope", "unassigned");
    }
    if population != Population::Provenance && absent(row, "semantic_revision") {
        patch.insert("semantic_revision", 0i64);
    }
    if population == Population::Reminders {
        ensure!(
            absent(row, "project") && absent(row, "legacy_target_project"),
            "Unowned reminder in backup snapshot already carries a mapping"
        );
        let targeted = !absent(row, "target_project");
        if targeted {
            patch.insert("legacy_target_project", row["target_project"].clone());
        }
        patch.insert("migration_hold", targeted);
    }
    Ok(Some(patch))
}

pub(super) async fn page(
    tx: &Transaction<Any>,
    sql: &str,
    table: &str,
    after: Option<&RecordId>,
    limit: i64,
) -> Result<Vec<Object>> {
    let records = RecordId::new(
        table,
        RecordIdKeyRange {
            start: after.map_or(Bound::Unbounded, |id| Bound::Excluded(id.key.clone())),
            end: Bound::Unbounded,
        },
    );
    Ok(tx
        .query(sql)
        .bind(("records", records))
        .bind(("limit", limit))
        .await?
        .check()?
        .take(0)?)
}

fn hash_bytes(hash: &mut Sha256, data: &[u8]) {
    hash.update((data.len() as u64).to_le_bytes());
    hash.update(data);
}

struct Scan {
    hash: Sha256,
    rows: u64,
    held_reminders: u64,
}

impl Scan {
    fn new() -> Self {
        Self {
            hash: Sha256::new(),
            rows: 0,
            held_reminders: 0,
        }
    }

    fn record(&mut self, row: Object, table: &str) {
        self.rows += 1;
        self.held_reminders +=
            u64::from(table == "reminder" && row.get("migration_hold") == Some(&Value::Bool(true)));
        hash_bytes(&mut self.hash, row.to_sql().as_bytes());
    }

    fn finish(self) -> Snapshot {
        Snapshot {
            sha256: format!("{:x}", self.hash.finalize()),
            rows: self.rows,
            held_reminders: self.held_reminders,
        }
    }
}

pub(super) struct Attestation {
    pub(super) original: Snapshot,
    pub(super) expected: Option<Snapshot>,
    pub(super) changes: i64,
}

/// When reading the pristine backup, compute the exact allowed postimage in the same scan.
/// The comparison at completion hashes typed rows as actually stored, not another simulation.
pub(super) async fn capture(
    tx: &Transaction<Any>,
    nominated: Option<&Owner>,
) -> Result<Attestation> {
    let mut original = Scan::new();
    let mut expected = nominated.map(|_| Scan::new());
    let mut changes = 0;
    for (table, population) in Population::ALL
        .into_iter()
        .map(|p| (p.table(), Some(p)))
        .chain(PRESERVED.iter().map(|table| (*table, None)))
    {
        original.hash.update(b"table");
        hash_bytes(&mut original.hash, table.as_bytes());
        if let Some(scan) = &mut expected {
            scan.hash.update(b"table");
            hash_bytes(&mut scan.hash, table.as_bytes());
        }
        let sql = if nominated.is_some() {
            match population {
                Some(Population::Memberships) => {
                    "SELECT *, in.account AS migration_account, in.workspace AS migration_workspace, out.account AS migration_other_account, out.workspace AS migration_other_workspace, out.scope AS migration_other_scope FROM $records ORDER BY id LIMIT $limit"
                }
                Some(Population::Hierarchy) => {
                    "SELECT *, in.account AS migration_account, in.workspace AS migration_workspace, out.account AS migration_other_account, out.workspace AS migration_other_workspace FROM $records ORDER BY id LIMIT $limit"
                }
                _ => "SELECT * FROM $records ORDER BY id LIMIT $limit",
            }
        } else {
            "SELECT * FROM $records ORDER BY id LIMIT $limit"
        };
        let mut after = None;
        loop {
            let rows = page(tx, sql, table, after.as_ref(), ATTESTATION_PAGE)
                .await
                .with_context(|| format!("Read ownership snapshot table {table}"))?;
            if rows.is_empty() {
                break;
            }
            for mut row in rows {
                after = Some(row_id(&row)?.clone());
                if table == "system_config"
                    && row.get("key") == Some(&Value::String("schema_version".into()))
                {
                    continue;
                }
                let detach = match (nominated, population) {
                    (Some(_), Some(p)) if p.is_edge() => detach_edge(p, &mut row)?,
                    _ => false,
                };
                if let (Some(scan), Some(owner)) = (&mut expected, nominated) {
                    changes += i64::from(record_expected(
                        scan, table, population, &row, owner, detach,
                    )?);
                }
                original.record(row, table);
            }
        }
    }
    Ok(Attestation {
        original: original.finish(),
        expected: expected.map(Scan::finish),
        changes,
    })
}

fn detach_edge(population: Population, row: &mut Object) -> Result<bool> {
    let detach = absent(row, "migration_account") && absent(row, "migration_workspace");
    let other_legacy =
        absent(row, "migration_other_account") && absent(row, "migration_other_workspace");
    if population == Population::Hierarchy {
        ensure!(
            detach == other_legacy,
            "Backup snapshot has mixed legacy and owned cluster hierarchy"
        );
    }
    if !detach {
        ensure!(
            !absent(row, "migration_account")
                && !absent(row, "migration_workspace")
                && row.get("migration_account") == row.get("migration_other_account")
                && row.get("migration_workspace") == row.get("migration_other_workspace")
                && (population == Population::Hierarchy
                    || matches!(row.get("migration_other_scope"), Some(Value::String(scope)) if scope == "personal" || scope == "project")),
            "Backup snapshot has an owned cluster with an unassigned or foreign endpoint"
        );
    }
    for field in [
        "migration_account",
        "migration_workspace",
        "migration_other_account",
        "migration_other_workspace",
        "migration_other_scope",
    ] {
        row.remove(field);
    }
    Ok(detach)
}

fn record_expected(
    scan: &mut Scan,
    table: &str,
    population: Option<Population>,
    row: &Object,
    owner: &Owner,
    detach: bool,
) -> Result<bool> {
    if detach {
        return Ok(true);
    }
    let mut postimage = row.clone();
    let mut changed = false;
    if let Some(population) = population.filter(|p| !p.is_edge())
        && let Some(patch) = adoption_patch(population, row, owner)?
    {
        postimage.extend(patch);
        changed = true;
    }
    scan.record(postimage, table);
    Ok(changed)
}
