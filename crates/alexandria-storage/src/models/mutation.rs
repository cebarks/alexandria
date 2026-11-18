//! The shared mutation contract: typed semantic snapshots, the expectations a caller must state, and
//! the closed change-set payload `MutationRepo::apply` writes as one transaction.
//!
//! Why one module holds all of it rather than each writer describing its own change: a memory edit is
//! not one row. Content, vector, tags, visibility, scope, the `derived_from` lineage edge, the hidden
//! preimage row, the session link, the cluster membership and that cluster's derived centroid move
//! together, and before this contract the ordinary writers issued them as separate round trips — one of
//! which was `.ok()`-ed. A half-failed update could therefore leave a live row whose text described one
//! thing and whose vector another, or a hidden snapshot with no edge pointing back from the row that
//! replaced it, and nothing could answer "what did this memory say before?" without a global audit table.
//!
//! Three properties the shapes here are built to make impossible:
//!
//! * **Snapshots are typed variants, not bags of columns.** [`SemanticSnapshot`] has one arm per v009
//!   `semantic_snapshot.kind`, and each arm carries exactly what its variant can restore. There is no
//!   heat or access-clock field anywhere in this module because v009 gives the table no such column: a
//!   snapshot that could carry one is a way to rewind an access that happened after it.
//! * **The change set is closed.** Nothing here is `Serialize` or `Deserialize`, and no field holds SQL
//!   text or a patch document. A model can supply *values* to a tool parameter; only storage assembles
//!   them into a [`PreparedChangeSet`], and only `MutationRepo::apply` turns one into statements.
//! * **Staleness is checked against the row, not only a counter.** [`ExpectedState`] carries a
//!   [`Version`] *and* a semantic digest of the state the caller read. The counter catches converted
//!   writers; the digest catches one that is not converted yet and so never increments the counter it
//!   is asked to match.
//!
//! ```compile_fail
//! use alexandria_storage::models::mutation::PreparedChangeSet;
//!
//! fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
//! requires_deserialize::<PreparedChangeSet>();
//! ```

use crate::models::identity::{FactId, MemoryScope, ProjectId, Version, WorkspaceId};
use crate::models::{operation_kind, scope_kind};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use surrealdb::types::{RecordId, Value};

/// One `memory_edge`-shaped relation plus its exact endpoints.
///
/// Endpoints are part of the identity a rollback guards on, not metadata: the same pair of records
/// related again *is* the same relation, while the same edge id aimed at a different pair is a different
/// fact about the graph — which is why both are kept.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeSnapshot {
    pub edge: RecordId,
    pub kind: EdgeKind,
    pub from: RecordId,
    pub to: RecordId,
    pub strength: f64,
}

impl EdgeSnapshot {
    /// A `memory_edge` row of the given subtype.
    pub fn memory(edge: RecordId, kind: impl Into<String>, from: RecordId, to: RecordId) -> Self {
        Self {
            edge,
            kind: EdgeKind::Memory(kind.into()),
            from,
            to,
            strength: 1.0,
        }
    }
}

/// The closed relation vocabulary the journal knows how to restore.
///
/// A new relation table is a new migration *and* a new arm here — never a string a caller can put in a
/// JSON patch. `Memory` subtypes are names the graph already carries; the others are structural tables
/// `v001`/`v002`/`v005` declare, each with fixed endpoint types, and that declaration is what makes an
/// injected `RELATE` failure a real engine refusal rather than a mock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeKind {
    /// A `memory_edge` row of this subtype (`derived_from`, `extracted_from`, `related`, …).
    Memory(String),
    /// A `contains_memory` cluster membership.
    ClusterMembership,
    /// A `contains_session_memory` session link.
    SessionMembership,
    /// A `has_provenance` link to a provenance row.
    Provenance,
    /// An `extracted_from` link from a chunk to its source document.
    ExtractedFrom,
}

impl EdgeKind {
    /// The relation table this kind lives in, which the writer aims its `RELATE` at.
    pub fn table(&self) -> &'static str {
        match self {
            Self::Memory(_) => "memory_edge",
            Self::ClusterMembership => "contains_memory",
            Self::SessionMembership => "contains_session_memory",
            Self::Provenance => "has_provenance",
            Self::ExtractedFrom => "extracted_from",
        }
    }

    /// The effect label the journal records for a change to this kind of relation.
    ///
    /// Closed and stable because `semantic_operation.expected_effects` is what a reviewer compares an
    /// operation's intent against its outcome with, and a label that moved per writer would make the
    /// two unreadable side by side.
    pub fn effect(&self) -> String {
        match self {
            Self::Memory(kind) => format!("memory_edge:{kind}"),
            other => other.table().to_string(),
        }
    }

    /// The `edge_type` value a `memory_edge` row of this kind carries, or `None` for a structural table,
    /// which has no such column.
    pub fn edge_type(&self) -> Option<&str> {
        match self {
            Self::Memory(kind) => Some(kind.as_str()),
            _ => None,
        }
    }
}

/// The semantic state of one memory, as v009's `fact` snapshot variant stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct FactSnapshot {
    pub id: FactId,
    pub workspace: WorkspaceId,
    pub scope: MemoryScope,
    /// The row's `semantic_revision`. A row that predates v009 carries none, which the writer reads as
    /// [`Version::ZERO`] — v009's own rule that `NONE` means the value a default would have given — and
    /// every journaled write stamps a number.
    pub version: Version,
    pub content: String,
    pub tags: Vec<String>,
    pub confidence: f64,
    pub visibility: Visibility,
    pub embedding: Option<EmbeddingSnapshot>,
    /// The relations incident on this fact, with endpoints. A fact revision alone cannot certify an
    /// unchanged graph, so the guarded edges ride with the fact's own preimage.
    pub relation_versions: Vec<EdgeSnapshot>,
}

impl FactSnapshot {
    /// The digest of everything a *semantic* change moves.
    ///
    /// Length-prefixed framing, so two different states cannot render to the same bytes: `["ab","c"]`
    /// and `["a","bc"]` are different tag lists and must hash apart. Floats hash by bit pattern,
    /// because a digest that coalesced `0.5` and `0.5000000000000001` would make a real change look like
    /// a stale expectation.
    ///
    /// This is the compatibility seam's guard: a writer that edits a row without bumping
    /// `semantic_revision` still moves this digest, and the in-transaction comparison in
    /// `MutationRepo::apply` refuses over it.
    pub fn semantic_hash(&self) -> String {
        let mut hasher = Sha256::new();
        field(&mut hasher, b"fact");
        field(&mut hasher, &id_bytes(self.id.as_record_id()));
        field(&mut hasher, &id_bytes(self.workspace.as_record_id()));
        field(&mut hasher, scope_label(&self.scope).as_bytes());
        field(&mut hasher, &self.version.get().to_le_bytes());
        field(&mut hasher, self.content.as_bytes());
        for tag in &self.tags {
            field(&mut hasher, b"tag");
            field(&mut hasher, tag.as_bytes());
        }
        field(&mut hasher, b"confidence");
        field(&mut hasher, &self.confidence.to_bits().to_le_bytes());
        field(
            &mut hasher,
            &u64::from(self.visibility.deleted).to_le_bytes(),
        );
        field(
            &mut hasher,
            &self
                .visibility
                .quarantined_at
                .map(|at| at.timestamp().to_le_bytes().to_vec())
                .unwrap_or_else(|| b"none".to_vec()),
        );
        match &self.embedding {
            None => field(&mut hasher, b"embedding:none"),
            Some(embedding) => {
                field(&mut hasher, b"embedding");
                field(
                    &mut hasher,
                    embedding.model.as_deref().unwrap_or("").as_bytes(),
                );
                field(&mut hasher, &(embedding.dimensions as u64).to_le_bytes());
                for value in &embedding.vector {
                    field(&mut hasher, &value.to_bits().to_le_bytes());
                }
            }
        }
        field(&mut hasher, b"relations");
        for edge in self.sorted_relations() {
            field(&mut hasher, edge.kind.effect().as_bytes());
            field(&mut hasher, &id_bytes(&edge.from));
            field(&mut hasher, &id_bytes(&edge.to));
            field(&mut hasher, &edge.strength.to_bits().to_le_bytes());
        }
        hex(&hasher)
    }

    /// The incident relations in a deterministic order, so two reads of one graph hash the same.
    fn sorted_relations(&self) -> Vec<&EdgeSnapshot> {
        let mut sorted: Vec<&EdgeSnapshot> = self.relation_versions.iter().collect();
        sorted.sort_by(|a, b| {
            (a.kind.effect(), &a.from, &a.to).cmp(&(b.kind.effect(), &b.from, &b.to))
        });
        sorted
    }
}

/// Whether a memory is visible, and which of the two hides-it states it is in.
///
/// Both halves travel together because a move can quarantine and a delete can follow a quarantine; v009
/// records them as two columns and a snapshot that collapsed them would lose which one an owner did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Visibility {
    pub deleted: bool,
    pub quarantined_at: Option<DateTime<Utc>>,
}

impl Visibility {
    pub fn live() -> Self {
        Self::default()
    }

    pub fn quarantined(at: DateTime<Utc>) -> Self {
        Self {
            deleted: false,
            quarantined_at: Some(at),
        }
    }

    pub fn deleted() -> Self {
        Self {
            deleted: true,
            quarantined_at: None,
        }
    }
}

/// A stored vector plus the identity it was written under.
///
/// The identity travels with the vector because a postimage compared against a vector from a different
/// embedding space is meaningless, and because v009 stores exactly these columns.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingSnapshot {
    pub vector: Vec<f32>,
    pub model: Option<String>,
    pub dimensions: usize,
}

impl EmbeddingSnapshot {
    /// A vector as the caller's locked embedding space produced it.
    pub fn new(vector: Vec<f32>, model: Option<&str>) -> Self {
        let dimensions = vector.len();
        Self {
            vector,
            model: model.map(str::to_string),
            dimensions,
        }
    }
}

/// A cluster's derived state and all three of its guards.
///
/// The three revisions split because the plan splits them: membership and centroid recomputation move on
/// different writers, and keeping them apart is what makes an unrelated centroid move a *retryable*
/// bookkeeping conflict instead of a reviewed-membership `Stale` that would refuse a lawful operation.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSnapshot {
    pub id: RecordId,
    pub label: Option<String>,
    pub centroid: Vec<f32>,
    pub depth: i64,
    pub members: Vec<RecordId>,
    pub membership_revision: Version,
    pub member_embedding_revision: Version,
    pub centroid_revision: Version,
}

/// A reminder's *mapping*, never its delivery clock.
///
/// v009 gives this variant no `status`, `next_due_at`, `delivered_count` or `cancelled_at` column and
/// this struct matches it deliberately: a snapshot that could carry schedule state is a way to put an
/// already-delivered reminder back on the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderSnapshot {
    pub id: RecordId,
    pub account: RecordId,
    pub message: String,
    pub target_project: Option<String>,
    pub legacy_target_project: Option<String>,
    pub project: Option<ProjectId>,
    pub migration_hold: Option<bool>,
    pub semantic_revision: Version,
    /// One of [`crate::models::reminder::schedule_kind`]'s constants, or `None` when the row has none.
    pub schedule_kind: Option<String>,
}

/// The typed journal payload — one arm per v009 `semantic_snapshot.kind`.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticSnapshot {
    Fact(FactSnapshot),
    Edge(EdgeSnapshot),
    Cluster(ClusterSnapshot),
    Reminder(ReminderSnapshot),
}

impl SemanticSnapshot {
    /// The v009 discriminator for this variant.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Fact(_) => crate::models::snapshot_kind::FACT,
            Self::Edge(_) => crate::models::snapshot_kind::EDGE,
            Self::Cluster(_) => crate::models::snapshot_kind::CLUSTER,
            Self::Reminder(_) => crate::models::snapshot_kind::REMINDER,
        }
    }

    /// The record this snapshot describes. v009's `kind` assertion requires it to be of the variant's
    /// own table, so an arm and a subject that disagree is a refused write, not a silent one.
    pub fn subject(&self) -> RecordId {
        match self {
            Self::Fact(snapshot) => snapshot.id.as_record_id().clone(),
            Self::Edge(snapshot) => snapshot.edge.clone(),
            Self::Cluster(snapshot) => snapshot.id.clone(),
            Self::Reminder(snapshot) => snapshot.id.clone(),
        }
    }

    /// The digest the integrity columns `preimage_hash` / `postimage_hash` store.
    ///
    /// Length-prefixed framing, exactly like [`FactSnapshot::semantic_hash`], and over the same principle:
    /// a digest that collapsed two different states into one string would let a changed membership read as
    /// an unchanged one. Every variant's *complete* semantic payload is covered — an edge by its kind,
    /// endpoints and strength, a cluster by its centroid, depth, member set and all three guards, a
    /// reminder by its mapping — because a `kind + subject`-only digest describes which row was touched,
    /// not what state it was in, and presenting that as an exact-state guard would be a lie of omission.
    pub fn integrity_digest(&self) -> String {
        let mut hasher = Sha256::new();
        match self {
            Self::Fact(fact) => {
                field(&mut hasher, b"fact");
                hasher.update(fact.semantic_hash().as_bytes());
            }
            Self::Edge(edge) => {
                field(&mut hasher, b"edge");
                field(&mut hasher, &id_bytes(&edge.edge));
                field(&mut hasher, edge.kind.effect().as_bytes());
                field(&mut hasher, &id_bytes(&edge.from));
                field(&mut hasher, &id_bytes(&edge.to));
                field(&mut hasher, &edge.strength.to_bits().to_le_bytes());
            }
            Self::Cluster(cluster) => {
                field(&mut hasher, b"cluster");
                field(&mut hasher, &id_bytes(&cluster.id));
                match &cluster.label {
                    None => field(&mut hasher, b"label:none"),
                    Some(label) => {
                        field(&mut hasher, b"label");
                        field(&mut hasher, label.as_bytes());
                    }
                }
                field(&mut hasher, &(cluster.centroid.len() as u64).to_le_bytes());
                for value in &cluster.centroid {
                    field(&mut hasher, &value.to_bits().to_le_bytes());
                }
                field(&mut hasher, &cluster.depth.to_le_bytes());
                let mut members: Vec<Vec<u8>> = cluster.members.iter().map(id_bytes).collect();
                members.sort();
                field(&mut hasher, b"members");
                for member in &members {
                    field(&mut hasher, member);
                }
                field(&mut hasher, b"guards");
                for guard in [
                    cluster.membership_revision,
                    cluster.member_embedding_revision,
                    cluster.centroid_revision,
                ] {
                    field(&mut hasher, &guard.get().to_le_bytes());
                }
            }
            Self::Reminder(reminder) => {
                field(&mut hasher, b"reminder");
                field(&mut hasher, &id_bytes(&reminder.id));
                field(&mut hasher, &id_bytes(&reminder.account));
                field(&mut hasher, reminder.message.as_bytes());
                for text in [
                    &reminder.target_project,
                    &reminder.legacy_target_project,
                    &reminder.schedule_kind,
                ] {
                    match text {
                        None => field(&mut hasher, b"none"),
                        Some(value) => field(&mut hasher, value.as_bytes()),
                    }
                }
                match &reminder.project {
                    None => field(&mut hasher, b"project:none"),
                    Some(project) => field(&mut hasher, &id_bytes(project.as_record_id())),
                }
                match reminder.migration_hold {
                    None => field(&mut hasher, b"hold:none"),
                    Some(hold) => field(&mut hasher, &[u8::from(hold)]),
                }
                field(&mut hasher, &reminder.semantic_revision.get().to_le_bytes());
            }
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The workspace this snapshot belongs to: the owner's own for a fact, and supplied separately for
    /// the other variants, whose rows carry one only where v009 gives them a column.
    pub fn fact(&self) -> Option<&FactSnapshot> {
        match self {
            Self::Fact(snapshot) => Some(snapshot),
            _ => None,
        }
    }
}

/// What a relation must look like at commit time for the change to be lawful.
///
/// Both halves are one type because both are expectations, and a writer that could only state
/// "present" would silently accept a duplicate edge it was about to create for the second time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationExpectation {
    /// An edge of this kind between exactly these endpoints must exist. `edge` names the row the caller
    /// read: an id that no longer points at these endpoints is a conflict, not a match.
    Present {
        kind: EdgeKind,
        from: RecordId,
        to: RecordId,
        edge: Option<RecordId>,
    },
    /// No such edge may exist. Checked in the same transaction as the write that would create it.
    Absent {
        kind: EdgeKind,
        from: RecordId,
        to: RecordId,
    },
}

impl RelationExpectation {
    pub fn effect(&self) -> String {
        match self {
            Self::Present { kind, .. } | Self::Absent { kind, .. } => kind.effect(),
        }
    }

    pub fn table(&self) -> &'static str {
        match self {
            Self::Present { kind, .. } | Self::Absent { kind, .. } => kind.table(),
        }
    }
}

/// The caller's belief about one memory, which `apply` must find or refuse.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedState {
    pub fact: FactId,
    pub workspace: WorkspaceId,
    pub scope: MemoryScope,
    pub version: Version,
    /// [`FactSnapshot::semantic_hash`] of the state the caller read. A counter that a not-yet-converted
    /// writer never bumps cannot hide behind this field.
    pub semantic_hash: String,
    pub graph: Vec<RelationExpectation>,
}

impl ExpectedState {
    /// The expectation a caller states from a state it read through
    /// [`MutationRepo::expected_state`](crate::repos::MutationRepo::expected_state).
    pub fn of(snapshot: &FactSnapshot) -> Self {
        Self {
            fact: snapshot.id.clone(),
            workspace: snapshot.workspace.clone(),
            scope: snapshot.scope.clone(),
            version: snapshot.version,
            semantic_hash: snapshot.semantic_hash(),
            graph: snapshot
                .relation_versions
                .iter()
                .map(|edge| RelationExpectation::Present {
                    kind: edge.kind.clone(),
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    edge: Some(edge.edge.clone()),
                })
                .collect(),
        }
    }

    /// The one expectation a caller states from a state it read, with no graph guard.
    ///
    /// Reached only where the write cannot touch the graph at all, which is nothing in this slice: an
    /// edit bumps a revision, a quarantine hides neighbours, a move detaches memberships. Kept because
    /// `ExpectedState`'s own shape demands a way to state a graph-free read for the *negative* control
    /// in `tests/mutation_test.rs`.
    pub fn without_graph(snapshot: &FactSnapshot) -> Self {
        Self {
            graph: Vec::new(),
            ..Self::of(snapshot)
        }
    }
}

/// One eligible member of the exact population used to prepare a derived cluster write.
#[derive(Debug, Clone)]
pub struct ClusterPopulationMember {
    pub id: RecordId,
    pub embedding: Vec<f32>,
}

impl PartialEq for ClusterPopulationMember {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self
                .embedding
                .iter()
                .map(|value| value.to_bits())
                .eq(other.embedding.iter().map(|value| value.to_bits()))
    }
}

impl Eq for ClusterPopulationMember {}

/// The commit-time guards on one cluster's derived bookkeeping and its numerical population.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterWriteExpectation {
    pub cluster: RecordId,
    pub membership_revision: Version,
    pub member_embedding_revision: Version,
    pub centroid_revision: Version,
    /// Ordered live workspace members, captured once and reused by the preparer's arithmetic.
    /// Compatibility visibility/vector writers need not bump any of the three revision counters.
    pub population: Vec<ClusterPopulationMember>,
}

/// The far end of a relation, which may be a row this very transaction has not minted yet.
///
/// `NewFact` is the honest spelling of "the chunk I am creating in this same statement list": the
/// alternative — minting ids in Rust and interpolating them — would put a caller-chosen record key into
/// a `RELATE`, which is exactly the shape this payload exists to forbid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationTarget {
    /// A row that already exists, named by its validated handle.
    Existing(RecordId),
    /// A fact this very change creates, by its index into [`PreparedChangeSet::facts`].
    NewFact(usize),
    /// A source document this very change stores, by its index into
    /// [`PreparedChangeSet::raw_documents`].
    NewRaw(usize),
    /// A provenance row this very change writes, by its index into
    /// [`PreparedChangeSet::provenance_rows`].
    NewProvenance(usize),
}

/// A fact row this change writes.
#[derive(Debug, Clone, PartialEq)]
pub enum FactChange {
    /// Write a new row into this partition. Used by an import chunk and by the hidden lineage snapshot a
    /// content update leaves behind. The id is minted by the engine and reported in
    /// [`AppliedChange::created`].
    Create {
        scope: MemoryScope,
        content: String,
        tags: Vec<String>,
        confidence: f64,
        embedding: EmbeddingSnapshot,
        visibility: Visibility,
        metadata: Option<serde_json::Value>,
    },
    /// Move an existing row's semantic state. Every `Some` half is written and every `None` half is left
    /// exactly as the row has it; `expected` is the revision the caller read, which the statement's own
    /// `WHERE` compares.
    Update {
        fact: FactId,
        expected: Version,
        content: Option<String>,
        tags: Option<Vec<String>>,
        confidence: Option<f64>,
        embedding: Option<EmbeddingSnapshot>,
        visibility: Option<Visibility>,
        scope: Option<MemoryScope>,
    },
}

impl FactChange {
    /// The row this change aims at, once it exists.
    pub fn target(&self) -> Option<&FactId> {
        match self {
            Self::Create { .. } => None,
            Self::Update { fact, .. } => Some(fact),
        }
    }

    /// The scope this change leaves the row in, where it states one.
    pub fn scope(&self) -> Option<MemoryScope> {
        match self {
            Self::Create { scope, .. } => Some(scope.clone()),
            Self::Update { scope, .. } => scope.clone(),
        }
    }
}

/// A relation this change creates or removes.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationChange {
    pub kind: EdgeKind,
    pub from: RelationTarget,
    pub to: RelationTarget,
    pub strength: f64,
}

impl RelationChange {
    pub fn new(kind: EdgeKind, from: RelationTarget, to: RelationTarget) -> Self {
        Self {
            kind,
            from,
            to,
            strength: 1.0,
        }
    }

    /// A `memory_edge` of the given subtype between exactly these endpoints.
    pub fn memory(
        edge_type: impl Into<String>,
        from: RelationTarget,
        to: RelationTarget,
        strength: f64,
    ) -> Self {
        Self {
            kind: EdgeKind::Memory(edge_type.into()),
            from,
            to,
            strength,
        }
    }

    /// A chunk's link to the source document this same change stores.
    pub fn extracted_from(chunk: RelationTarget, raw: RelationTarget) -> Self {
        Self {
            kind: EdgeKind::ExtractedFrom,
            from: chunk,
            to: raw,
            strength: 1.0,
        }
    }

    /// A memory's link to the provenance row this change writes.
    pub fn provenance(fact: RelationTarget, provenance: RelationTarget) -> Self {
        Self {
            kind: EdgeKind::Provenance,
            from: fact,
            to: provenance,
            strength: 1.0,
        }
    }

    /// A cluster membership effect.
    pub fn membership(cluster: RecordId, member: RelationTarget) -> Self {
        Self {
            kind: EdgeKind::ClusterMembership,
            from: RelationTarget::Existing(cluster),
            to: member,
            strength: 1.0,
        }
    }
}

/// A session link this change writes: the caller's own session row to a fact this change touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLink {
    pub session: RecordId,
    pub to: RelationTarget,
}

/// A cluster effect this change approves, plus the CAS its derived bookkeeping must satisfy.
///
/// Two arms because the two jobs differ: a created cluster has no earlier version to compare against,
/// so a CAS on it would be a guard that can never be wrong, while an existing cluster's derived state is
/// exactly what a concurrent writer may have recomputed.
#[derive(Debug, Clone, PartialEq)]
pub enum ClusterChange {
    /// Defend an existing cluster's population and three revisions without changing it or imaging it.
    /// Used when a pinned contribution removal explicitly approved no centroid write.
    Guard {
        expectation: ClusterWriteExpectation,
    },
    /// Mint a cluster row with this centroid and attach one of this change's facts to it.
    Create {
        centroid: Vec<f32>,
        label: Option<String>,
        member: RelationTarget,
    },
    /// Detach a member and write the centre its survivors imply, under the same three guards.
    ///
    /// This is what makes a scope move honest about the cluster it leaves. A plain `Effect` with
    /// `centroid: None` drops the membership and leaves the stored mean still averaging a vector that is
    /// no longer a member — a derived number that disagrees with the population it claims to describe,
    /// read by the debug cluster view and by the next similarity ranking alike, and exactly the deferral
    /// the owner refused when they ruled that a move may not silently leave a stale centroid to Task 12.
    ///
    /// `surviving_centroid` is **prepared, not derived here**: the mean of a member population is the
    /// engine's arithmetic, and `alexandria-storage` cannot depend on the engine, so the caller that read
    /// the survivors computes it (see `alexandria_mcp::mutation::ApprovedCluster::Depart`). `None` states
    /// that no member survived — the cluster is now empty, it has no centre to write, and its derived
    /// state is exactly *nothing*, which is a claim about the population rather than a stale number.
    Detach {
        expectation: ClusterWriteExpectation,
        member: RecordId,
        surviving_centroid: Option<Vec<f32>>,
    },
    /// Apply an approved membership effect to an existing cluster and, optionally, write the centroid
    /// that effect implies.
    ///
    /// A join is guarded by the transaction against the partition **the arriving row itself names**, which
    /// is read inside the transaction rather than declared here. The reason is that the candidate
    /// population a destination was ranked against is selected *before* the transaction opens: a writer
    /// that attached a member of another partition in that window would otherwise be joined to by this one,
    /// and the cluster would end up straddling two scopes with a single `contains_memory` edge on each side
    /// — the exact state [`ScopedClusterRepo::partition_candidates`] exists to refuse, reached by racing
    /// past it rather than by selecting it. Deriving the claim from the row that is being joined, rather
    /// than from a field a caller may leave empty, is what makes the guard impossible to forget: every
    /// effect that adds a member is checked, whoever wrote the effect, and no construction site can opt
    /// out by passing `None`. See [`MutationRepo::apply_cluster`].
    Effect {
        expectation: ClusterWriteExpectation,
        add_member: Option<RelationTarget>,
        remove_member: Option<RecordId>,
        /// The centroid to write *after* the membership effect, recomputed by the caller from the member
        /// embeddings it read. `None` leaves the centroid alone.
        centroid: Option<Vec<f32>>,
    },
    /// Atomically replace membership across one or more clusters under the full journal kernel.
    ///
    /// Every source is guarded against its live embedded population. Destinations name the complete
    /// final membership: `Some(cluster)` retains and rewrites that cluster, while `None` mints a new
    /// cluster. Source clusters omitted from destinations are retired. Storage verifies in-transaction
    /// that destinations contain exactly the existing source membership, so no retained (including
    /// hidden) member can be lost or imported from outside the restructuring.
    Restructure {
        partition: MemoryScope,
        expectations: Vec<ClusterWriteExpectation>,
        destinations: Vec<ClusterDestination>,
    },
}

/// One final cluster state for a journaled restructuring.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterDestination {
    pub cluster: Option<RecordId>,
    /// A storage-minted typed id for a new destination, needed so the linked summary can name it.
    pub new_id: Option<RecordId>,
    pub label: Option<String>,
    pub centroid: Vec<f32>,
    pub members: Vec<RecordId>,
}

impl ClusterChange {
    /// Join the caller's existing cluster, leaving its centroid as it stands.
    pub fn join(expectation: ClusterWriteExpectation) -> Self {
        Self::Effect {
            expectation,
            add_member: None,
            remove_member: None,
            centroid: None,
        }
    }

    /// The cluster this effect names, when it names one.
    pub fn cluster(&self) -> Option<&RecordId> {
        match self {
            Self::Create { .. } => None,
            Self::Guard { expectation }
            | Self::Detach { expectation, .. }
            | Self::Effect { expectation, .. } => Some(&expectation.cluster),
            Self::Restructure { expectations, .. } => {
                expectations.first().map(|item| &item.cluster)
            }
        }
    }

    /// The membership edge this effect removes, when it removes one.
    pub fn detached_member(&self) -> Option<&RecordId> {
        match self {
            Self::Detach { member, .. } => Some(member),
            Self::Effect {
                remove_member: Some(member),
                ..
            } => Some(member),
            _ => None,
        }
    }
}

/// Where the memory a scope move filed lands *inside its new partition's cluster population*.
///
/// Ranking a memory against candidate clusters is the engine's job and the server's threshold, so the
/// decision arrives prepared — this type is the storage-owned record of it, never a second ranker. The
/// three arms are the three states the ranking can honestly produce, and `Unclustered` is one of them:
/// a memory whose vector reaches no existing centre and whose partition mints no cluster for it is a
/// memory that is simply not yet grouped, which is a different claim from "nothing was decided".
#[derive(Debug, Clone, PartialEq)]
pub enum DestinationMembership {
    /// Join this cluster of the destination partition, writing the centroid its live members together
    /// with the moved memory's own vector imply.
    Join {
        expectation: ClusterWriteExpectation,
        centroid: Vec<f32>,
    },
    /// Mint the destination partition's cluster for it, centred on the moved memory's own vector.
    Create { centroid: Vec<f32> },
    /// The caller prepared no destination membership: the row lands grouped by nothing, for now.
    Unclustered,
}

/// The cluster half of one scope move, as the caller that ranked it prepared it.
///
/// Two halves because a move crosses a partition boundary and both sides' derived state is then wrong:
/// the cluster the memory leaves still averages a vector that no longer belongs to it, and the partition
/// it enters has a cluster population the memory is not yet part of. Both arrive *prepared* — the mean of
/// a population and the choice of nearest centre are the engine's arithmetic, and storage may not reach it
/// — and both are then applied under storage's own compare-and-set, so a centroid computed from a member
/// set that has since moved loses the transaction instead of overwriting the winner.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveReconciliation {
    /// The cluster the memory is leaving, with the centroid its survivors imply, when it was in one.
    pub leaving: Option<LeavingCluster>,
    /// Where the memory lands inside the destination partition's cluster population.
    pub arriving: DestinationMembership,
}

impl MoveReconciliation {
    /// The reconciliation carrying **no caller opinion on either side**.
    ///
    /// This is not "skip the leaving half": `leaving: None` tells `MutationRepo::plan_move` to read the
    /// membership itself and prepare the survivors' centre from that expectation's population before
    /// applying the move. The transaction validates the same population before writing; it does not run
    /// the arithmetic. `arriving: Unclustered` is the caller
    /// stating that it did not rank a destination cluster — the honest answer from a writer with no
    /// threshold and no engine, and the reason a destination opinion arrives as a prepared
    /// [`DestinationMembership`] rather than being guessed at here.
    pub fn ungrouped() -> Self {
        Self {
            leaving: None,
            arriving: DestinationMembership::Unclustered,
        }
    }
}

impl Default for MoveReconciliation {
    /// The reconciliation that claims nothing on either side: `leaving: None` still makes the writer read
    /// the membership and re-derive the survivors' centre, and `arriving: Unclustered` states that the
    /// destination partition's clusters do not know about this memory yet.
    fn default() -> Self {
        Self {
            leaving: None,
            arriving: DestinationMembership::Unclustered,
        }
    }
}

/// The cluster a moved memory leaves, and the centre its survivors imply.
#[derive(Debug, Clone, PartialEq)]
pub struct LeavingCluster {
    pub expectation: ClusterWriteExpectation,
    /// `None` when no member survives the detach: the cluster has no population to centre on.
    pub surviving_centroid: Option<Vec<f32>>,
}

/// A provenance row this change writes: the description, linked to the memory it describes.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvenanceRow {
    /// The slot this create occupies among the change's provenance rows, which is what
    /// [`RelationTarget::NewProvenance`] names.
    pub slot: usize,
    pub kind: String,
    pub timestamp: DateTime<Utc>,
    /// The memory this description is *of*. It may be a row the same change mints, which is what makes
    /// "newly stored provenance is linked/owned" one atomic fact rather than a later best-effort edge.
    pub fact: RelationTarget,
    pub metadata: Option<serde_json::Value>,
}

/// A source document an import stores, so a chunk's `extracted_from` edge has a row to point at.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDocument {
    /// The slot this create occupies among the change's raw documents, which is what
    /// [`RelationTarget::NewRaw`] names.
    pub slot: usize,
    pub content: String,
}

/// The exact pending human placement opinion decided by one scope-only mutation.
/// Identity, authority and timestamps are supplied by the transaction, not by this clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationApproval {
    pub proposal: RecordId,
    pub proposal_revision: Version,
    pub fact: FactId,
    pub source_revision: Version,
    pub destination: MemoryScope,
}

/// The whole change, as one atomic unit.
///
/// Built by storage's own writers, never parsed: `PreparedChangeSet` has no `Deserialize` and no field
/// that could hold a statement.
///
/// **It carries no before- or after-state, and never will.** Every image the journal writes is what
/// *this transaction* read or wrote, captured inside `MutationRepo::run`, and nothing a caller declared
/// before opening it. That is the difference between a journal entry and a receipt: a caller-stated
/// preimage describes the state the caller *believed* it was changing, and the belief is exactly what a
/// race invalidates. An earlier `before`/`snapshot` builder pair stood unused for a whole review round
/// while the cluster images were still journaling as fact preimages only, which is the failure the
/// review named; the seam is now singular and the invariant is stated at [`MutationRepo::run`].
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedChangeSet {
    /// The v009 `semantic_operation.kind` this change writes.
    pub kind: &'static str,
    /// The effects the caller expects to apply. v009 refuses an empty effect set, and `apply` does not
    /// pre-validate it: an operation that describes nothing is an engine refusal, and that refusal is
    /// what `journal_snapshot_or_summary_failure_rolls_back_everything` observes rolling the rest back.
    pub effects: Vec<String>,
    /// The writes, in the order the statements run. A create's row is addressable by later statements by
    /// its index here, which is what `RelationTarget::NewFact` names.
    pub facts: Vec<FactChange>,
    pub edge_additions: Vec<RelationChange>,
    pub edge_removals: Vec<RelationChange>,
    pub session_links: Vec<SessionLink>,
    pub cluster_changes: Vec<ClusterChange>,
    /// The embedding identity every vector in this change was produced under.
    pub embedding_space: Option<String>,
    /// A background writer's linked summary, written in the same transaction as the change it describes.
    /// `None` for every ordinary owner request.
    pub maintenance_summary: Option<MaintenanceSummary>,
    /// Source documents this change stores, before the chunks that reference them.
    pub raw_documents: Vec<RawDocument>,
    /// Provenance descriptions this change writes and links, for "newly stored provenance is
    /// linked/owned".
    pub provenance_rows: Vec<ProvenanceRow>,
    /// The heat this change seeds for every fact it **creates**, or `None` to seed none.
    ///
    /// Not a journal entry and not an access clock update: `heat_state` is created with the row so a
    /// stored memory is never a memory with no heat row, which is the shape the "did not write heat"
    /// assertions in this repo have to avoid asserting vacuously. An existing row's heat is never
    /// touched by a mutation.
    pub heat_seed: Option<f64>,
    pub classification_approval: Option<ClassificationApproval>,
}

impl PreparedChangeSet {
    /// An empty change of one closed kind. Every builder starts here, so `kind`, the effect set and the
    /// embedding identity are stated once and the rest is appended by the writer that plans the change.
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            effects: Vec::new(),
            facts: Vec::new(),
            edge_additions: Vec::new(),
            edge_removals: Vec::new(),
            session_links: Vec::new(),
            cluster_changes: Vec::new(),
            embedding_space: None,
            maintenance_summary: None,
            raw_documents: Vec::new(),
            provenance_rows: Vec::new(),
            heat_seed: None,
            classification_approval: None,
        }
    }

    pub fn approve_classification(mut self, approval: ClassificationApproval) -> Self {
        self.classification_approval = Some(approval);
        self
    }

    /// Record one effect the change means to apply, as the closed label [`EdgeKind::effect`] or a
    /// semantic verb from [`effect`] names it.
    pub fn effect(mut self, label: impl Into<String>) -> Self {
        self.effects.push(label.into());
        self
    }

    pub fn fact(mut self, change: FactChange) -> Self {
        self.facts.push(change);
        self
    }

    pub fn relate(mut self, change: RelationChange) -> Self {
        self.edge_additions.push(change);
        self
    }

    pub fn unrelate(mut self, change: RelationChange) -> Self {
        self.edge_removals.push(change);
        self
    }

    pub fn link_session(mut self, session: RecordId, to: RelationTarget) -> Self {
        self.session_links.push(SessionLink { session, to });
        self
    }

    pub fn cluster(mut self, change: ClusterChange) -> Self {
        self.cluster_changes.push(change);
        self
    }

    pub fn raw(mut self, content: String) -> Self {
        self.raw_documents.push(RawDocument {
            slot: self.raw_documents.len(),
            content,
        });
        self
    }

    /// Record where a memory came from, linked to the memory it describes.
    ///
    /// The default target is slot zero of this change's own creates, because the only caller that has no
    /// explicit fact is a store that mints exactly one row — and `store_body` is the path this exists for.
    pub fn provenance(mut self, kind: impl Into<String>, timestamp: DateTime<Utc>) -> Self {
        self.provenance_rows.push(ProvenanceRow {
            slot: self.provenance_rows.len(),
            kind: kind.into(),
            timestamp,
            fact: RelationTarget::NewFact(0),
            metadata: None,
        });
        self
    }

    /// Record where a memory came from, aimed at a fact this change does not mint.
    pub fn provenance_of(
        mut self,
        kind: impl Into<String>,
        timestamp: DateTime<Utc>,
        fact: RelationTarget,
    ) -> Self {
        self.provenance_rows.push(ProvenanceRow {
            slot: self.provenance_rows.len(),
            kind: kind.into(),
            timestamp,
            fact,
            metadata: None,
        });
        self
    }

    /// Seed one initial heat value for every fact this change creates.
    pub fn heat(mut self, initial: f64) -> Self {
        self.heat_seed = Some(initial);
        self
    }

    pub fn in_space(mut self, model: Option<&str>) -> Self {
        self.embedding_space = model.map(str::to_string);
        self
    }

    pub fn summary(mut self, summary: MaintenanceSummary) -> Self {
        self.maintenance_summary = Some(summary);
        self
    }

    /// The fingerprint of *this request*, which is what makes an operation key mean the same thing twice.
    ///
    /// Hashed over the closed effect set, the kind, and every semantic value the change would write, in
    /// statement order. Deliberately not over record ids alone: a caller that retries the same edit after
    /// the row moved must replay, and one that reuses its key for a *different* edit must conflict —
    /// ids would make those two look identical.
    pub fn request_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        field(&mut hasher, b"semantic-request");
        field(&mut hasher, self.kind.as_bytes());
        for effect in &self.effects {
            field(&mut hasher, effect.as_bytes());
        }
        field(
            &mut hasher,
            self.embedding_space.as_deref().unwrap_or("").as_bytes(),
        );
        for fact in &self.facts {
            field(&mut hasher, b"fact");
            match fact {
                FactChange::Create {
                    scope,
                    content,
                    tags,
                    confidence,
                    embedding,
                    visibility,
                    metadata,
                } => {
                    field(&mut hasher, b"create");
                    field(&mut hasher, scope_label(scope).as_bytes());
                    field(&mut hasher, content.as_bytes());
                    for tag in tags {
                        field(&mut hasher, tag.as_bytes());
                    }
                    field(&mut hasher, &confidence.to_bits().to_le_bytes());
                    for value in &embedding.vector {
                        field(&mut hasher, &value.to_bits().to_le_bytes());
                    }
                    field(&mut hasher, &[u8::from(visibility.deleted)]);
                    field(&mut hasher, b"metadata");
                    field(
                        &mut hasher,
                        metadata
                            .as_ref()
                            .map(serde_json::Value::to_string)
                            .unwrap_or_default()
                            .as_bytes(),
                    );
                }
                FactChange::Update {
                    fact,
                    expected,
                    content,
                    tags,
                    confidence,
                    embedding,
                    visibility,
                    scope,
                } => {
                    field(&mut hasher, b"update");
                    field(&mut hasher, &id_bytes(fact.as_record_id()));
                    field(&mut hasher, &expected.get().to_le_bytes());
                    field(
                        &mut hasher,
                        content.as_deref().map(str::as_bytes).unwrap_or(b"\0"),
                    );
                    field(&mut hasher, &[u8::from(tags.is_some())]);
                    for tag in tags.iter().flatten() {
                        field(&mut hasher, tag.as_bytes());
                    }
                    field(
                        &mut hasher,
                        &confidence.unwrap_or(f64::NAN).to_bits().to_le_bytes(),
                    );
                    field(
                        &mut hasher,
                        embedding
                            .iter()
                            .flat_map(|e| e.vector.iter().map(|v| v.to_bits().to_le_bytes()))
                            .collect::<Vec<_>>()
                            .concat()
                            .as_slice(),
                    );
                    field(&mut hasher, &[u8::from(visibility.is_some())]);
                    field(
                        &mut hasher,
                        visibility
                            .map(|v| u64::from(v.deleted).to_le_bytes().to_vec())
                            .unwrap_or_default()
                            .as_slice(),
                    );
                    field(
                        &mut hasher,
                        scope
                            .as_ref()
                            .map(scope_label)
                            .unwrap_or_default()
                            .as_bytes(),
                    );
                }
            }
        }
        for edge in self.edge_additions.iter().chain(&self.edge_removals) {
            field(&mut hasher, edge.kind.effect().as_bytes());
            field(&mut hasher, target_bytes(&edge.from).as_bytes());
            field(&mut hasher, target_bytes(&edge.to).as_bytes());
            field(&mut hasher, &edge.strength.to_bits().to_le_bytes());
        }
        for link in &self.session_links {
            field(&mut hasher, b"session");
            field(&mut hasher, &id_bytes(&link.session));
            field(&mut hasher, target_bytes(&link.to).as_bytes());
        }
        // Cluster effects contribute their *requested* shape only, never their derived half.
        //
        // The guard revisions a `ClusterWriteExpectation` carries and the centroid a join recomputes are
        // both re-derived at every preparation inside the same live request (that is what the
        // three-preparation bound *is*), so folding them into the request fingerprint would mean the
        // second attempt of one request carries a different fingerprint from its first — and a replay of
        // an already-committed request, whose approved population has since moved under it, would answer
        // "different effects under the same key" when the honest answer is "the same effects, and the
        // bookkeeping moved". The replay contract Task 10 states — same key plus same *requested* effects
        // returns the recorded result; same key under different requested effects conflicts — therefore
        // holds only if the fingerprint covers intent: kind, effect labels, embedding space, the fact
        // values, the graph edges, and which cluster each membership effect names with which member.
        // The derived halves, including the captured live population, stay in the transactional guards.
        // They may refresh without changing request identity; neither stale counters nor a stale numerical
        // population may be published by a new commit.
        for change in &self.cluster_changes {
            field(&mut hasher, b"cluster");
            match change {
                ClusterChange::Guard { expectation } => {
                    field(&mut hasher, b"guard");
                    field(&mut hasher, &typed_id_bytes(&expectation.cluster));
                }
                ClusterChange::Create { label, member, .. } => {
                    field(&mut hasher, b"create");
                    field(&mut hasher, label.as_deref().unwrap_or("").as_bytes());
                    field(&mut hasher, target_bytes(member).as_bytes());
                }
                // The *requested* half of a cluster effect: which cluster, which member in, which member
                // out. The three guard revisions and the recomputed centroid are deliberately absent —
                // see the note above, where the reason is stated as the replay contract rather than here
                // as a comment on a match arm.
                ClusterChange::Effect {
                    expectation,
                    add_member,
                    remove_member,
                    ..
                } => {
                    field(&mut hasher, b"effect");
                    field(&mut hasher, &id_bytes(&expectation.cluster));
                    field(
                        &mut hasher,
                        add_member
                            .as_ref()
                            .map(target_bytes)
                            .unwrap_or_default()
                            .as_bytes(),
                    );
                    field(
                        &mut hasher,
                        &remove_member.as_ref().map(id_bytes).unwrap_or_default(),
                    );
                }
                // The derived centroid is deliberately not bound here — see the note above the loop.
                ClusterChange::Detach {
                    expectation,
                    member: detach_member,
                    ..
                } => {
                    let member = detach_member;
                    field(&mut hasher, b"detach");
                    field(&mut hasher, &id_bytes(&expectation.cluster));
                    field(&mut hasher, b"");
                    field(&mut hasher, &id_bytes(member));
                }
                ClusterChange::Restructure {
                    partition,
                    expectations,
                    destinations,
                } => {
                    field(&mut hasher, format!("{partition:?}").as_bytes());
                    field(&mut hasher, b"restructure");
                    let mut sources: Vec<_> =
                        expectations.iter().map(|item| &item.cluster).collect();
                    sources.sort();
                    for source in sources {
                        field(&mut hasher, &typed_id_bytes(source));
                    }
                    for destination in destinations {
                        field(
                            &mut hasher,
                            &destination
                                .cluster
                                .as_ref()
                                .map(typed_id_bytes)
                                .unwrap_or_default(),
                        );
                        field(
                            &mut hasher,
                            &destination
                                .new_id
                                .as_ref()
                                .map(typed_id_bytes)
                                .unwrap_or_default(),
                        );
                        field(
                            &mut hasher,
                            destination.label.as_deref().unwrap_or("").as_bytes(),
                        );
                        let mut members: Vec<_> = destination.members.iter().collect();
                        members.sort();
                        for member in members {
                            field(&mut hasher, &typed_id_bytes(member));
                        }
                    }
                }
            }
        }
        if let Some(summary) = &self.maintenance_summary {
            field(&mut hasher, b"summary");
            field(&mut hasher, summary.action.as_bytes());
            field(&mut hasher, summary.source_id.as_bytes());
            field(&mut hasher, summary.job.as_bytes());
            field(&mut hasher, summary.disposition.as_bytes());
            field(&mut hasher, &summary.members_moved.to_le_bytes());
        }
        for document in &self.raw_documents {
            field(&mut hasher, b"raw");
            field(&mut hasher, document.content.as_bytes());
        }
        for row in &self.provenance_rows {
            field(&mut hasher, b"provenance");
            field(&mut hasher, row.kind.as_bytes());
            field(&mut hasher, &row.timestamp.timestamp().to_le_bytes());
        }
        field(&mut hasher, b"heat");
        field(
            &mut hasher,
            &self
                .heat_seed
                .map(|heat| heat.to_bits().to_le_bytes().to_vec())
                .unwrap_or_default(),
        );
        // The approval clause is requested intent like every other field above: a caller that retries
        // the same placement after the proposal moved must replay, and one that reuses its key for a
        // different placement must conflict. It is hashed last so it cannot be confused with the scope
        // fact's own field framing, and with *typed* id bytes: `id_bytes` coalesces numeric and string
        // keys that render alike (`proposal:1` vs `proposal:'1'`). Preserve that historical encoding
        // outside this clause, but never let different proposal/fact/project IDs alias an approval.
        if let Some(approval) = &self.classification_approval {
            field(&mut hasher, b"classification-approval");
            field(&mut hasher, &typed_id_bytes(&approval.proposal));
            field(&mut hasher, &approval.proposal_revision.get().to_le_bytes());
            field(&mut hasher, &typed_id_bytes(approval.fact.as_record_id()));
            field(&mut hasher, &approval.source_revision.get().to_le_bytes());
            match &approval.destination {
                MemoryScope::Personal => field(&mut hasher, scope_kind::PERSONAL.as_bytes()),
                MemoryScope::Unassigned => field(&mut hasher, scope_kind::UNASSIGNED.as_bytes()),
                MemoryScope::Project(project) => {
                    field(&mut hasher, scope_kind::PROJECT.as_bytes());
                    field(&mut hasher, &typed_id_bytes(project.as_record_id()));
                }
            }
        }
        hex(&hasher)
    }

    /// The scope kinds this change touches, for the journal's `affected_scope_kinds`.
    pub fn affected_scope_kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = Vec::new();
        for fact in &self.facts {
            if let Some(scope) = fact.scope() {
                let kind = match scope {
                    MemoryScope::Personal => scope_kind::PERSONAL,
                    MemoryScope::Unassigned => scope_kind::UNASSIGNED,
                    MemoryScope::Project(_) => scope_kind::PROJECT,
                };
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        kinds
    }

    /// The projects this change names, for the journal's `affected_projects`.
    pub fn affected_projects(&self) -> Vec<RecordId> {
        let mut projects: Vec<RecordId> = Vec::new();
        for fact in &self.facts {
            if let Some(MemoryScope::Project(project)) = fact.scope() {
                let id = project.as_record_id().clone();
                if !projects.contains(&id) {
                    projects.push(id);
                }
            }
        }
        projects
    }
}

/// The effect vocabulary a semantic change labels itself with.
///
/// A closed set of verbs, kept apart from [`EdgeKind::effect`] because one names a graph relation and
/// the other names what happened to a memory's state; both end up in `expected_effects`, and a reader
/// comparing an operation's intent to its outcome needs them spelled from one place.
pub mod effect {
    pub const CONTENT: &str = "content";
    pub const TAGS: &str = "tags";
    pub const CONFIDENCE: &str = "confidence";
    pub const VECTOR: &str = "vector";
    pub const DELETE: &str = "visibility:delete";
    pub const QUARANTINE: &str = "visibility:quarantine";
    pub const SCOPE: &str = "scope";
    pub const STORE: &str = "store:memory";
    pub const LINEAGE: &str = "lineage:snapshot";
    pub const PROVENANCE: &str = "provenance";
    pub const CLUSTER_MEMBERSHIP: &str = "cluster:membership";
    pub const CLUSTER_CENTROID: &str = "cluster:centroid";
    pub const RAW_SOURCE: &str = "import:raw";
    pub const CHUNK: &str = "import:chunk";
}

/// One `maintenance_log` row a journaled background change links to itself.
///
/// Part of the payload rather than a write afterwards, because an audit summary that survives the
/// rollback of the change it describes is a trail about a write that never happened.
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceSummary {
    pub action: String,
    pub source_id: String,
    pub target_ids: Vec<String>,
    pub members_moved: i64,
    pub run_id: String,
    /// One of [`crate::models::maintenance::job`]'s constants.
    pub job: String,
    /// One of [`crate::models::maintenance::disposition`]'s constants.
    pub disposition: String,
    pub previous_value: Option<f64>,
}

/// What one applied operation left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedChange {
    /// The journal row's id, in this crate's canonical display form.
    pub operation: String,
    pub preimages: Vec<String>,
    pub postimages: Vec<String>,
    /// The metadata-only audit reference. Today that is the journal row itself — `semantic_operation`
    /// *is* the audit — and the field is kept separate because Task 20's proposal/attempt metadata hangs
    /// off the same operation without becoming part of corpus-write history.
    pub audit: String,
    /// The ids this transaction minted, in [`PreparedChangeSet::facts`] order.
    pub created: Vec<FactId>,
    /// The source documents this transaction minted, in [`PreparedChangeSet::raw_documents`] order.
    ///
    /// A separate list rather than a widened `created`, because [`FactId`](crate::models::identity::FactId)
    /// is sealed to the `fact` table and a `raw` row is not a memory. The import path needs the handle its
    /// own transaction produced: it cannot re-parse one out of a display form (`AGENTS.md` — that round trip
    /// is not parser-safe for numeric or quoted opaque keys) and must not re-query for it, because a re-read
    /// is a row the caller cannot prove it just wrote rather than one somebody else added a moment later.
    /// A replay reports none, because a replay minted none.
    pub created_raw: Vec<RecordId>,
    /// Whether this answer came from a duplicate same-key request rather than a fresh write.
    pub replayed: bool,
}

/// The batch answer for classification application: per-entry outcome, never one blurred story.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchResult {
    /// One journal row handle **per applied entry**, in the order the entries were given.
    ///
    /// There is deliberately no single "the batch operation" field: an applied batch writes N journal
    /// rows, one per memory, because each entry is its own transaction with its own staleness guard, and
    /// a field naming one of them as if it covered all of them would let a consumer cite an operation
    /// that does not describe the entries it is being read for.
    pub applied: Vec<String>,
    pub conflicts: Vec<BatchConflict>,
    pub stopped: Option<BatchStop>,
}

/// An outcome that could not be confirmed, followed by selections not dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchStop {
    pub proposal: String,
    pub unattempted: Vec<String>,
}

impl BatchResult {
    pub fn applied_count(&self) -> usize {
        self.applied.len()
    }

    pub fn is_complete(&self) -> bool {
        self.conflicts.is_empty() && self.stopped.is_none()
    }
}

/// Why one entry of a batch did not apply: the proposal it was for and the reason, and nothing else.
/// A conflict must not become a way to learn what is in somebody's memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchConflict {
    pub proposal: String,
    pub reason: BatchConflictReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchConflictReason {
    /// The memory moved or was edited since the proposal was formed.
    Stale,
    /// The memory is gone, or was never this caller's.
    NotFound,
    /// The destination names a project this caller does not own.
    InvalidDestination,
    /// The proposal is not pending, or was already decided.
    Decided,
    /// The fixed command exhausted its three numerical preparations.
    RetryExhausted,
    /// A typed command was not a lawful scope-only placement.
    InvalidRequest,
}

/// The journal row plus the exact before/after a private history page renders.
///
/// Read back through [`MutationRepo::operation`](crate::repos::MutationRepo::operation), which
/// authorizes the operation row and *each* snapshot against the caller's own workspace: an id alone can
/// name another owner's journal row, and a read that trusted the row's own snapshot list would turn the
/// history page into that probe.
#[derive(Debug, Clone, PartialEq)]
pub struct OperationDetail {
    pub id: String,
    pub kind: String,
    pub operation_key: String,
    pub actor: String,
    pub committed_at: Option<DateTime<Utc>>,
    pub expected_effects: Vec<String>,
    pub actual_effects: Vec<String>,
    pub preimages: Vec<SemanticSnapshot>,
    pub postimages: Vec<SemanticSnapshot>,
    /// Original owner-qualified journal records, in the same order as the typed images.
    /// These retain stored reference fields that the semantic projection does not reconstruct.
    pub preimage_records: Vec<Value>,
    pub postimage_records: Vec<Value>,
}

impl OperationDetail {
    /// One line of the owner's history list: what happened, when, and how many records it touched.
    ///
    /// No content, and no record ids: the list page is a navigation surface, and an operator-facing
    /// aggregate that leaked text would be a corpus read in disguise. `contents` counts *this*
    /// operation's own facts, which is what a list row can honestly say.
    pub fn summary(&self) -> OperationSummary {
        OperationSummary {
            id: self.id.clone(),
            kind: self.kind.clone(),
            operation_key: self.operation_key.clone(),
            actor: self.actor.clone(),
            committed_at: self.committed_at,
            effects: self.actual_effects.clone(),
            touched: self.preimages.len() + self.postimages.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSummary {
    pub id: String,
    pub kind: String,
    pub operation_key: String,
    pub actor: String,
    pub committed_at: Option<DateTime<Utc>>,
    pub effects: Vec<String>,
    pub touched: usize,
}

/// The closed operation kinds a change set may claim.
///
/// v009's `ASSERT $value IN [...]` is the enforcement; this list is the compile-time half, so a writer
/// cannot hand `apply` a kind the journal will refuse and then report the refusal as an engine fault.
pub const OPERATION_KINDS: &[&str] = &[
    operation_kind::STORE,
    operation_kind::IMPORT,
    operation_kind::UPDATE,
    operation_kind::DELETE,
    operation_kind::MOVE,
    operation_kind::CLUSTER_CHANGE,
    operation_kind::COLLAPSE,
    operation_kind::APPRAISE,
    operation_kind::REMINDER_MAP,
    operation_kind::QUALITY,
    operation_kind::REVERT,
    operation_kind::BOOTSTRAP_OWNER,
];

// -- hashing primitives -------------------------------------------------------------------------------

/// Length-prefixed field framing for the digests: a big-endian length, then the bytes.
fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u32).to_be_bytes());
    hasher.update(bytes);
}

/// A record id's stable bytes: table and key, never a SQL round trip.
///
/// `record_id_to_string` is not parser-safe for numeric or quoted opaque keys (AGENTS.md), and the
/// digest must not care about the *spelling* either — two renders of one key must hash the same.
fn id_bytes(id: &RecordId) -> Vec<u8> {
    let key = match &id.key {
        surrealdb::types::RecordIdKey::String(text) => text.clone(),
        surrealdb::types::RecordIdKey::Number(number) => number.to_string(),
        surrealdb::types::RecordIdKey::Uuid(uuid) => uuid.to_string(),
        other => format!("{other:?}"),
    };
    format!("{}\u{1}{key}", id.table).into_bytes()
}

/// Typed identity encoding for new approval/guard clauses. Existing request hashes stay unchanged.
/// SQL serialization preserves key types and escaping; these bytes are hashed, never interpolated.
fn typed_id_bytes(id: &RecordId) -> Vec<u8> {
    use surrealdb::types::ToSql;
    id.to_sql().into_bytes()
}

/// A relation endpoint's stable bytes: an existing handle, or the slot of a row this change mints.
fn target_bytes(target: &RelationTarget) -> String {
    match target {
        RelationTarget::Existing(id) => String::from_utf8_lossy(&id_bytes(id)).into_owned(),
        RelationTarget::NewFact(index) => format!("fact\u{1}{index}"),
        RelationTarget::NewRaw(index) => format!("raw\u{1}{index}"),
        RelationTarget::NewProvenance(index) => format!("provenance\u{1}{index}"),
    }
}

/// The scope half of a digest: the kind, and for a project the project's own handle.
///
/// A project's identity is part of the scope, so two projects of one workspace must not render to one
/// digest and let a move between them read as an unchanged row.
fn scope_label(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Personal => scope_kind::PERSONAL.to_string(),
        MemoryScope::Unassigned => scope_kind::UNASSIGNED.to_string(),
        MemoryScope::Project(project) => format!(
            "{}\u{1}{}",
            scope_kind::PROJECT,
            String::from_utf8_lossy(&id_bytes(project.as_record_id()))
        ),
    }
}

fn hex(hasher: &Sha256) -> String {
    hasher
        .clone()
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::identity::{FactId, WorkspaceId};

    fn workspace() -> WorkspaceId {
        WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap()
    }

    fn fact(key: &str) -> FactId {
        FactId::try_from(RecordId::new("fact", key)).unwrap()
    }

    fn snapshot(tags: &[&str], content: &str, confidence: f64) -> FactSnapshot {
        FactSnapshot {
            id: fact("one"),
            workspace: workspace(),
            scope: MemoryScope::Personal,
            version: Version::new(3),
            content: content.to_string(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            confidence,
            visibility: Visibility::live(),
            embedding: Some(EmbeddingSnapshot::new(vec![0.1, 0.2], Some("stub"))),
            relation_versions: vec![],
        }
    }

    /// The digest is about semantic state, and the framing cannot be collided by regrouping bytes.
    #[test]
    fn semantic_hash_separates_states_a_naive_concatenation_would_merge() {
        let ab_c = snapshot(&["ab", "c"], "x", 0.5).semantic_hash();
        let a_bc = snapshot(&["a", "bc"], "x", 0.5).semantic_hash();
        assert_ne!(
            ab_c, a_bc,
            "tag lists must hash by element, not by joined text"
        );
        assert_ne!(ab_c, snapshot(&["ab", "c"], "xy", 0.5).semantic_hash());
        assert_ne!(
            ab_c,
            snapshot(&["ab", "c"], "x", 0.6).semantic_hash(),
            "confidence is semantic state"
        );
    }

    /// Visibility and the graph move the digest; the edge's row handle does not.
    #[test]
    fn semantic_hash_moves_with_visibility_and_relations() {
        let live = snapshot(&[], "x", 0.5);
        let mut quarantined = live.clone();
        quarantined.visibility = Visibility::quarantined(Utc::now());
        assert_ne!(live.semantic_hash(), quarantined.semantic_hash());

        let mut deleted = live.clone();
        deleted.visibility = Visibility::deleted();
        assert_ne!(live.semantic_hash(), deleted.semantic_hash());

        let edge = |key: &str, to: &str| {
            EdgeSnapshot::memory(
                RecordId::new("memory_edge", key),
                "derived_from",
                live.id.as_record_id().clone(),
                RecordId::new("fact", to),
            )
        };
        let mut with_edge = live.clone();
        with_edge.relation_versions.push(edge("e1", "snapshot"));
        assert_ne!(
            live.semantic_hash(),
            with_edge.semantic_hash(),
            "a fact revision alone cannot certify an unchanged graph"
        );

        // The same relation, read through a different row handle, is the same fact about the graph.
        let mut other_handle = with_edge.clone();
        other_handle.relation_versions[0].edge = RecordId::new("memory_edge", "other");
        assert_eq!(with_edge.semantic_hash(), other_handle.semantic_hash());

        // …and a relation between different endpoints is not the same relation at all.
        let mut moved_endpoint = with_edge.clone();
        moved_endpoint.relation_versions[0].to = RecordId::new("fact", "someone-elses");
        assert_ne!(with_edge.semantic_hash(), moved_endpoint.semantic_hash());
    }

    /// Relation order is a read accident, not a semantic difference.
    #[test]
    fn semantic_hash_is_independent_of_relation_ordering() {
        let edge = |key: &str, to: &str| EdgeSnapshot {
            edge: RecordId::new("memory_edge", key),
            kind: EdgeKind::Memory("related".to_string()),
            from: RecordId::new("fact", key),
            to: RecordId::new("fact", to),
            strength: 1.0,
        };
        let mut first = snapshot(&[], "x", 0.5);
        first.relation_versions = vec![edge("a", "b"), edge("c", "d")];
        let mut second = snapshot(&[], "x", 0.5);
        second.relation_versions = vec![edge("c", "d"), edge("a", "b")];
        assert_eq!(first.semantic_hash(), second.semantic_hash());
    }

    /// A project destination is part of the scope, so a move between two projects cannot read as no
    /// change at all.
    #[test]
    fn semantic_hash_distinguishes_project_destinations() {
        let mut a = snapshot(&[], "x", 0.5);
        let mut b = snapshot(&[], "x", 0.5);
        a.scope =
            MemoryScope::Project(ProjectId::try_from(RecordId::new("project", "p_a")).unwrap());
        b.scope =
            MemoryScope::Project(ProjectId::try_from(RecordId::new("project", "p_b")).unwrap());
        assert_ne!(a.semantic_hash(), b.semantic_hash());
        b.scope = a.scope.clone();
        assert_eq!(a.semantic_hash(), b.semantic_hash());
    }

    /// Each variant reports its own kind and subject, which is what v009's `ASSERT` compares.
    #[test]
    fn snapshot_variants_report_their_own_subject_and_kind() {
        let reminder = SemanticSnapshot::Reminder(ReminderSnapshot {
            id: RecordId::new("reminder", "r1"),
            account: RecordId::new("account", "a1"),
            message: "check the deploy".to_string(),
            target_project: Some("alexandria".to_string()),
            legacy_target_project: None,
            project: None,
            migration_hold: Some(false),
            semantic_revision: Version::new(1),
            schedule_kind: Some(crate::models::reminder::schedule_kind::PATTERN.to_string()),
        });
        assert_eq!(reminder.kind(), crate::models::snapshot_kind::REMINDER);
        assert_eq!(reminder.subject().table.as_str(), "reminder");

        let fact = SemanticSnapshot::Fact(snapshot(&[], "x", 0.5));
        assert_eq!(fact.kind(), crate::models::snapshot_kind::FACT);
        assert_eq!(fact.subject().table.as_str(), "fact");
        assert!(fact.fact().is_some());
        assert!(reminder.fact().is_none());
    }

    fn classification_scope_change() -> PreparedChangeSet {
        PreparedChangeSet::new(operation_kind::MOVE)
            .effect(effect::SCOPE)
            .fact(FactChange::Update {
                fact: fact("ordinary-baseline"),
                expected: Version::ZERO,
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: Some(MemoryScope::Personal),
            })
    }

    fn classification_approval(proposal: RecordId) -> ClassificationApproval {
        ClassificationApproval {
            proposal,
            proposal_revision: Version::ZERO,
            fact: fact("ordinary-baseline"),
            source_revision: Version::ZERO,
            destination: MemoryScope::Personal,
        }
    }

    #[test]
    fn classification_repair_fingerprint_binds_approval() {
        let ordinary = classification_scope_change();
        assert_eq!(
            ordinary.request_fingerprint(),
            "fa7fff94d98ecc7a268c91a6f45f2d0adbb74965b9a767b31ab6d26c2c92c965",
            "ordinary scope-only requests retain their pre-clause fingerprint"
        );
        let approval = classification_approval(RecordId::new("classification_proposal", "one"));
        let original = ordinary.clone().approve_classification(approval.clone());
        assert_ne!(
            original.request_fingerprint(),
            ordinary.request_fingerprint(),
            "approval identity is requested intent"
        );
        let mut revised = approval.clone();
        revised.proposal_revision = Version::new(1);
        assert_ne!(
            original.request_fingerprint(),
            ordinary
                .clone()
                .approve_classification(revised)
                .request_fingerprint()
        );
        let mut destination = approval;
        destination.destination = MemoryScope::Unassigned;
        assert_ne!(
            original.request_fingerprint(),
            ordinary
                .approve_classification(destination)
                .request_fingerprint()
        );
        assert_eq!(
            original.request_fingerprint(),
            original.clone().request_fingerprint()
        );
    }

    #[test]
    fn classification_repair_fingerprint_distinguishes_typed_keys() {
        let ordinary = classification_scope_change();
        let numeric = ordinary
            .clone()
            .approve_classification(classification_approval(RecordId::new(
                "classification_proposal",
                1_i64,
            )));
        let text = ordinary
            .clone()
            .approve_classification(classification_approval(RecordId::new(
                "classification_proposal",
                "1",
            )));
        assert_ne!(numeric.request_fingerprint(), text.request_fingerprint());
        let approval = classification_approval(RecordId::new("classification_proposal", "one"));
        let mut numeric_fact = approval.clone();
        numeric_fact.fact = FactId::try_from(RecordId::new("fact", 1_i64)).unwrap();
        let mut text_fact = approval.clone();
        text_fact.fact = FactId::try_from(RecordId::new("fact", "1")).unwrap();
        assert_ne!(
            ordinary
                .clone()
                .approve_classification(numeric_fact)
                .request_fingerprint(),
            ordinary
                .clone()
                .approve_classification(text_fact)
                .request_fingerprint()
        );
        let mut numeric_project = approval.clone();
        numeric_project.destination =
            MemoryScope::Project(ProjectId::try_from(RecordId::new("project", 1_i64)).unwrap());
        let mut text_project = approval;
        text_project.destination =
            MemoryScope::Project(ProjectId::try_from(RecordId::new("project", "1")).unwrap());
        assert_ne!(
            ordinary
                .clone()
                .approve_classification(numeric_project)
                .request_fingerprint(),
            ordinary
                .approve_classification(text_project)
                .request_fingerprint(),
            "typed project IDs in the approval must not alias"
        );
    }

    /// The request fingerprint identifies a *request*, not a row.
    #[test]
    fn request_fingerprint_follows_effects_not_ids() {
        let base = PreparedChangeSet::new(operation_kind::UPDATE)
            .effect(effect::CONTENT)
            .fact(FactChange::Update {
                fact: fact("one"),
                expected: Version::new(1),
                content: Some("new text".to_string()),
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: None,
            });
        let replay = base.clone();
        assert_eq!(
            base.request_fingerprint(),
            replay.request_fingerprint(),
            "the same request replayed must match"
        );

        // The same key aiming at the *same row* with different effects is a different request.
        let different = PreparedChangeSet::new(operation_kind::UPDATE)
            .effect(effect::TAGS)
            .fact(FactChange::Update {
                fact: fact("one"),
                expected: Version::new(1),
                content: Some("other text".to_string()),
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: None,
            });
        assert_ne!(base.request_fingerprint(), different.request_fingerprint());

        // And a kind change alone is a different request too: `store` and `update` of identical bytes
        // are not the same operation on the journal.
        let other_kind = PreparedChangeSet {
            kind: operation_kind::STORE,
            ..base.clone()
        };
        assert_ne!(base.request_fingerprint(), other_kind.request_fingerprint());
    }

    /// An empty effect set is distinguishable from any real change — v009 refuses it, and the writer
    /// must not pre-validate it away.
    #[test]
    fn empty_effect_set_is_still_a_well_formed_payload() {
        let empty = PreparedChangeSet::new(operation_kind::UPDATE);
        assert!(empty.effects.is_empty());
        assert_eq!(empty.request_fingerprint().len(), 64);
    }

    /// A create's slot is its own index, and a raw document's slot does not collide with it.
    #[test]
    fn raw_documents_and_facts_address_distinct_slots() {
        let change = PreparedChangeSet::new(operation_kind::IMPORT)
            .raw("the whole document".to_string())
            .fact(FactChange::Create {
                scope: MemoryScope::Personal,
                content: "chunk".to_string(),
                tags: vec![],
                confidence: 1.0,
                embedding: EmbeddingSnapshot::new(vec![0.1], None),
                visibility: Visibility::live(),
                metadata: None,
            });
        assert_eq!(change.raw_documents[0].slot, 0);
        // The chunk is the second create this transaction issues.
        assert_eq!(
            change.facts[change.facts.len() - 1].target(),
            None,
            "a create has no id until the engine mints one"
        );
    }

    /// `affected_*` describes the change to the journal without ever naming a record's content.
    #[test]
    fn affected_partitions_are_deduplicated_and_typed() {
        let project = ProjectId::try_from(RecordId::new("project", "p_a")).unwrap();
        let change = PreparedChangeSet::new(operation_kind::MOVE)
            .fact(FactChange::Update {
                fact: fact("one"),
                expected: Version::new(1),
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: Some(MemoryScope::Project(project.clone())),
            })
            .fact(FactChange::Update {
                fact: fact("two"),
                expected: Version::new(1),
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: Some(MemoryScope::Project(project)),
            });
        assert_eq!(change.affected_scope_kinds(), vec![scope_kind::PROJECT]);
        assert_eq!(change.affected_projects().len(), 1);
    }
}
