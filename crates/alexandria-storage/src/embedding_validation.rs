//! Original target data attestation, persisted before any rewrite. Hashes cover
//! typed rows across all tables (including extension tables), except vector fields,
//! owned lifecycle keys, and explicitly approved empty/unreferenced cluster rows.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use surrealdb::{
    Surreal,
    engine::any::Any,
    types::{RecordId, RecordIdKeyRange, SurrealValue, ToSql, Value},
};

use crate::{
    models::{Fact, embedding::ReembedJournal},
    repos::ClusterRepo,
    system_config,
};

pub const BASELINE_KEY: &str = "embedding_migration_baseline";
const PAGE_SIZE: i64 = 50;
const LIFECYCLE_KEYS: &[&str] = &[
    "embedding_model",
    "embedding_dimensions",
    "embedding_max_tokens",
    "embedding_space",
    "embedding_migration_journal",
    BASELINE_KEY,
];

/// Original counts and metadata digest belong to the same owned run as the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationBaseline {
    pub format_version: u32,
    pub run_id: String,
    pub source_manifest_sha256: String,
    pub fact_count: usize,
    /// Retained clusters; only the individually recorded empty IDs may disappear.
    pub cluster_count: usize,
    pub empty_cluster_ids: Vec<RecordId>,
    pub nonvector_sha256: String,
}

impl MigrationBaseline {
    fn validate(&self, journal: &ReembedJournal) -> Result<()> {
        ensure!(
            self.format_version == 1
                && self.run_id == journal.run_id
                && self.source_manifest_sha256 == journal.source_manifest_sha256,
            "Migration baseline ownership/version mismatch"
        );
        ensure!(
            self.nonvector_sha256.len() == 64
                && self
                    .nonvector_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "Invalid baseline metadata SHA256"
        );
        ensure!(
            self.empty_cluster_ids
                .windows(2)
                .all(|pair| pair[0] < pair[1])
                && self
                    .empty_cluster_ids
                    .iter()
                    .all(|id| id.table.as_str() == "cluster" && !id.key.is_range()),
            "Invalid baseline empty-cluster IDs"
        );
        Ok(())
    }
}

/// Never recapture on resume: absence, corruption or a different owner is an error.
pub async fn read_target_baseline(
    db: &Surreal<Any>,
    journal: &ReembedJournal,
) -> Result<MigrationBaseline> {
    journal.validate()?;
    let encoded = system_config::get_config(db, BASELINE_KEY)
        .await?
        .context("Missing original migration validation baseline")?;
    let baseline: MigrationBaseline =
        serde_json::from_str(&encoded).context("Malformed migration baseline")?;
    baseline.validate(journal)?;
    Ok(baseline)
}

pub(crate) async fn capture_baseline(
    db: &Surreal<Any>,
    journal: &ReembedJournal,
) -> Result<MigrationBaseline> {
    let clusters = ClusterRepo::new(db);
    let mut empty_cluster_ids = Vec::new();
    for cluster in clusters.list().await? {
        let id = cluster.id.context("Cluster has no record ID")?;
        if clusters.get_members(&id.to_sql()).await?.is_empty() {
            ensure!(
                !clusters.has_reembed_references(&id).await?,
                "Cluster {id:?} has zero visible members but retains references; resolve it explicitly before migration"
            );
            empty_cluster_ids.push(id);
        }
    }
    empty_cluster_ids.sort();
    let snapshot = metadata_snapshot(db, &empty_cluster_ids).await?;
    Ok(MigrationBaseline {
        format_version: 1,
        run_id: journal.run_id.clone(),
        source_manifest_sha256: journal.source_manifest_sha256.clone(),
        fact_count: snapshot.fact_count,
        cluster_count: snapshot.cluster_count,
        empty_cluster_ids,
        nonvector_sha256: snapshot.sha256,
    })
}

/// Validate counts and every non-vector row against the persisted original, not
/// against a transient snapshot of a partially migrated/resumed target.
pub async fn validate_target_metadata(db: &Surreal<Any>, journal: &ReembedJournal) -> Result<()> {
    let baseline = read_target_baseline(db, journal).await?;
    let actual = metadata_snapshot(db, &baseline.empty_cluster_ids).await?;
    ensure!(
        actual.fact_count == baseline.fact_count && actual.cluster_count == baseline.cluster_count,
        "Migration fact/cluster count differs from original baseline"
    );
    ensure!(
        actual.sha256 == baseline.nonvector_sha256,
        "Migration non-vector metadata differs from original baseline"
    );
    Ok(())
}

/// Validate an encoder result before writing it; never silently normalize bad output.
pub fn validate_unit_embedding(vector: &[f32], dimensions: usize) -> Result<()> {
    ensure!(
        vector.len() == dimensions,
        "Migration embedding dimension mismatch"
    );
    ensure!(
        vector.iter().all(|value| value.is_finite()),
        "Migration embedding is non-finite"
    );
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    ensure!(
        (norm - 1.0).abs() <= 1e-5,
        "Migration embedding is not a unit vector"
    );
    Ok(())
}

/// Plain member mean, matching current live-population semantics. Shape checks
/// precede accumulation, so a short member cannot be silently zip-truncated.
pub fn checked_target_centroid(members: &[Fact], dimensions: usize) -> Result<Vec<f32>> {
    ensure!(
        !members.is_empty(),
        "Cannot compute an empty target centroid"
    );
    let mut sums = vec![0.0f64; dimensions];
    for member in members {
        validate_unit_embedding(&member.embedding, dimensions)?;
        for (sum, value) in sums.iter_mut().zip(&member.embedding) {
            *sum += f64::from(*value);
        }
    }
    Ok(sums
        .into_iter()
        .map(|sum| (sum / members.len() as f64) as f32)
        .collect())
}

/// Prove that all facts (hidden ones included) were rewritten and the checkpoint
/// reaches the final key. This is also required before centroid work on resume.
pub async fn validate_target_facts(db: &Surreal<Any>, journal: &ReembedJournal) -> Result<()> {
    #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
    struct Row {
        id: RecordId,
        embedding: Vec<f32>,
    }
    let baseline = read_target_baseline(db, journal).await?;
    let mut last = None;
    let mut count = 0usize;
    loop {
        let records = RecordId::new(
            "fact",
            RecordIdKeyRange {
                start: last
                    .as_ref()
                    .map(|id: &RecordId| std::ops::Bound::Excluded(id.key.clone()))
                    .unwrap_or(std::ops::Bound::Unbounded),
                end: std::ops::Bound::Unbounded,
            },
        );
        let mut response = db
            .query("SELECT id, embedding FROM $records ORDER BY id ASC LIMIT $limit")
            .bind(("records", records))
            .bind(("limit", PAGE_SIZE))
            .await?;
        let rows: Vec<Row> = response.take(0)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            validate_unit_embedding(&row.embedding, journal.target_space.dimensions())?;
            count += 1;
            last = Some(row.id);
        }
    }
    ensure!(
        count == baseline.fact_count
            && count == journal.reembedded_count
            && last == journal.last_fact_id,
        "Migration fact count/cursor does not cover the complete original corpus"
    );
    Ok(())
}

/// Exact metadata survival, complete fact rewrite, retained cluster shapes/member
/// consistency, and disappearance of only the explicitly approved empty clusters.
pub async fn validate_target_data(db: &Surreal<Any>, journal: &ReembedJournal) -> Result<()> {
    validate_target_metadata(db, journal).await?;
    validate_target_facts(db, journal).await?;
    let baseline = read_target_baseline(db, journal).await?;
    let clusters = ClusterRepo::new(db);
    let rows = clusters.list().await?;
    ensure!(
        rows.len() == baseline.cluster_count,
        "Not every approved empty cluster was removed"
    );
    for cluster in rows {
        let id = cluster.id.context("Cluster has no ID")?;
        ensure!(
            baseline.empty_cluster_ids.binary_search(&id).is_err(),
            "Approved empty cluster still exists"
        );
        let members = clusters.get_members(&id.to_sql()).await?;
        let expected = checked_target_centroid(&members, journal.target_space.dimensions())?;
        ensure!(
            cluster.centroid.len() == expected.len()
                && cluster.centroid.iter().all(|value| value.is_finite())
                && cluster
                    .centroid
                    .iter()
                    .zip(&expected)
                    .all(|(actual, expected)| (actual - expected).abs() <= 1e-6),
            "Migration centroid disagrees with its live members"
        );
    }
    Ok(())
}

struct MetadataSnapshot {
    fact_count: usize,
    cluster_count: usize,
    sha256: String,
}

async fn table_names(db: &Surreal<Any>) -> Result<Vec<String>> {
    let mut response = db.query("INFO FOR DB STRUCTURE").await?;
    let info: Option<serde_json::Value> = response.take(0)?;
    let info = info.context("Missing database information")?;
    let mut names = info["tables"]
        .as_array()
        .context("Missing database tables")?
        .iter()
        .map(|table| {
            table["name"]
                .as_str()
                .context("Invalid database table name")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

async fn metadata_snapshot(db: &Surreal<Any>, empty: &[RecordId]) -> Result<MetadataSnapshot> {
    let mut hasher = Sha256::new();
    let mut snapshot = MetadataSnapshot {
        fact_count: 0,
        cluster_count: 0,
        sha256: String::new(),
    };
    for table in table_names(db).await? {
        hasher.update(b"table");
        hash_bytes(&mut hasher, table.as_bytes());
        let mut cursor = None;
        loop {
            let records = RecordId::new(
                table.clone(),
                RecordIdKeyRange {
                    start: cursor
                        .take()
                        .map(std::ops::Bound::Excluded)
                        .unwrap_or(std::ops::Bound::Unbounded),
                    end: std::ops::Bound::Unbounded,
                },
            );
            let sql = match table.as_str() {
                "fact" => "SELECT * OMIT embedding FROM $records ORDER BY id ASC LIMIT $limit",
                "cluster" => "SELECT * OMIT centroid FROM $records ORDER BY id ASC LIMIT $limit",
                _ => "SELECT * FROM $records ORDER BY id ASC LIMIT $limit",
            };
            let mut response = db
                .query(sql)
                .bind(("records", records))
                .bind(("limit", PAGE_SIZE))
                .await?;
            let rows: Vec<Value> = response.take(0)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let Value::Object(object) = &row else {
                    anyhow::bail!("Invalid metadata row");
                };
                let Some(Value::RecordId(id)) = object.get("id") else {
                    anyhow::bail!("Metadata row has no typed ID");
                };
                cursor = Some(id.key.clone());
                if table == "cluster" && empty.binary_search(id).is_ok() {
                    continue;
                }
                if table == "system_config"
                    && matches!(object.get("key"), Some(Value::String(key)) if LIFECYCLE_KEYS.contains(&key.as_str()))
                {
                    continue;
                }
                snapshot.fact_count += usize::from(table == "fact");
                snapshot.cluster_count += usize::from(table == "cluster");
                hasher.update(b"row");
                hash_bytes(&mut hasher, row.to_sql().as_bytes());
            }
        }
    }
    snapshot.sha256 = format!("{:x}", hasher.finalize());
    Ok(snapshot)
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}
