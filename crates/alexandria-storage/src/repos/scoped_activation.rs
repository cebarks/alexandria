//! Scoped graph reads and heat writes — the activation write flow.
//!
//! Spreading activation is a *write* path that walks a graph, so it needs two things a plain scoped
//! read does not:
//!
//! * **One source partition per focal record.** A retrieval may span Personal and a project, but each
//!   focal record's neighbours live in that record's own `(workspace, scope)`. Deriving the warm set
//!   from the *request's* union would let a project memory warm a Personal neighbour (and the other way
//!   round), which is cross-partition influence expressed as a heat number rather than as a leaked row.
//!   [`FocalPartition`] is therefore built from the focal fact's own columns, and nothing else.
//! * **The partition cannot choose whose rows warm.** A `FocalPartition` names a workspace, and the
//!   partition travels with each target, so a writer that bound its `WHERE workspace = …` from the
//!   target would be letting the caller pick the rows its authority applied to. `warm` therefore
//!   validates every target's workspace against the sealed [`OwnerContext`] before it opens a
//!   transaction, and the write itself binds `$workspace` from that context. The partition's scope and
//!   project still come from the target — they select which of the owner's own partitions may warm, and
//!   the row-level test inside the statement refuses a target that has moved out of them.
//! * **A live, ownership-checked write.** [`ScopedHeatWriter::warm`] re-derives each target's
//!   eligibility inside its own `UPDATE`. `HeatRepo::add_heat` is an `UPDATE heat_state … WHERE
//!   memory = …` that silently matches zero rows when the row is absent and its caller swallows the
//!   result, so an unguarded warm can reach any memory an edge points at — another account's, an
//!   Unassigned one's, a soft-deleted one's — and the write succeeds as far as the caller is concerned.
//!
//! Access recording is deliberately *not* here either. Retrieval records the accesses of the rows it
//! returned through `HeatRepo::record_accesses` — one bulk read plus one batched write, flat at ~1000
//! rows, which `AGENTS.md` records as a measured property — and `heat_state` carries no ownership column,
//! so the ids that batch takes come from the caller's own scoped candidate read. Re-expressing it as one
//! guarded write per returned row would put back exactly the per-target claim cost this pass removes.
//!
//! Both halves put their predicates inside the statement, before any truncation, because a filtered
//! page of an unfiltered traversal is not a filter: it is somebody else's neighbours, fewer of them.

use crate::access::{OwnerContext, ScopedContext};
use crate::error::StorageError;
use crate::models::{MemoryScope, Partition, ProjectId};
use crate::repos::scope::{Bind, GuardedWrite, guarded_writes, record_key};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// The live-memory predicate, spelled once.
///
/// `quarantined_at = NONE`, never `IS NOT NULL` or `!= NULL`: on this engine both of those are
/// *satisfied by* `NONE`, so they would filter nothing and quarantine would stop meaning anything the
/// moment somebody "simplified" the clause.
const LIVE_FACT: &str = "deleted = false AND quarantined_at = NONE";

/// The partition one focal record belongs to.
///
/// The producer in the request path is [`focal_partition_of`], which reads the partition out of the row
/// the store returned rather than accepting one the caller named — a row with no ownership columns is
/// migration-pending and seeds nothing, and guessing a partition from a partial row would guard the
/// wrong rows.
///
/// What the type does **not** claim is that a caller cannot build one: its fields are public and it is
/// re-exported, because the activation pass assembles its targets in the MCP crate. The trust boundary
/// is therefore in the writer and the reader, not in the type — and the two halves of a partition are
/// guarded by different mechanisms. [`ScopedHeatWriter::warm`] guards the **workspace** half by itself:
/// it refuses a target whose workspace is not the bound owner's before it opens a transaction, and binds
/// the statement's `$workspace` from the sealed context rather than from this field, so a fabricated
/// workspace is an error rather than a quiet no-op and can never aim a warm at another account's row.
/// The **scope/project** half is not `warm`'s check. It holds because
/// [`ScopedGraphReader::direct_neighbors_in_partition`] refuses any partition its sealed
/// `ScopedContext` does not cover, and because the statement `warm` runs tests each fact's own scope
/// against the *claimed* partition — a claim that matches the caller's own row is accepted, which is
/// exactly why a fabricated scope is stopped by the reader's coverage test and not by the writer.
///
/// A row that carries no workspace or no scope is migration-pending, and migration-pending data is not
/// eligible to warm anything: the constructor refuses rather than guessing, so an unowned fact cannot
/// become the seed of somebody's activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocalPartition {
    pub workspace: RecordId,
    pub scope: MemoryScope,
    pub project: Option<ProjectId>,
}

impl FocalPartition {
    fn from_parts(
        workspace: Option<RecordId>,
        scope: Option<String>,
        project: Option<RecordId>,
    ) -> Result<Self, StorageError> {
        // The project handle is validated here rather than carried raw: a `fact.project` column that
        // names something other than a `project` record is a corrupt row, and letting it through would
        // make the scope test compare a foreign key against itself.
        let project = project.map(ProjectId::try_from).transpose()?;
        let workspace = workspace.ok_or(StorageError::NotFound)?;
        let scope = match scope.as_deref() {
            Some("personal") => MemoryScope::Personal,
            Some("unassigned") => MemoryScope::Unassigned,
            // A row that claims the project scope without naming a project is corrupt, and reading "any
            // project partition" from it would turn one bad row into a cross-project warm.
            Some("project") => {
                let Some(project) = project.clone() else {
                    return Err(StorageError::Internal(anyhow::anyhow!(
                        "a project-scoped memory carries no project; refusing to widen the partition"
                    )));
                };
                MemoryScope::Project(project)
            }
            // Absent or unrecognised ownership is migration-pending or corrupt, and neither is
            // eligible to warm a neighbour.
            _ => return Err(StorageError::NotFound),
        };
        Ok(Self {
            workspace,
            scope,
            project,
        })
    }

    /// The project handle as a bind value.
    ///
    /// `ProjectId` is a validated *handle*, not a `SurrealValue` — that is the property that keeps it
    /// from being deserialised out of a stray table — so the raw `RecordId` inside it is what a query
    /// binds. `None` for a Personal or Unassigned partition, which the `project = $project` arm of the
    /// scope test then cannot match, so an absent project selects nothing instead of everything.
    pub(crate) fn project_bind(&self) -> Option<RecordId> {
        self.project
            .as_ref()
            .map(|project| project.as_record_id().clone())
    }

    /// The `Partition` this names, for callers that want the modelled value.
    ///
    /// The refusal stays a `StorageError` rather than an `expect`: the workspace came from a
    /// `fact.workspace` column, which v009 types as `option<record<workspace>>`, so a cross-table value
    /// reaching here means the row is not what the schema says it is — a finding, not a panic.
    pub fn partition(&self) -> Result<Partition, StorageError> {
        Ok(Partition {
            workspace: crate::models::WorkspaceId::try_from(self.workspace.clone())?,
            scope: self.scope.clone(),
        })
    }
}

/// A read of the graph inside one partition, bound to the context that resolved it.
pub struct ScopedGraphReader<'a> {
    db: &'a Surreal<Any>,
    scope: &'a ScopedContext,
}

impl<'a> ScopedGraphReader<'a> {
    pub fn new(db: &'a Surreal<Any>, scope: &'a ScopedContext) -> Self {
        Self { db, scope }
    }

    /// The direct neighbours of `focal` that live in `partition` and are live.
    ///
    /// Both directions, because an edge is a relationship and activation follows it either way, and
    /// each side's *target* row is filtered before it is returned: an edge to a foreign or dead fact
    /// yields no neighbour rather than a neighbour the writer then has to remember to skip.
    ///
    /// The `workspace`/`scope` comparison is made against the focal partition's own values, bound per
    /// call, so one reader can serve several focal records of one retrieval without ever letting
    /// partition A's neighbours answer for partition B's.
    pub async fn direct_neighbors_in_partition(
        &self,
        focal: &RecordId,
        partition: &FocalPartition,
    ) -> Result<Vec<GraphNeighbor>, StorageError> {
        // The focal record must itself be inside the caller's resolved population. A neighbour query
        // for a fact the context cannot read would otherwise leak a warm into a partition the caller
        // was never granted.
        if !self.covers(partition) {
            return Err(StorageError::NotFound);
        }

        let mut out = self.neighbors(focal, partition, "in", "out").await?;
        out.extend(self.neighbors(focal, partition, "out", "in").await?);
        Ok(out)
    }

    /// Whether the resolved population includes this partition.
    fn covers(&self, partition: &FocalPartition) -> bool {
        self.scope.partitions().iter().any(|candidate| {
            candidate.workspace.as_record_id() == &partition.workspace
                && scope_matches(&candidate.scope, partition)
        })
    }

    /// One direction of the traversal: edges whose *other end* is a live fact of this partition.
    async fn neighbors(
        &self,
        focal: &RecordId,
        partition: &FocalPartition,
        from_column: &str,
        to_column: &str,
    ) -> Result<Vec<GraphNeighbor>, StorageError> {
        // `from_column`/`to_column` are `in`/`out`, chosen by this file rather than by a caller, so
        // interpolating them cannot become an injection surface.
        debug_assert!(matches!(from_column, "in" | "out"));
        debug_assert!(matches!(to_column, "in" | "out"));
        // Step 1: the candidate ids from the edge table. Step 2: admit only those of them that are live
        // facts of *this* partition, filtered in SQL. Two statements either way, and the filter lives in
        // the second query rather than in Rust, so an over-wide candidate set cannot become an over-wide
        // warm.
        let candidates = {
            let sql = format!(
                "SELECT {to_column} AS id, edge_type, strength FROM memory_edge WHERE {from_column} \
                 = $focal;"
            );
            let mut response = self
                .db
                .query(sql)
                .bind(("focal", focal.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
            let rows: Vec<EdgeRow> = response.take(0).map_err(internal)?;
            rows
        };
        let ids: Vec<RecordId> = candidates
            .iter()
            .filter_map(|row| row.id.clone())
            .filter(|id| id != focal)
            .collect();
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut response = self
            .db
            .query(format!(
                "SELECT id, workspace, scope, project FROM fact WHERE id INSIDE $ids AND workspace \
                 = $workspace AND {LIVE_FACT} AND ( \
                   (scope = 'personal' AND $personal) \
                   OR (scope = 'unassigned' AND $unassigned) \
                   OR (scope = 'project' AND project = $project) \
                 );"
            ))
            .bind(("ids", ids.clone()))
            .bind(("workspace", partition.workspace.clone()))
            .bind(("personal", partition.scope == MemoryScope::Personal))
            .bind(("unassigned", partition.scope == MemoryScope::Unassigned))
            .bind(("project", partition.project_bind()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let eligible: Vec<EligibleRow> = response.take(0).map_err(internal)?;
        let allowed: std::collections::HashSet<String> = eligible
            .iter()
            .filter_map(|row| row.id.as_ref().map(|id| id.key_string()))
            .collect();

        Ok(candidates
            .into_iter()
            .filter(|row| {
                row.id
                    .as_ref()
                    .is_some_and(|id| allowed.contains(&id.key_string()))
            })
            .map(|row| GraphNeighbor {
                id: row.id.expect("filtered above"),
                edge_type: row.edge_type,
                strength: row.strength,
            })
            .collect())
    }
}

/// Whether a resolved partition's scope is the one the focal record lives in.
fn scope_matches(candidate: &MemoryScope, partition: &FocalPartition) -> bool {
    match (candidate, &partition.scope) {
        (MemoryScope::Personal, MemoryScope::Personal) => true,
        (MemoryScope::Unassigned, MemoryScope::Unassigned) => true,
        (MemoryScope::Project(a), MemoryScope::Project(b)) => a == b,
        _ => false,
    }
}

/// A neighbour the partition filter admitted.
#[derive(Debug, Clone)]
pub struct GraphNeighbor {
    pub id: RecordId,
    pub edge_type: String,
    pub strength: f64,
}

#[derive(SurrealValue)]
struct EdgeRow {
    id: Option<RecordId>,
    edge_type: String,
    strength: f64,
}

#[derive(SurrealValue)]
struct EligibleRow {
    id: Option<RecordId>,
}

trait KeyString {
    fn key_string(&self) -> String;
}

impl KeyString for RecordId {
    fn key_string(&self) -> String {
        crate::record_id_to_string(self)
    }
}

/// One neighbour to warm, with the partition it must be live inside to receive the warm.
///
/// The partition travels with the target rather than being a parameter of the pass because one pass can
/// span several focal records, and each focal record's neighbours live in *that record's* partition. A
/// pass-level partition would be the old defect — one request's union warming another partition's rows.
#[derive(Debug, Clone)]
pub struct WarmTarget {
    pub target: RecordId,
    pub partition: FocalPartition,
    pub delta: f64,
}

/// What an activation pass wrote, in the order the targets were given.
///
/// Per-target booleans rather than a count, because the property this crate was burned by is "a warm that
/// wrote nothing looked like a warm that happened": `HeatRepo::add_heat` is an `UPDATE heat_state … WHERE
/// memory = …` that matches zero rows silently when there is no row, and its caller wrapped the result in
/// `.ok()`. A refusal here is `false`, reported, and never a success.
#[derive(Debug, Clone)]
pub struct WarmPass {
    pub wrote: Vec<bool>,
}

impl WarmPass {
    pub fn warmed(&self) -> usize {
        self.wrote.iter().filter(|wrote| **wrote).count()
    }

    pub fn refused(&self) -> usize {
        self.wrote.iter().filter(|wrote| !**wrote).count()
    }
}

/// The activation writes, guarded once per pass.
pub struct ScopedHeatWriter<'a> {
    db: &'a Surreal<Any>,
    owner: &'a OwnerContext,
}

impl<'a> ScopedHeatWriter<'a> {
    /// Bound to the owner whose credential the pass guards on.
    ///
    /// The `OwnerContext` — not a bare account id — because a warm is a write, and every write in this
    /// checkpoint re-asserts its authority inside its own transaction.
    pub fn new(db: &'a Surreal<Any>, owner: &'a OwnerContext) -> Self {
        Self { db, owner }
    }

    /// Warms every target of one activation pass in **one** transaction, under **one** authority claim.
    ///
    /// Each target still carries its own partition, live and quarantine test *inside its own `UPDATE`*, so
    /// a neighbour that was deleted, quarantined or moved out of the focal record's partition between the
    /// read and the commit is refused by the engine rather than warmed. What is one-per-pass instead of
    /// one-per-target is the claim: [`super::scope::claim_authority`] writes the `account` row (and, for a
    /// hosted caller, the `credential` row) as the commit predicate, so a loop of per-target transactions
    /// meant one retrieval opened a dozen transactions all contending on the same account row — two
    /// concurrent retrieves for one account became guaranteed conflict opponents, retried to the bound and
    /// then dropped, and a dropped warm is invisible in the response. `AGENTS.md` treats this path's cost
    /// as a measured property; `one_activation_pass_claims_authority_once` is the measurement.
    ///
    /// The pass is therefore also atomic: a revocation landing between the claim and the commit costs the
    /// *whole* pass its commit, so a neighbour set is never half-warmed. An empty target list is the
    /// degenerate case of the same rule and is answered without opening anything: a claim is a write,
    /// and a pass that warms nothing has no business writing the account row — see
    /// `an_empty_warm_pass_claims_nothing_and_opens_no_transaction`.
    ///
    /// `Ok(WarmPass)` reports per-target refusals; `Err(Stale)` means the authority was gone before the
    /// first statement, and `Err(RetryableBookkeepingConflict)` that the race never cleared inside
    /// [`crate::repos::scope::GUARD_CONFLICT_ATTEMPTS`]. Nothing is written in either case.
    pub async fn warm(
        &self,
        targets: &[WarmTarget],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<WarmPass, StorageError> {
        // The seam the review named: `WarmTarget` carries a partition, and a partition carries a
        // workspace, so a writer that bound `$workspace` from the target was letting the caller choose
        // which rows its own authority could write. Two things close it, and both are here: the
        // workspace is validated against the sealed owner before anything opens, and the statement
        // below binds `$workspace` from that owner instead of from the target. A target naming another
        // workspace is a programming error and not a data condition, so it is an error and not a
        // per-target `false` — `false` is for a row the store itself declined to warm.
        let ours = self.owner.workspace().as_record_id();
        if let Some(foreign) = targets
            .iter()
            .find(|target| &target.partition.workspace != ours)
        {
            return Err(StorageError::InvalidInput(format!(
                "activation target {} names a workspace this writer does not hold",
                crate::record_id_to_string(&foreign.target)
            )));
        }

        let writes: Vec<GuardedWrite> = targets
            .iter()
            .map(|target| {
                GuardedWrite::new(
                    "UPDATE heat_state SET heat = heat + $delta, last_touched = time::now() \
                     WHERE memory = $target \
                       AND array::len((SELECT id FROM fact WHERE id = $target AND workspace = \
                                   $workspace AND deleted = false AND quarantined_at = NONE AND ( \
                                     (scope = 'personal' AND $personal) OR \
                                     (scope = 'unassigned' AND $unassigned) OR \
                                     (scope = 'project' AND project = $project) \
                                   ))) = 1 \
                     RETURN AFTER;",
                )
                // No `workspace` bind: `guarded_writes` binds the owner's own workspace, which is the
                // point. The partition below selects *which of the owner's rows* may warm; it cannot
                // select whose rows may warm.
                .value("delta", Bind::Float(target.delta))
                .id("target", target.target.clone())
                .value(
                    "personal",
                    Bind::Bool(target.partition.scope == MemoryScope::Personal),
                )
                .value(
                    "unassigned",
                    Bind::Bool(target.partition.scope == MemoryScope::Unassigned),
                )
                .value("project", Bind::OptRecord(target.partition.project_bind()))
            })
            .collect();

        let written = guarded_writes::<serde_json::Value>(self.db, self.owner, &writes, now)
            .await?
            .ok_or(StorageError::Stale)?;
        Ok(WarmPass {
            wrote: written.into_iter().map(|rows| !rows.is_empty()).collect(),
        })
    }
}

/// The partition of a fact row, read straight from the store rather than taken from the caller.
///
/// This is the producer the request path uses, and the reason it is the right one: a caller that named
/// a partition from memory could describe any partition it liked, and the live predicate would then be
/// guarding the wrong rows. Deriving it from the row means the partition a warm is scoped to is a fact
/// the store reported, not a claim the caller made.
///
/// It is the *producer* that makes request-path partitions trustworthy, not the type: `FocalPartition`
/// stays constructible outside this crate because the activation pass assembles its targets in the MCP
/// crate. What closes that seam is [`ScopedHeatWriter::warm`], which refuses a target whose workspace is
/// not the bound owner's before anything opens and binds the statement's `$workspace` from the sealed
/// context — see the module documentation.
///
/// `Ok(None)` means the fact does not exist. An existing row with no ownership columns is
/// migration-pending and errors as `NotFound` — indistinguishable from absent to a caller, which is the
/// point.
pub async fn focal_partition_of(
    db: &Surreal<Any>,
    id: &RecordId,
) -> Result<Option<FocalPartition>, StorageError> {
    #[derive(SurrealValue)]
    struct ScopeRow {
        workspace: Option<RecordId>,
        scope: Option<String>,
        project: Option<RecordId>,
    }
    let mut response = db
        .query(
            "SELECT workspace, scope, project FROM type::record('fact', $key) WHERE id = $id \
             LIMIT 1;",
        )
        .bind(("key", record_key(id)?))
        .bind(("id", id.clone()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    let rows: Vec<ScopeRow> = response.take(0).map_err(internal)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some(FocalPartition::from_parts(
        row.workspace,
        row.scope,
        row.project,
    )?))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}
