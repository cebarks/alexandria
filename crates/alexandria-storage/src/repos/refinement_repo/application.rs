//! Plan quality effects from the sealed proposal and current derived state. The kernel admits and
//! journals the plan; preparation carries vectors, never permission to write a row.

use crate::access::MutationContext;
use crate::models::identity::RefinementProposalId;
use crate::models::mutation::{
    ClusterChange, EmbeddingSnapshot, FactChange, FactSnapshot, RelationChange, RelationTarget,
    Visibility, effect,
};
use crate::models::operation_kind;
use crate::models::refinement::{GeneratedSession, QualityDetails, QualityProposalTransition};
use crate::repos::{MAX_PREPARATIONS, MutationRepo};

use super::*;

#[cfg(test)]
mod tests;

impl RefinementRepo<'_> {
    /// Pin the exact graph seen at assessment admission, before the inference request departs.
    pub async fn capture_sources(
        &self,
        system: &SystemContext,
        evidence: &EvidenceBundle,
    ) -> Result<Vec<ExpectedState>, StorageError> {
        let mutation = MutationRepo::new(self.db.inner());
        let partition = &evidence.focal.partition;
        let access = mutation
            .system_mutation_context(self.db, system, partition)
            .await?;
        let mut sources = Vec::with_capacity(1 + evidence.related.len());
        for candidate in std::iter::once(&evidence.focal).chain(&evidence.related) {
            let snapshot = mutation
                .state(access.owner(), &candidate.id)
                .await?
                .ok_or(StorageError::Stale)?;
            if snapshot.scope != partition.scope
                || candidate.partition != *partition
                || snapshot.version != candidate.version
                || snapshot.content != candidate.complete_content
                || snapshot.tags != candidate.tags
                || snapshot.visibility.deleted
                || snapshot.visibility.quarantined_at.is_some()
            {
                return Err(StorageError::Stale);
            }
            sources.push(ExpectedState::of(&snapshot));
        }
        Ok(sources)
    }

    pub async fn apply_human(
        &self,
        owner: &OwnerContext,
        proposal: &ProposalRevision,
        operation: &OperationId,
        prepared: &PreparedChangeSet,
    ) -> Result<AppliedChange, StorageError> {
        let access = MutationContext::issued_to(owner);
        apply_quality(
            self.db.inner().clone(),
            access,
            proposal.clone(),
            operation.clone(),
            prepared.clone(),
            None,
        )
        .await
    }

    /// Discover a duplicate approval without needing the embedding provider again.
    pub async fn replay_human(
        &self,
        owner: &OwnerContext,
        proposal: &ProposalRevision,
        operation: &OperationId,
    ) -> Result<Option<AppliedChange>, StorageError> {
        let mutation = MutationRepo::new(self.db.inner());
        if mutation
            .operation(owner, operation.as_str())
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let envelope = quality_envelope(proposal, None)?;
        mutation
            .apply(
                &MutationContext::issued_to(owner),
                operation,
                &[],
                &envelope,
            )
            .await
            .map(Some)
    }

    pub async fn apply_secondary(
        &self,
        system: &SystemContext,
        proposal: &ProposalRevision,
        generation: u64,
        operation: &OperationId,
        prepared: &PreparedChangeSet,
    ) -> Result<AppliedChange, StorageError> {
        self.require_system(system)?;
        if proposal.origin_policy.mode != ApprovalMode::SecondaryLlm
            || proposal.partition.scope == MemoryScope::Unassigned
            || proposal.evidence.coverage.selection != EvidenceSelectionKind::Scheduled
        {
            return Err(StorageError::Stale);
        }
        let access = MutationContext::issued_by_system(
            system,
            proposal.origin_policy.account.clone(),
            proposal.partition.workspace.clone(),
        );
        apply_quality(
            self.db.inner().clone(),
            access,
            proposal.clone(),
            operation.clone(),
            prepared.clone(),
            Some(generation),
        )
        .await
    }

    pub async fn revert(
        &self,
        owner: &OwnerContext,
        applied: &SemanticOperationId,
        operation: &OperationId,
    ) -> Result<AppliedChange, StorageError> {
        MutationRepo::new(self.db.inner())
            .revert_quality(owner, applied, operation)
            .await
    }

    pub async fn defer_proposal_revision(
        &self,
        owner: &OwnerContext,
        id: &RefinementProposalId,
        revision: Version,
    ) -> Result<(), StorageError> {
        decide_review(
            self.db.inner().clone(),
            owner.clone(),
            id.clone(),
            revision,
            true,
        )
        .await
    }

    pub async fn record_failed_application(
        &self,
        owner: &OwnerContext,
        proposal: &ProposalRevision,
        operation: &OperationId,
    ) -> Result<(), StorageError> {
        record_failure(
            self.db.inner().clone(),
            owner.clone(),
            proposal.clone(),
            operation.clone(),
            None,
        )
        .await
    }

    pub async fn record_failed_secondary(
        &self,
        system: &SystemContext,
        proposal: &ProposalRevision,
        operation: &OperationId,
        generation: u64,
    ) -> Result<(), StorageError> {
        self.require_system(system)?;
        let access = crate::access::AccessRepo::new(self.db);
        let principal = access
            .local_principal(system, &proposal.origin_policy.account)
            .await?;
        let owner = access.authorize(&principal).await?;
        validate_proposal(&owner, proposal)?;
        record_failure(
            self.db.inner().clone(),
            owner,
            proposal.clone(),
            operation.clone(),
            Some(generation),
        )
        .await
    }
}

async fn record_failure(
    db: Surreal<Any>,
    owner: OwnerContext,
    proposal: ProposalRevision,
    operation: OperationId,
    generation: Option<u64>,
) -> Result<(), StorageError> {
    for _ in 0..MAX_PREPARATIONS {
        let tx = db.clone().begin().await.map_err(internal)?;
        let result = record_failed_in_tx(&tx, &owner, &proposal, &operation, generation).await;
        match result {
            Ok(()) => match tx.commit().await {
                Ok(_) => return Ok(()),
                Err(error) if is_write_conflict(&error) => {}
                Err(error) => return Err(internal(error)),
            },
            Err(error) => {
                tx.cancel().await.map_err(internal)?;
                if !can_retry(&error) {
                    return Err(error);
                }
            }
        }
    }
    Err(StorageError::RetryableBookkeepingConflict)
}

pub(super) async fn decide_review(
    db: Surreal<Any>,
    owner: OwnerContext,
    id: RefinementProposalId,
    revision: Version,
    defer: bool,
) -> Result<(), StorageError> {
    for _ in 0..MAX_PREPARATIONS {
        let tx = db.clone().begin().await.map_err(internal)?;
        let result = decide_review_in_tx(&tx, &owner, &id, revision, defer).await;
        match result {
            Ok(()) => match tx.commit().await {
                Ok(_) => return Ok(()),
                Err(error) if is_write_conflict(&error) => {}
                Err(error) => return Err(internal(error)),
            },
            Err(error) => {
                tx.cancel().await.map_err(internal)?;
                if !can_retry(&error) {
                    return Err(error);
                }
            }
        }
    }
    Err(StorageError::RetryableBookkeepingConflict)
}

async fn decide_review_in_tx(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    id: &RefinementProposalId,
    revision: Version,
    defer: bool,
) -> Result<(), StorageError> {
    claim_owner(tx, owner).await?;
    let row = read_decision(tx, owner, id.as_record_id())
        .await?
        .ok_or(StorageError::NotFound)?;
    if row.revision != revision.get() || row.state != ProposalState::Pending {
        return Err(StorageError::Stale);
    }
    tx.query("UPDATE $id SET state = $state, deferred = $defer RETURN NONE;")
        .bind(("id", row.id))
        .bind(("state", if defer { "pending" } else { "dismissed" }))
        .bind(("defer", defer))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    Ok(())
}

async fn apply_quality(
    db: Surreal<Any>,
    access: MutationContext,
    proposal: ProposalRevision,
    operation: OperationId,
    prepared: PreparedChangeSet,
    generation: Option<u64>,
) -> Result<AppliedChange, StorageError> {
    let owner = access.owner();
    validate_proposal(owner, &proposal)?;
    let mutation = MutationRepo::new(&db);
    let envelope = quality_envelope(&proposal, generation)?;
    let mut pinned = None;
    for _ in 0..MAX_PREPARATIONS {
        if mutation
            .operation(owner, operation.as_str())
            .await?
            .is_some()
        {
            return mutation.apply(&access, &operation, &[], &envelope).await;
        }
        let mut change = envelope.clone();
        change.embedding_space.clone_from(&prepared.embedding_space);
        match plan_quality(
            &mutation,
            owner,
            &proposal,
            &operation,
            &prepared,
            &mut change,
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                // The duplicate may have committed while this request was reading preimages.
                if mutation
                    .operation(owner, operation.as_str())
                    .await?
                    .is_some()
                {
                    return mutation.apply(&access, &operation, &[], &envelope).await;
                }
                return Err(error);
            }
        }
        let shape = intent_shape(&change);
        if pinned.as_ref().is_some_and(|seen| seen != &shape) {
            return Err(StorageError::Stale);
        }
        pinned = Some(shape);
        match mutation
            .apply(&access, &operation, &proposal.sources, &change)
            .await
        {
            Err(StorageError::RetryableBookkeepingConflict) => tokio::task::yield_now().await,
            outcome => return outcome,
        }
    }
    Err(StorageError::RetryableBookkeepingConflict)
}

fn quality_envelope(
    proposal: &ProposalRevision,
    generation: Option<u64>,
) -> Result<PreparedChangeSet, StorageError> {
    let mut envelope = PreparedChangeSet::new(operation_kind::QUALITY);
    envelope.quality_transition = Some(QualityProposalTransition {
        proposal: RefinementProposalId::try_from(proposal.id.clone())?,
        revision: proposal.revision,
    });
    envelope.quality_details = Some(QualityDetails {
        proposal: Some(proposal.clone()),
        request_payload_hash: Some(sha256(proposal_payload(proposal)?.as_bytes())),
        generation,
        ..QualityDetails::default()
    });
    Ok(envelope)
}

// Derived numbers/revisions may be refreshed; the exact endpoints and semantic writes may not.
fn intent_shape(change: &PreparedChangeSet) -> PreparedChangeSet {
    let mut pinned = change.clone();
    for cluster in &mut pinned.cluster_changes {
        if let ClusterChange::Effect {
            expectation,
            centroid,
            ..
        } = cluster
        {
            expectation.membership_revision = Version::ZERO;
            expectation.member_embedding_revision = Version::ZERO;
            expectation.centroid_revision = Version::ZERO;
            expectation.population.clear();
            if centroid.is_some() {
                *centroid = Some(Vec::new());
            }
        }
    }
    // The quarantine timestamp is materialization, not a change in whether a fact is quarantined.
    for fact in &mut pinned.facts {
        if let FactChange::Update {
            visibility: Some(visibility),
            ..
        } = fact
            && visibility.quarantined_at.is_some()
        {
            visibility.quarantined_at = Some(DateTime::<Utc>::UNIX_EPOCH);
        }
    }
    pinned
}

async fn plan_quality(
    mutation: &MutationRepo<'_>,
    owner: &OwnerContext,
    proposal: &ProposalRevision,
    operation: &OperationId,
    prepared: &PreparedChangeSet,
    change: &mut PreparedChangeSet,
) -> Result<(), StorageError> {
    let mut rows: Vec<(FactId, FactSnapshot)> = Vec::new();
    let candidates = std::iter::once(&proposal.evidence.focal).chain(&proposal.evidence.related);
    for candidate in candidates {
        let Some(expected) = proposal
            .sources
            .iter()
            .find(|state| state.fact == candidate.id)
        else {
            return Err(StorageError::Stale);
        };
        let state = mutation
            .state(owner, &candidate.id)
            .await?
            .ok_or(StorageError::Stale)?;
        if ExpectedState::of(&state) != *expected
            || state.scope != proposal.partition.scope
            || state.visibility.deleted
            || state.visibility.quarantined_at.is_some()
        {
            return Err(StorageError::Stale);
        }
        rows.push((candidate.id.clone(), state));
    }
    // `FactId` has interior mutability, so it cannot key a `BTreeMap` (`mutable_key_type`); the
    // branch's established shape is a sorted, deduplicated Vec plus a `row` lookup. The sort
    // reproduces the map's key order, which `sessions`' first-match scan depends on, and the
    // post-dedup length keeps the distinct-fact count the map's len check compared.
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.dedup_by(|a, b| a.0 == b.0);
    if rows.len() != proposal.sources.len() {
        return Err(StorageError::Stale);
    }
    let mut planner = Planner {
        proposal,
        rows,
        change,
        prepared: &prepared.quality_embeddings,
    };
    planner.semantic(operation)?;
    planner.clusters(mutation, owner).await
}

struct Planner<'a> {
    proposal: &'a ProposalRevision,
    rows: Vec<(FactId, FactSnapshot)>,
    change: &'a mut PreparedChangeSet,
    prepared: &'a [crate::models::refinement::PreparedQualityEmbedding],
}

impl Planner<'_> {
    /// The `BTreeMap` index the row list replaces: same panic-on-missing contract, and the
    /// planner only ever looks up facts the proposal's own grounding validated into `rows`.
    fn row(&self, id: &FactId) -> &FactSnapshot {
        self.rows
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, row)| row)
            .expect("planner row is grounded in the proposal sources")
    }

    fn semantic(&mut self, operation: &OperationId) -> Result<(), StorageError> {
        let expected_vectors = match &self.proposal.proposed {
            QualityOperation::Rewrite { .. } => 1,
            QualityOperation::Split { parts, .. } => parts.len(),
            _ => 0,
        };
        if self.prepared.len() != expected_vectors {
            return Err(invalid_quality());
        }
        match &self.proposal.proposed {
            QualityOperation::Rewrite { source, content } => {
                let row = self.row(source).clone();
                self.change.effects.extend([
                    effect::CONTENT.into(),
                    effect::VECTOR.into(),
                    effect::LINEAGE.into(),
                ]);
                self.change.facts.push(create(
                    &row,
                    row.content.clone(),
                    row.embedding.clone().ok_or_else(invalid_quality)?,
                    Visibility::deleted(),
                ));
                let mut write = update(&row, None);
                if let FactChange::Update {
                    content: text,
                    embedding,
                    ..
                } = &mut write
                {
                    *text = Some(content.clone());
                    *embedding = Some(self.embedding(0, content)?);
                }
                self.change.facts.push(write);
                self.lineage(existing(source), RelationTarget::NewFact(0));
            }
            QualityOperation::Consolidate { canonical, sources } => {
                let mut tags = self.row(canonical).tags.clone();
                for source in sources {
                    for tag in &self.row(source).tags {
                        if !tags.contains(tag) {
                            tags.push(tag.clone());
                        }
                    }
                    self.retire(source);
                    self.lineage(existing(source), existing(canonical));
                    self.sessions(source, &existing(canonical));
                }
                let mut write = update(self.row(canonical), None);
                if let FactChange::Update { tags: field, .. } = &mut write {
                    *field = Some(tags);
                }
                self.change.facts.push(write);
                self.change.effects.extend([
                    effect::TAGS.into(),
                    effect::DELETE.into(),
                    effect::LINEAGE.into(),
                ]);
            }
            QualityOperation::Split { source, parts } => {
                let row = self.row(source).clone();
                self.retire(source);
                self.change.effects.extend([
                    effect::CONTENT.into(),
                    effect::VECTOR.into(),
                    effect::DELETE.into(),
                    effect::LINEAGE.into(),
                ]);
                for (slot, text) in parts.iter().enumerate() {
                    let embedding = self.embedding(slot, text)?;
                    self.change.facts.push(create(
                        &row,
                        text.clone(),
                        embedding,
                        Visibility::live(),
                    ));
                    self.change
                        .quality_details
                        .as_mut()
                        .ok_or_else(invalid_quality)?
                        .stable_slots
                        .push(descendant(&self.proposal.partition, operation, slot)?);
                    self.lineage(RelationTarget::NewFact(slot), existing(source));
                    self.sessions(source, &RelationTarget::NewFact(slot));
                }
            }
            QualityOperation::Noise { source } => {
                self.change.facts.push(update(
                    self.row(source),
                    Some(Visibility::quarantined(Utc::now())),
                ));
                self.change.effects.push(effect::QUARANTINE.into());
            }
            QualityOperation::Supersede {
                source,
                replacement,
            } => {
                if !explicit_supersession(self.proposal, self.row(source), self.row(replacement)) {
                    return Err(invalid_quality());
                }
                self.retire(source);
                self.lineage(existing(source), existing(replacement));
                self.change
                    .effects
                    .extend([effect::DELETE.into(), effect::LINEAGE.into()]);
            }
            QualityOperation::Contradiction { .. } | QualityOperation::NoSafeChange => {
                return Err(invalid_quality());
            }
        }
        let details = self
            .change
            .quality_details
            .as_ref()
            .ok_or_else(invalid_quality)?;
        if !details.generated_sessions.is_empty() {
            self.change
                .effects
                .push(EdgeKind::SessionMembership.effect());
        }
        self.validate_relationship_effects(operation)
    }

    fn validate_relationship_effects(&self, operation: &OperationId) -> Result<(), StorageError> {
        for approved in &self.proposal.approved_relationship_effects {
            let present = match approved {
                ApprovedRelationshipEffect::LinkSourceToSource { source, target } => self.change.edge_additions.iter().any(|edge| edge.from == existing(source) && edge.to == existing(target)),
                ApprovedRelationshipEffect::GeneratedEndpoint { operation: key, slot } => {
                    key == operation && self.change.quality_details.as_ref().is_some_and(|d| usize::from(*slot) < d.stable_slots.len())
                }
                ApprovedRelationshipEffect::RemoveSourceMembership { source, cluster } => self.change.facts.iter().any(|fact| matches!(fact,
                    FactChange::Update { fact, visibility: Some(visibility), .. }
                        if fact == source && (visibility.deleted || visibility.quarantined_at.is_some())
                )) && self.row(source).relation_versions.iter().any(|e| e.kind == EdgeKind::ClusterMembership && &e.from == cluster),
                // The quality transforms don't invent a destination cluster for a source.
                ApprovedRelationshipEffect::AddSourceMembership { .. } => false,
            };
            if !present {
                return Err(invalid_quality());
            }
        }
        Ok(())
    }

    fn embedding(&self, slot: usize, text: &str) -> Result<EmbeddingSnapshot, StorageError> {
        let artifact = &self.prepared[slot];
        let vector = &artifact.embedding;
        if artifact.content != text
            || vector.dimensions != vector.vector.len()
            || vector.vector.is_empty()
            || !vector.vector.iter().all(|value| value.is_finite())
        {
            return Err(invalid_quality());
        }
        Ok(vector.clone())
    }

    fn retire(&mut self, source: &FactId) {
        // Built before the push: the `&self` borrow of `row` must not cross the mutable
        // `change.facts` borrow, which the old map's disjoint-field index allowed for free.
        let retired = update(self.row(source), Some(Visibility::deleted()));
        self.change.facts.push(retired);
    }

    fn lineage(&mut self, from: RelationTarget, to: RelationTarget) {
        self.change
            .edge_additions
            .push(RelationChange::memory("derived_from", from, to, 1.0));
    }

    fn sessions(&mut self, source: &FactId, target: &RelationTarget) {
        // Read first, then take the mutable envelope borrow: `row(&self)` cannot coexist with
        // `quality_details.as_mut()`, where the old map's disjoint-field reads could.
        let session_edges: Vec<RecordId> = self
            .row(source)
            .relation_versions
            .iter()
            .filter(|edge| edge.kind == EdgeKind::SessionMembership)
            .map(|edge| edge.from.clone())
            .collect();
        let details = self
            .change
            .quality_details
            .as_mut()
            .expect("planner has a quality envelope");
        for session in session_edges {
            let existing_target = self
                .rows
                .iter()
                .find(|(id, _)| existing(id) == *target)
                .map(|(_, row)| row);
            if existing_target.is_some_and(|row| {
                row.relation_versions
                    .iter()
                    .any(|link| link.kind == EdgeKind::SessionMembership && link.from == session)
            }) || details
                .generated_sessions
                .iter()
                .any(|link| link.session == session && link.target == *target)
            {
                continue;
            }
            details.generated_sessions.push(GeneratedSession {
                session,
                target: target.clone(),
                source: source.clone(),
            });
        }
    }

    async fn clusters(
        &mut self,
        mutation: &MutationRepo<'_>,
        owner: &OwnerContext,
    ) -> Result<(), StorageError> {
        // `RecordId` has interior mutability, so it cannot key a `BTreeMap` (`mutable_key_type`);
        // the branch's established shape is a Vec of per-cluster contribution lists, sorted by
        // cluster id before the walk so cluster effects and edge removals are journalled in the
        // same deterministic order the map's key iteration gave.
        let mut edits: Vec<(RecordId, Vec<Contribution>)> = Vec::new();
        for change in &self.change.facts {
            let FactChange::Update {
                fact,
                embedding,
                visibility,
                ..
            } = change
            else {
                continue;
            };
            let retire = visibility.is_some_and(|v| v.deleted || v.quarantined_at.is_some());
            if !retire && embedding.is_none() {
                continue;
            }
            for edge in self
                .row(fact)
                .relation_versions
                .iter()
                .filter(|edge| edge.kind == EdgeKind::ClusterMembership)
            {
                let contribution = Contribution {
                    id: fact.as_record_id().clone(),
                    replacement: embedding.as_ref().map(|v| v.vector.clone()),
                };
                match edits.iter_mut().find(|(cluster, _)| *cluster == edge.from) {
                    Some((_, contributions)) => contributions.push(contribution),
                    None => edits.push((edge.from.clone(), vec![contribution])),
                }
            }
        }
        edits.sort_by(|a, b| a.0.cmp(&b.0));
        edits.dedup_by(|a, b| a.0 == b.0);
        for (cluster, changes) in edits {
            let expected = mutation.cluster_expectation(owner, &cluster).await?;
            let removed: Vec<&Contribution> =
                changes.iter().filter(|c| c.replacement.is_none()).collect();
            let mut surviving = Vec::new();
            for member in &expected.population {
                let id = FactId::try_from(member.id.clone())?;
                let snapshot = mutation
                    .state(owner, &id)
                    .await?
                    .ok_or(StorageError::Stale)?;
                if snapshot.scope != self.proposal.partition.scope {
                    return Err(StorageError::Stale);
                }
                match changes.iter().find(|change| change.id == member.id) {
                    Some(change) => surviving.extend(change.replacement.as_deref()),
                    None => surviving.push(member.embedding.as_slice()),
                }
            }
            let centroid = mean(&surviving)?;
            if !removed.is_empty() {
                push_effect(self.change, &EdgeKind::ClusterMembership.effect());
            }
            if centroid.is_some() {
                push_effect(self.change, effect::CLUSTER_CENTROID);
            }
            self.change.cluster_changes.push(ClusterChange::Effect {
                expectation: expected,
                add_member: None,
                remove_member: removed.first().map(|item| item.id.clone()),
                centroid,
            });
            for item in removed.into_iter().skip(1) {
                self.change.edge_removals.push(RelationChange::membership(
                    cluster.clone(),
                    RelationTarget::Existing(item.id.clone()),
                ));
            }
        }
        Ok(())
    }
}

struct Contribution {
    id: RecordId,
    replacement: Option<Vec<f32>>,
}

fn mean(vectors: &[&[f32]]) -> Result<Option<Vec<f32>>, StorageError> {
    let Some(first) = vectors.first() else {
        return Ok(None);
    };
    let mut sum = vec![0_f64; first.len()];
    for vector in vectors {
        if vector.len() != sum.len() || !vector.iter().all(|value| value.is_finite()) {
            return Err(invalid_quality());
        }
        for (out, input) in sum.iter_mut().zip(*vector) {
            *out += f64::from(*input);
        }
    }
    Ok(Some(
        sum.into_iter()
            .map(|value| (value / vectors.len() as f64) as f32)
            .collect(),
    ))
}

fn push_effect(change: &mut PreparedChangeSet, label: &str) {
    if !change.effects.iter().any(|effect| effect == label) {
        change.effects.push(label.into());
    }
}

fn create(
    row: &FactSnapshot,
    content: String,
    embedding: EmbeddingSnapshot,
    visibility: Visibility,
) -> FactChange {
    FactChange::Create {
        scope: row.scope.clone(),
        content,
        tags: row.tags.clone(),
        confidence: row.confidence,
        embedding,
        visibility,
        metadata: None,
    }
}

fn update(row: &FactSnapshot, visibility: Option<Visibility>) -> FactChange {
    FactChange::Update {
        fact: row.id.clone(),
        expected: row.version,
        content: None,
        tags: None,
        confidence: None,
        embedding: None,
        visibility,
        scope: None,
    }
}

fn existing(fact: &FactId) -> RelationTarget {
    RelationTarget::Existing(fact.as_record_id().clone())
}

fn descendant(
    partition: &Partition,
    operation: &OperationId,
    slot: usize,
) -> Result<FactId, StorageError> {
    let key = serde_json::to_vec(&(
        partition.workspace.as_record_id().to_sql(),
        operation.as_str(),
        slot,
    ))
    .map_err(|error| StorageError::Internal(error.into()))?;
    FactId::try_from(RecordId::new("fact", format!("split-{}", sha256(&key))))
}

// v012 carries directed evidence references, not a structured supersedes assertion. Require a
// named source in the actual replacement text, not merely a rationale or a newer timestamp.
fn explicit_supersession(
    proposal: &ProposalRevision,
    source: &FactSnapshot,
    target: &FactSnapshot,
) -> bool {
    let Some((name, _)) = source.content.split_once(':') else {
        return false;
    };
    let Some((_, claim)) = target.content.split_once(" explicitly supersedes ") else {
        return false;
    };
    let referenced = claim
        .split([';', '.', '\n', ':'])
        .next()
        .unwrap_or_default()
        .trim();
    !name.trim().is_empty()
        && referenced == name.trim()
        && proposal.approved_relationship_effects.contains(
            &ApprovedRelationshipEffect::LinkSourceToSource {
                source: source.id.clone(),
                target: target.id.clone(),
            },
        )
}

pub(crate) fn invalid_quality() -> StorageError {
    StorageError::InvalidInput("unresolved or unsupported quality effect".into())
}

#[derive(SurrealValue)]
struct DecisionRow {
    id: RecordId,
    account: RecordId,
    workspace: RecordId,
    scope: String,
    project: Option<RecordId>,
    revision: u64,
    state: ProposalState,
    proposal_payload: String,
    source_fingerprint: String,
}

async fn read_decision(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    id: &RecordId,
) -> Result<Option<DecisionRow>, StorageError> {
    let rows: Vec<DecisionRow> = tx.query("SELECT id, account, workspace, scope, project, revision, state, proposal_payload, source_fingerprint FROM $id WHERE account = $account AND workspace = $workspace;")
        .bind(("id", id.clone())).bind(("account", owner.account().as_record_id().clone())).bind(("workspace", owner.workspace().as_record_id().clone()))
        .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
    Ok(rows.into_iter().next())
}

pub(crate) async fn guard_quality_decision(
    mutation: &MutationRepo<'_>,
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    operation: &OperationId,
    expected: &[ExpectedState],
    change: &PreparedChangeSet,
    system: bool,
) -> Result<(), StorageError> {
    let transition = change
        .quality_transition
        .as_ref()
        .ok_or_else(invalid_quality)?;
    let id = transition.proposal.as_record_id();
    let details = change.quality_details.as_ref();
    let proposal = details.and_then(|d| d.proposal.as_ref());
    if select_record::<IdOnly>(tx, failed_id(owner, operation))
        .await?
        .is_some()
    {
        return Err(StorageError::Stale);
    }
    let staged = if system {
        let proposal = proposal.ok_or(StorageError::Stale)?;
        let generation = details
            .and_then(|detail| detail.generation)
            .ok_or(StorageError::Stale)?;
        guard_secondary(tx, owner, proposal, generation).await?;
        let failures: Vec<RecordId> = tx.query("SELECT VALUE id FROM refinement_attempt WHERE account = $account AND workspace = $workspace AND proposal = $proposal AND kind = 'apply' AND state = 'failed' LIMIT 1;")
            .bind(("account", owner.account().as_record_id().clone()))
            .bind(("workspace", owner.workspace().as_record_id().clone()))
            .bind(("proposal", id.clone()))
            .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
        if !failures.is_empty() {
            return Err(StorageError::Stale);
        }
        Some(proposal)
    } else {
        if details.is_some_and(|d| d.generation.is_some()) {
            return Err(StorageError::Stale);
        }
        None
    };
    let row = match read_decision(tx, owner, id).await? {
        Some(row) => row,
        None => {
            let proposal = staged.ok_or(StorageError::NotFound)?;
            stage_secondary(tx, owner, proposal).await?;
            read_decision(tx, owner, id)
                .await?
                .ok_or(StorageError::Stale)?
        }
    };
    if row.revision != transition.revision.get() || row.state != ProposalState::Pending {
        return Err(StorageError::Stale);
    }
    if let Some(proposal) = proposal {
        if !matches_decision(&row, proposal)? || expected != proposal.sources.as_slice() {
            return Err(StorageError::Stale);
        }
    } else {
        let [source] = expected else {
            return Err(invalid_quality());
        };
        let snapshot = mutation
            .state_in_tx(tx, owner, &source.fact)
            .await?
            .ok_or(StorageError::Stale)?;
        guard_kernel_rewrite(&row, source, &snapshot, change)?;
    }
    Ok(())
}

// The public kernel can also be given a fully planned rewrite. It must never authorize a
// different semantic write just because its caller possesses the proposal id and revision.
fn guard_kernel_rewrite(
    row: &DecisionRow,
    source: &ExpectedState,
    snapshot: &FactSnapshot,
    change: &PreparedChangeSet,
) -> Result<(), StorageError> {
    let value: serde_json::Value = serde_json::from_str(&row.proposal_payload)
        .map_err(|error| StorageError::Internal(error.into()))?;
    let proposed = &value["operation"];
    let [
        archive @ FactChange::Create { .. },
        FactChange::Update {
            fact,
            expected: version,
            content: Some(text),
            embedding: Some(_),
            tags: None,
            confidence: None,
            visibility: None,
            scope: None,
        },
    ] = change.facts.as_slice()
    else {
        return Err(invalid_quality());
    };
    let Some(sources) = value["sources"].as_array() else {
        return Err(invalid_quality());
    };
    let [stored] = sources.as_slice() else {
        return Err(invalid_quality());
    };
    let source_sql = fact.as_record_id().to_sql();
    let approved_edge = RelationChange::memory(
        "derived_from",
        existing(fact),
        RelationTarget::NewFact(0),
        1.0,
    );
    if proposed["kind"] != "rewrite" || proposed["source"].as_str() != Some(&source_sql)
        || proposed["content"].as_str() != Some(text) || *stored != source_value(source)
        || &source.fact != fact || source.version != *version || snapshot.visibility != Visibility::live()
        || *archive != create(snapshot, snapshot.content.clone(), snapshot.embedding.clone().ok_or_else(invalid_quality)?, Visibility::deleted())
        || change.edge_additions != [approved_edge] || !change.edge_removals.is_empty() || !change.session_links.is_empty()
        || change.cluster_changes.iter().any(|cluster| !matches!(cluster, ClusterChange::Effect { expectation, add_member: None, remove_member: None, .. } if cluster_is_in_source_graph(std::slice::from_ref(source), fact, &expectation.cluster)))
        || !change.raw_documents.is_empty() || !change.provenance_rows.is_empty() || change.maintenance_summary.is_some() || change.heat_seed.is_some()
    { return Err(invalid_quality()); }
    Ok(())
}

fn matches_decision(row: &DecisionRow, proposal: &ProposalRevision) -> Result<bool, StorageError> {
    let (scope, project) = scope_fields(&proposal.partition.scope);
    Ok(row.id == proposal.id
        && row.account == *proposal.origin_policy.account.as_record_id()
        && row.workspace == *proposal.partition.workspace.as_record_id()
        && row.scope == scope
        && row.project == project
        && row.revision == proposal.revision.get()
        && row.proposal_payload == proposal_payload(proposal)?
        && row.source_fingerprint == source_fingerprint(proposal)?)
}

pub(crate) async fn guard_secondary(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    proposal: &ProposalRevision,
    generation: u64,
) -> Result<(), StorageError> {
    if proposal.partition.workspace != *owner.workspace()
        || proposal.partition.scope == MemoryScope::Unassigned
    {
        return Err(StorageError::Stale);
    }
    let workspace = owner.workspace().as_record_id().clone();
    let rows: Vec<IdOnly> = tx
        .query("SELECT id FROM $workspace WHERE owner = $account FOR UPDATE;")
        .bind(("workspace", workspace.clone()))
        .bind(("account", owner.account().as_record_id().clone()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    if rows.len() != 1 {
        return Err(StorageError::Stale);
    }
    if let MemoryScope::Project(project) = &proposal.partition.scope {
        let rows: Vec<IdOnly> = tx
            .query("SELECT id FROM $project WHERE workspace = $workspace FOR UPDATE;")
            .bind(("project", project.as_record_id().clone()))
            .bind(("workspace", workspace))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if rows.len() != 1 {
            return Err(StorageError::Stale);
        }
    }
    let binding = &proposal.origin_policy;
    let snapshot = PolicySnapshot::read(tx, owner.account()).await?;
    let effective = snapshot.effective()?;
    if !effective.inference_admitted
        || work::bind_authority(owner.account(), &effective).as_ref() != Some(binding)
        || binding.mode != ApprovalMode::SecondaryLlm
    {
        return Err(StorageError::Stale);
    }
    pin_configuration(tx, snapshot.policy.as_ref()).await?;
    work::fence_generation(tx, generation).await
}

async fn stage_secondary(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    proposal: &ProposalRevision,
) -> Result<(), StorageError> {
    let evidence = evidence_fingerprint(&proposal.evidence)?;
    let sources = source_fingerprint(proposal)?;
    let id = RecordId::new(
        "refinement_evidence",
        sha256(
            format!(
                "{}:{}:{}",
                owner.account().as_record_id().to_sql(),
                evidence,
                sources
            )
            .as_bytes(),
        ),
    );
    if select_record::<EvidenceRecord>(tx, id.clone())
        .await?
        .is_none()
    {
        tx.query("CREATE $id CONTENT $row")
            .bind(("id", id.clone()))
            .bind(("row", evidence_row(owner, proposal)?.into_value()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
    }
    let row = proposal_record(
        owner,
        proposal,
        id,
        evidence,
        sources,
        proposal_payload(proposal)?,
    );
    tx.query("CREATE $id CONTENT $row")
        .bind(("id", proposal.id.clone()))
        .bind(("row", row.into_value()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    Ok(())
}

pub(crate) async fn link_decision(
    tx: &Transaction<Any>,
    transition: &QualityProposalTransition,
    journal: &RecordId,
) -> Result<(), StorageError> {
    let rows: Vec<IdOnly> = tx.query("UPDATE $id SET state = 'applied', operation = $operation, deferred = false, needs_human = false WHERE state = 'pending' AND revision = $revision RETURN AFTER;")
        .bind(("id", transition.proposal.as_record_id().clone())).bind(("operation", journal.clone())).bind(("revision", transition.revision.get()))
        .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
    if rows.len() == 1 {
        Ok(())
    } else {
        Err(StorageError::Stale)
    }
}

#[derive(SurrealValue)]
struct IdOnly {
    #[allow(dead_code)]
    id: RecordId,
}

fn failed_id(owner: &OwnerContext, operation: &OperationId) -> RecordId {
    let key = format!(
        "{}\0{}",
        owner.account().as_record_id().to_sql(),
        operation.as_str()
    );
    RecordId::new(
        "refinement_attempt",
        format!("apply-{}", sha256(key.as_bytes())),
    )
}

async fn record_failed_in_tx(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    proposal: &ProposalRevision,
    operation: &OperationId,
    generation: Option<u64>,
) -> Result<(), StorageError> {
    claim_owner(tx, owner).await?;
    if let Some(generation) = generation {
        guard_secondary(tx, owner, proposal, generation).await?;
        if read_decision(tx, owner, &proposal.id).await?.is_none() {
            stage_secondary(tx, owner, proposal).await?;
        }
    }
    let row = read_decision(tx, owner, &proposal.id)
        .await?
        .ok_or(StorageError::NotFound)?;
    if row.state != ProposalState::Pending || !matches_decision(&row, proposal)? {
        return Err(StorageError::Stale);
    }
    let id = failed_id(owner, operation);
    if select_record::<IdOnly>(tx, id.clone()).await?.is_some() {
        return Ok(());
    }
    let (scope, project) = scope_fields(&proposal.partition.scope);
    tx.query("CREATE $id SET account = $account, workspace = $workspace, scope = $scope, project = $project, proposal = $proposal, proposal_revision = $revision, state = 'failed', kind = 'apply', idempotency_key = $key, selection = { kind: 'scheduled', related: [] }, authority = $authority RETURN NONE;")
        .bind(("key", id.to_sql())).bind(("id", id)).bind(("account", row.account)).bind(("workspace", row.workspace)).bind(("scope", scope)).bind(("project", project))
        .bind(("proposal", row.id)).bind(("revision", row.revision)).bind(("authority", AuthorityRecord::from(&proposal.origin_policy).into_value()))
        .await.map_err(internal)?.check().map_err(internal)?;
    Ok(())
}
