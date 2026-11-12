//! The apply service: turning one request's intent into a journaled semantic change.
//!
//! Ordinary writers used to issue their own round trips. A store created the fact, then the heat row, then
//! the provenance row, then the cluster membership, then that cluster's centroid, then the session link — six
//! statements with no transaction across them, so a refusal at number four left a live memory with a vector
//! and no provenance, and an owner asking "what did this say before?" had no answer. An update was worse: the
//! lineage snapshot, the `derived_from` edge and the supersede were three calls, and the interim path
//! `.ok()`-ed the edge, so a failed edge produced a hidden row nothing pointed back from.
//!
//! This module is where that intent becomes a [`PreparedChangeSet`] and where it is committed. It lives in the
//! MCP crate rather than in storage for the reason [`MutationRepo`](alexandria_storage::repos::MutationRepo)
//! records: `alexandria-storage` cannot depend on `alexandria-engine` — the arrow points the other way — and a
//! cluster's centroid derived from the members a caller actually read is engine arithmetic. Storage's half is
//! to classify a conflict honestly (`Stale` for a semantic move, `RetryableBookkeepingConflict` for a derived
//! one); this module's half is what to do about the second class.
//!
//! # The approved-effect-set rule
//!
//! A change decides its cluster effect **once**, when the request first plans, and every later preparation
//! re-reads only the derived halves: that cluster's revision guards and its members' vectors. Re-deciding is
//! what would let a retry move a memory into a different cluster because an unrelated writer nudged a centroid
//! between the attempts — an effect change wearing a bookkeeping retry, which the plan forbids in the same
//! breath as it allows the retry. [`ApprovedCluster`] is therefore the fixed decision, [`ApplyService::prepare`]
//! is the re-derivation, and the effect-set comparison in [`ApplyService::commit`] makes the rule observable
//! rather than trusted: a preparation that would name different effects fails instead of journaling a
//! different operation under the same key.
//!
//! Cluster *ranking* is deliberately not here. The server already ranks its own candidates against its own
//! `cluster_join_threshold`, and a second ranking path in this module would mean two thresholds and two notions
//! of "the nearest cluster" that could disagree. A plan receives the decision that ranking produced.
//!
//! # Three preparations, no more
//!
//! [`MAX_PREPARATIONS`] bounds the loop at three within one live request. Each attempt re-asserts the caller's
//! authority inside the transaction (storage's `claim_authority`), so a credential revoked between attempts
//! loses the commit rather than inheriting the first attempt's admission. A change that still conflicts after
//! three is reported as the conflict it is: nothing is left half-applied, and no durable approval is recorded
//! anywhere, because this module writes no row of its own. The bound is not a performance knob — it is what
//! stops "retry the derived bookkeeping" from becoming a loop that holds a write transaction open.

use alexandria_engine::clusters::{centroid, centroid_with};
use alexandria_storage::access::{MutationContext, OwnerContext, ScopedContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{FactId, OperationId};
use alexandria_storage::models::mutation::{
    AppliedChange, ClusterChange, ClusterWriteExpectation, EdgeKind, EmbeddingSnapshot,
    ExpectedState, FactChange, PreparedChangeSet, RelationChange, RelationTarget, effect,
};
use alexandria_storage::repos::{MutationRepo, ScopedClusterRepo};
use surrealdb::types::RecordId;

/// The largest number of times one request may prepare the same semantic change.
///
/// One initial preparation plus the two extras the plan allows. Counted per request, never per cluster: a
/// change that touches three clusters whose guards all moved re-prepares the *change*, not each cluster.
pub use alexandria_storage::repos::MAX_PREPARATIONS;

/// The cluster effect one request is approved to apply, fixed when its plan is first built.
///
/// Create, Join and Revector serve ordinary writers. Depart and Attach describe a move of an existing
/// member, including whether a source centroid write was initially approved. Named identities and effect
/// shapes remain fixed; only guards and the means of the current eligible populations are refreshed.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovedCluster {
    /// Mint a cluster whose sole member is one of this change's own creates.
    Create {
        /// The index into [`PreparedChangeSet::facts`] of the member this cluster is built around.
        member: usize,
    },
    /// Attach one of this change's creates to a cluster this request already named.
    Join {
        cluster: RecordId,
        /// The index into [`PreparedChangeSet::facts`] of the member being attached.
        member: usize,
    },
    /// Recompute the centre of a cluster because one of its existing members changed vector.
    Revector {
        cluster: RecordId,
        /// The member whose vector moved. It stays a member; only its contribution changes.
        member: RecordId,
    },
    /// Leave a named source without changing the initially approved centroid-effect shape.
    Depart {
        cluster: RecordId,
        member: RecordId,
        writes_centroid: bool,
    },
    /// Hide a member's contribution without removing its historical membership.
    RemoveContribution {
        cluster: RecordId,
        member: RecordId,
        contributes: bool,
        writes_centroid: bool,
    },
    /// Join the named destination, or create one when creation was the initial decision.
    Attach {
        cluster: Option<RecordId>,
        member: RecordId,
        /// Eligibility is pinned with the first semantic state; hidden arrivals do not enter the mean.
        contributes: bool,
    },
}

impl ApprovedCluster {
    /// The effect labels this shape always contributes, whether or not its derived values move.
    ///
    /// A join moves the membership *and* the centre it recomputes, so it names both; a re-vectoring moves only
    /// the centre, and naming a membership effect it does not write would make the journal describe an edge the
    /// transaction never touched. A create attaches the member but mints no earlier centre to correct, so it
    /// names the membership alone — which is the same label storage's `move_scope` records for a detach.
    pub fn effects(&self) -> Vec<String> {
        let membership = EdgeKind::ClusterMembership.effect();
        match self {
            Self::Create { .. }
            | Self::Depart {
                writes_centroid: false,
                ..
            }
            | Self::Attach { cluster: None, .. } => vec![membership],
            Self::Join { .. }
            | Self::Depart {
                writes_centroid: true,
                ..
            }
            | Self::Attach {
                cluster: Some(_), ..
            } => {
                vec![membership, effect::CLUSTER_CENTROID.to_string()]
            }
            Self::Revector { .. }
            | Self::RemoveContribution {
                writes_centroid: true,
                ..
            } => {
                vec![effect::CLUSTER_CENTROID.to_string()]
            }
            Self::RemoveContribution {
                writes_centroid: false,
                ..
            } => vec![],
        }
    }
}

/// One semantic change, before it is prepared.
///
/// Wraps the payload so its effect set can be stated *through* the builder rather than remembered beside it:
/// every effect a writer records goes into the one [`PreparedChangeSet`] the commit compares, so an effect can
/// not be present in the payload and absent from the approved set, which is the discrepancy that would let a
/// retry look like a widening.
#[derive(Debug, Clone)]
pub struct Plan {
    change: PreparedChangeSet,
    clusters: Vec<ApprovedCluster>,
    /// The vector the cluster half of this change derives its centroid from.
    embedding: Option<Vec<f32>>,
}

impl Plan {
    /// A plan around one change of a closed `kind`.
    pub fn new(kind: &'static str) -> Self {
        Self {
            change: PreparedChangeSet::new(kind),
            clusters: Vec::new(),
            embedding: None,
        }
    }

    /// Record one approved effect the change writes.
    pub fn effect(mut self, label: impl Into<String>) -> Self {
        self.change = self.change.effect(label);
        self
    }

    /// Add one fact write.
    pub fn fact(mut self, change: FactChange) -> Self {
        self.change = self.change.fact(change);
        self
    }

    /// Add one relation this change creates.
    pub fn relate(mut self, change: RelationChange) -> Self {
        self.change = self.change.relate(change);
        self
    }

    /// Link one of this change's own creates to a session row this caller already resolved.
    pub fn link_session(mut self, session: RecordId, to: RelationTarget) -> Self {
        self.change = self.change.link_session(session, to);
        self
    }

    /// Record where a memory came from, linked in the same transaction as the row it describes.
    pub fn provenance(
        mut self,
        kind: impl Into<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.change = self.change.provenance(kind, timestamp);
        self
    }

    /// Seed the initial heat of every fact this change creates.
    pub fn heat(mut self, initial: f64) -> Self {
        self.change = self.change.heat(initial);
        self
    }

    /// Store a source document this change's chunks were cut from.
    pub fn raw(mut self, content: String) -> Self {
        self.change = self.change.raw(content);
        self
    }

    /// Stamp the embedding identity every vector in this change was produced under.
    pub fn in_space(mut self, model: Option<&str>) -> Self {
        self.change = self.change.in_space(model);
        self
    }

    /// Append a named cluster effect, fixed from here on.
    ///
    /// Recording the decision also records the effect labels that decision always contributes, which is what
    /// makes the approved effect set complete *before* any preparation runs: a preparation fills in derived
    /// values, it never widens the change's intent. A caller that added a cluster label at prepare time would
    /// be journaling an effect the request never asked for.
    pub fn cluster(mut self, approved: ApprovedCluster) -> Self {
        for label in approved.effects() {
            self.change = self.change.effect(label);
        }
        self.clusters.push(approved);
        self
    }
    /// The vector the cluster half derives a centroid from.
    pub fn embedding(mut self, embedding: &[f32]) -> Self {
        self.embedding = Some(embedding.to_vec());
        self
    }

    /// The effect labels approved so far, which every preparation must reproduce exactly.
    fn approved_effects(&self) -> &[String] {
        &self.change.effects
    }

    /// The vector the cluster half derives from, or a refusal naming the plan that lacked one.
    fn vector(&self) -> Result<&[f32], StorageError> {
        self.embedding
            .as_deref()
            .ok_or_else(|| invalid("an approved cluster effect carries no vector to derive from"))
    }
}

/// One caller's semantic writes, planned and committed under the authority that request carries.
pub struct ApplyService<'a> {
    /// The caller's write authority, re-named from the owner context the transport admitted.
    access: MutationContext,
    mutation: MutationRepo<'a>,
    /// The caller's read population, for the workspace-member reads a centroid is derived from.
    clusters: ScopedClusterRepo<'a>,
}

impl<'a> ApplyService<'a> {
    /// Binds the service to one admitted request.
    ///
    /// The [`MutationContext`] is minted here and nowhere else, and it is a re-naming of the owner authority
    /// the transport already issued: a caller that never went through admission has nothing to hand in, and the
    /// journal's `initiator` column records the credential that actually presented itself. A read context alone
    /// cannot reach this type, which is the boundary `AGENTS.md` draws for a journal write.
    pub fn new(
        db: &'a alexandria_storage::Database,
        owner: &'a OwnerContext,
        read: &'a ScopedContext,
    ) -> Self {
        let inner = db.inner();
        Self {
            access: MutationContext::issued_to(owner),
            mutation: MutationRepo::new(inner),
            clusters: ScopedClusterRepo::new(inner, owner, read),
        }
    }

    /// The cluster one of the caller's memories already belongs to, with its guards.
    ///
    /// A membership read, not a similarity guess: for a re-vectoring the question is "which cluster's stored
    /// centre is now wrong", which only the edge can answer. `None` means the memory is in no cluster, which
    /// makes the change a plain content update with no derived bookkeeping to defend.
    pub async fn cluster_of(
        &self,
        fact: &RecordId,
    ) -> Result<Option<(RecordId, ClusterWriteExpectation)>, StorageError> {
        self.mutation.cluster_of(self.access.owner(), fact).await
    }

    /// The semantic state of one of the caller's memories, or `None` when it is foreign or gone.
    ///
    /// The plan an update or a delete is built from this, and never from the row the caller *thinks* it read:
    /// `apply` compares the expectation against the state the transaction actually finds, so a version read
    /// from a stale response is a conflict rather than an overwrite.
    pub async fn state(
        &self,
        fact: &FactId,
    ) -> Result<Option<alexandria_storage::models::FactSnapshot>, StorageError> {
        self.mutation.state(self.access.owner(), fact).await
    }

    /// The expectation `apply` must find for `fact`, refused as `NotFound` when the caller cannot see it.
    pub async fn expected_state(&self, fact: &FactId) -> Result<ExpectedState, StorageError> {
        self.mutation
            .expected_state(self.access.owner(), fact)
            .await
    }

    /// The cluster one of the caller's memories already belongs to, by handle alone.
    pub async fn cluster_of_fact(&self, fact: &RecordId) -> Result<Option<RecordId>, StorageError> {
        Ok(self
            .cluster_of(fact)
            .await?
            .map(|(cluster, _expectation)| cluster))
    }

    /// Prepare `plan` against the cluster state as it is *now*, and commit it.
    ///
    /// Delegates the retry policy to storage's shared `commit_prepared` driver. Every preparation
    /// refreshes this plan's guards, populations and means; it does not choose a new identity or effect
    /// set. Only an apply-side [`StorageError::RetryableBookkeepingConflict`] permits another attempt.
    /// Preparation errors and every other apply error return immediately.
    ///
    /// Storage's `classification_repair_driver_*` tests exercise the actual shared driver's second-attempt
    /// success, three-attempt exhaustion, effect mismatch and immediate error paths. The MCP pinned-move
    /// tests cover preparation refresh and fixed destinations separately; they do not force a second
    /// driver attempt through this thin adapter or add production fault-injection hooks.
    pub async fn commit(
        &self,
        operation: &OperationId,
        expected: &[ExpectedState],
        plan: &Plan,
    ) -> Result<AppliedChange, StorageError> {
        // The loop itself lives in storage's shared driver (`commit_prepared`), so the classification
        // batch and this service cannot drift into two different retry policies. `prepare` re-derives
        // the approved clusters' guards and means on every attempt; the driver compares the prepared
        // effect set against the approved one and refuses a mismatch as `Stale` rather than letting a
        // retry quietly become a different operation.
        let approved = plan.approved_effects();
        alexandria_storage::repos::commit_prepared(
            &self.mutation,
            &self.access,
            operation,
            expected,
            approved,
            || self.prepare(plan),
        )
        .await
    }

    /// Read whether departure includes a centroid write when the intent is first pinned.
    pub(crate) async fn departure(
        &self,
        cluster: &RecordId,
        member: &RecordId,
    ) -> Result<ApprovedCluster, StorageError> {
        let population = self.survivor_vectors(cluster, member).await?;
        let held: Vec<&[f32]> = population.iter().map(Vec::as_slice).collect();
        let mean = centroid(&held);
        if !population.is_empty() && mean.is_none() {
            return Err(invalid(
                "a source cluster's survivors carry no common dimension",
            ));
        }
        Ok(ApprovedCluster::Depart {
            cluster: cluster.clone(),
            member: member.clone(),
            writes_centroid: mean.is_some(),
        })
    }

    /// Pin the actual source and whether hiding this member changes its live mean. Hidden members
    /// contribute nothing; a last live member leaves no new mean. Both cases still need a guard.
    pub(crate) async fn contribution_removal(
        &self,
        member: &RecordId,
    ) -> Result<Option<ApprovedCluster>, StorageError> {
        let Some((cluster, expectation)) = self.cluster_of(member).await? else {
            return Ok(None);
        };
        let contributes = expectation
            .population
            .iter()
            .any(|entry| &entry.id == member);
        let held: Vec<&[f32]> = expectation
            .population
            .iter()
            .filter(|entry| &entry.id != member)
            .map(|entry| entry.embedding.as_slice())
            .collect();
        let mean = if contributes { centroid(&held) } else { None };
        if contributes && !held.is_empty() && mean.is_none() {
            return Err(invalid(
                "a source cluster's survivors carry no common dimension",
            ));
        }
        Ok(Some(ApprovedCluster::RemoveContribution {
            cluster,
            member: member.clone(),
            contributes,
            writes_centroid: mean.is_some(),
        }))
    }

    /// Refresh only the derived data of the already named effects.
    async fn prepare(&self, plan: &Plan) -> Result<PreparedChangeSet, StorageError> {
        let mut change = plan.change.clone();
        for approved in &plan.clusters {
            change = change.cluster(self.prepare_cluster(plan, approved).await?);
        }
        Ok(change)
    }

    async fn prepare_cluster(
        &self,
        plan: &Plan,
        approved: &ApprovedCluster,
    ) -> Result<ClusterChange, StorageError> {
        // Each expectation captures counters then ONE population. Arithmetic consumes that sample;
        // the transaction validates it even when a compatibility writer changed no counter.
        Ok(match approved {
            ApprovedCluster::Create { member } => ClusterChange::Create {
                centroid: plan.vector()?.to_vec(),
                label: None,
                member: RelationTarget::NewFact(*member),
            },
            ApprovedCluster::Join { cluster, member } => {
                let expectation = self.guard(cluster).await?;
                let held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .map(|member| member.embedding.as_slice())
                    .collect();
                let centroid = centroid_with(&held, plan.vector()?)
                    .ok_or_else(|| invalid("a cluster's members carry no common dimension"))?;
                ClusterChange::Effect {
                    expectation,
                    add_member: Some(RelationTarget::NewFact(*member)),
                    remove_member: None,
                    centroid: Some(centroid),
                }
            }
            ApprovedCluster::Revector { cluster, member } => {
                let expectation = self.guard(cluster).await?;
                let held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .filter(|entry| &entry.id != member)
                    .map(|entry| entry.embedding.as_slice())
                    .collect();
                let with_moved: Vec<&[f32]> = [held.as_slice(), &[plan.vector()?]].concat();
                let centroid = centroid(&with_moved).ok_or_else(|| {
                    invalid("a re-vectored cluster has no population to derive from")
                })?;
                ClusterChange::Effect {
                    expectation,
                    add_member: None,
                    remove_member: None,
                    centroid: Some(centroid),
                }
            }
            ApprovedCluster::Depart {
                cluster,
                member,
                writes_centroid,
            } => {
                let expectation = self.guard(cluster).await?;
                let held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .filter(|entry| &entry.id != member)
                    .map(|entry| entry.embedding.as_slice())
                    .collect();
                let surviving_centroid = centroid(&held);
                if !held.is_empty() && surviving_centroid.is_none() {
                    return Err(invalid(
                        "a source cluster's survivors carry no common dimension",
                    ));
                }
                if surviving_centroid.is_some() != *writes_centroid {
                    return Err(StorageError::RetryableBookkeepingConflict);
                }
                ClusterChange::Detach {
                    expectation,
                    member: member.clone(),
                    surviving_centroid,
                }
            }
            ApprovedCluster::RemoveContribution {
                cluster,
                member,
                contributes,
                writes_centroid,
            } => {
                let expectation = self.guard(cluster).await?;
                if expectation
                    .population
                    .iter()
                    .any(|entry| &entry.id == member)
                    != *contributes
                {
                    return Err(StorageError::RetryableBookkeepingConflict);
                }
                let held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .filter(|entry| &entry.id != member)
                    .map(|entry| entry.embedding.as_slice())
                    .collect();
                let mean = if *contributes { centroid(&held) } else { None };
                if *contributes && !held.is_empty() && mean.is_none() {
                    return Err(invalid(
                        "a source cluster's survivors carry no common dimension",
                    ));
                }
                if mean.is_some() != *writes_centroid {
                    return Err(StorageError::RetryableBookkeepingConflict);
                }
                match mean {
                    Some(centroid) => ClusterChange::Effect {
                        expectation,
                        add_member: None,
                        remove_member: None,
                        centroid: Some(centroid),
                    },
                    None => ClusterChange::Guard { expectation },
                }
            }
            ApprovedCluster::Attach {
                cluster: Some(cluster),
                member,
                contributes,
            } => {
                let expectation = self.guard(cluster).await?;
                let held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .map(|entry| entry.embedding.as_slice())
                    .collect();
                let mean = if *contributes {
                    centroid_with(&held, plan.vector()?)
                } else {
                    centroid(&held)
                };
                let centroid = mean.ok_or_else(|| {
                    if held.is_empty() {
                        StorageError::RetryableBookkeepingConflict
                    } else {
                        invalid("a cluster's members carry no common dimension")
                    }
                })?;
                ClusterChange::Effect {
                    expectation,
                    add_member: Some(RelationTarget::Existing(member.clone())),
                    remove_member: None,
                    centroid: Some(centroid),
                }
            }
            ApprovedCluster::Attach {
                cluster: None,
                member,
                ..
            } => ClusterChange::Create {
                centroid: plan.vector()?.to_vec(),
                label: None,
                member: RelationTarget::Existing(member.clone()),
            },
        })
    }

    async fn survivor_vectors(
        &self,
        cluster: &RecordId,
        member: &RecordId,
    ) -> Result<Vec<Vec<f32>>, StorageError> {
        Ok(self
            .members(cluster)
            .await?
            .into_iter()
            .filter(|(id, _)| id != member)
            .map(|(_, embedding)| embedding)
            .collect())
    }

    /// The cluster's three revision guards, read at the moment of this attempt.
    async fn guard(&self, cluster: &RecordId) -> Result<ClusterWriteExpectation, StorageError> {
        self.mutation
            .cluster_expectation(self.access.owner(), cluster)
            .await
    }

    /// The vectors of `cluster`'s live workspace members, paired with their handles.
    ///
    /// The workspace population, not the caller's selected partitions: a cluster's centroid is the centre of
    /// every member it has, and averaging only the members in one partition would rewrite a mixed cluster's
    /// centre from a fraction of it — at the degenerate case of zero eligible members, replacing the whole
    /// centroid with the one vector this change carries. One read serves both halves because pairing vectors
    /// with handles by position across *two* reads is a silent misalignment the moment either read's filter
    /// or order changes.
    async fn members(&self, cluster: &RecordId) -> Result<Vec<(RecordId, Vec<f32>)>, StorageError> {
        Ok(self
            .clusters
            .workspace_members_of(cluster)
            .await?
            .into_iter()
            .filter_map(|member| member.id.map(|id| (id, member.embedding)))
            .collect())
    }

    /// The caller's own workspace, spelled the way a journal read binds it.
    pub fn workspace(&self) -> &alexandria_storage::models::WorkspaceId {
        self.access.workspace()
    }
}

fn invalid(text: &str) -> StorageError {
    StorageError::InvalidInput(text.to_string())
}

/// The vector a change writes, in the closed snapshot form the payload takes.
pub fn vector(embedding: &[f32], model: Option<&str>) -> EmbeddingSnapshot {
    EmbeddingSnapshot::new(embedding.to_vec(), model)
}

#[cfg(test)]
mod tests {
    //! The retry policy's own invariants, asserted where the private state lives.
    //!
    //! These tests cover effect labels and the configured bound. Real-store pinned-plan tests live in
    //! debug::collections::pinned_move_tests. Neither set proves wrapper success specifically on attempt
    //! two or executes the generic effect-comparison failure branch; those remain explicit carries.

    use super::*;
    use alexandria_storage::models::operation_kind;

    #[test]
    fn an_approved_cluster_shape_names_its_effects_once_and_stably() {
        let cluster = RecordId::new("cluster", "c1");
        let member = RecordId::new("fact", "f1");
        let shapes = [
            ApprovedCluster::Create { member: 0 },
            ApprovedCluster::Join {
                cluster: cluster.clone(),
                member: 0,
            },
            ApprovedCluster::Revector {
                cluster: cluster.clone(),
                member: member.clone(),
            },
        ];
        for shape in shapes {
            let labels = shape.effects();
            assert!(!labels.is_empty(), "{shape:?} names no effect at all");
            assert_eq!(
                labels,
                shape.effects(),
                "the same shape rendered two effect sets"
            );
            let mut unique = labels.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                labels.len(),
                "{shape:?} named one label twice: {labels:?}"
            );
            for label in &labels {
                // v009 asserts only that every effect is a non-empty string, and the namespace has two
                // legal spellings — a `effect::*` constant like `cluster:centroid`, and an edge label
                // rendered from `EdgeKind::effect()` like `contains_memory`. Both are the journal's
                // vocabulary; an empty label is the only shape the schema refuses.
                assert!(
                    !label.is_empty(),
                    "{shape:?} contributed an empty effect label"
                );
            }
        }
        // The three shapes are distinguishable by effect set, which is what makes a preparation that
        // switched between them observable rather than silent.
        let create = ApprovedCluster::Create { member: 0 }.effects();
        let join = ApprovedCluster::Join {
            cluster: cluster.clone(),
            member: 0,
        }
        .effects();
        let revector = ApprovedCluster::Revector { cluster, member }.effects();
        assert_ne!(create, join);
        assert_ne!(join, revector);
        assert_ne!(create, revector);
    }

    /// The bound is the plan's contract — one preparation plus the two extras — not a tuning value.
    #[test]
    fn the_preparation_bound_is_three_total_within_one_request() {
        assert_eq!(
            MAX_PREPARATIONS, 3,
            "the plan allows at most two extra preparations after the first; a larger bound would hold a
             write transaction open longer than the derived-bookkeeping rule authorises"
        );
    }

    /// A plan's approved set is what `commit` compares against, so recording a cluster decision must add
    /// its labels to the plan rather than leave them to be remembered beside it.
    #[test]
    fn recording_a_cluster_decision_extends_the_plans_own_effect_set() {
        let plain = Plan::new(operation_kind::UPDATE).effect(effect::CONTENT);
        assert_eq!(plain.approved_effects(), vec![effect::CONTENT.to_string()]);
        let with_cluster = plain.clone().cluster(ApprovedCluster::Revector {
            cluster: RecordId::new("cluster", "c1"),
            member: RecordId::new("fact", "f1"),
        });
        let labels = with_cluster.approved_effects();
        assert!(
            labels.contains(&effect::CLUSTER_CENTROID.to_string()),
            "the cluster's own label is missing from the approved set: {labels:?}"
        );
        assert!(
            labels.contains(&effect::CONTENT.to_string()),
            "adding a cluster decision dropped what the plan already approved: {labels:?}"
        );
    }
}
