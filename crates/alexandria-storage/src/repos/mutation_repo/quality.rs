//! The quality-specific envelope, final graph images and inverse of a committed quality change.

use crate::models::identity::SemanticOperationId;
use crate::models::refinement::QualityDetails;

use super::super::refinement_repo::application::invalid_quality;
use super::*;

pub(super) fn validate_envelope(change: &PreparedChangeSet) -> Result<(), StorageError> {
    let details = change.quality_details.as_ref();
    let revert = details.and_then(|d| d.reverts.as_ref());
    let valid = match change.kind {
        operation_kind::QUALITY => change.quality_transition.is_some() && revert.is_none(),
        operation_kind::REVERT => change.quality_transition.is_none() && revert.is_some(),
        _ => {
            return if change.quality_transition.is_none()
                && details.is_none()
                && change.quality_embeddings.is_empty()
            {
                Ok(())
            } else {
                Err(invalid_quality())
            };
        }
    };
    if !valid
        || !change.quality_embeddings.is_empty()
        || change.classification_approval.is_some()
        || !change.raw_documents.is_empty()
        || !change.provenance_rows.is_empty()
        || !change.session_links.is_empty()
        || change.maintenance_summary.is_some()
        || change.heat_seed.is_some()
    {
        return Err(invalid_quality());
    }
    Ok(())
}

#[derive(SurrealValue)]
pub(super) struct GraphRow {
    edge: RecordId,
    kind: String,
    source: RecordId,
    target: RecordId,
    strength: f64,
    generated_from: Option<RecordId>,
}

impl From<&EdgeSnapshot> for GraphRow {
    fn from(edge: &EdgeSnapshot) -> Self {
        Self {
            edge: edge.edge.clone(),
            kind: edge.kind.effect(),
            source: edge.from.clone(),
            target: edge.to.clone(),
            strength: edge.strength,
            generated_from: edge.generated_from.clone(),
        }
    }
}

impl GraphRow {
    pub(super) fn into_edge(self) -> Result<EdgeSnapshot, StorageError> {
        let kind = match self.edge.table.as_str() {
            "memory_edge" => EdgeKind::Memory(
                self.kind
                    .strip_prefix("memory_edge:")
                    .ok_or_else(invalid_quality)?
                    .to_owned(),
            ),
            "contains_memory" => EdgeKind::ClusterMembership,
            "contains_session_memory" => EdgeKind::SessionMembership,
            "has_provenance" => EdgeKind::Provenance,
            "extracted_from" => EdgeKind::ExtractedFrom,
            _ => return Err(invalid_quality()),
        };
        if kind.effect() != self.kind
            || (self.generated_from.is_some() && kind != EdgeKind::SessionMembership)
        {
            return Err(invalid_quality());
        }
        Ok(EdgeSnapshot {
            edge: self.edge,
            kind,
            from: self.source,
            to: self.target,
            strength: self.strength,
            generated_from: self.generated_from,
        })
    }
}

pub(super) async fn write_graph(
    tx: &Transaction<Any>,
    id: &RecordId,
    snapshot: &SemanticSnapshot,
) -> Result<(), StorageError> {
    let SemanticSnapshot::Fact(fact) = snapshot else {
        return Ok(());
    };
    let graph = fact
        .relation_versions
        .iter()
        .map(GraphRow::from)
        .collect::<Vec<_>>();
    tx.query("UPDATE $id SET quality_graph = $graph RETURN NONE;")
        .bind(("id", id.clone()))
        .bind(("graph", graph.into_value()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;
    Ok(())
}

impl MutationRepo<'_> {
    pub(super) async fn quality_state(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        fact: &FactId,
    ) -> Result<FactSnapshot, StorageError> {
        let rows: Vec<IdRow> = tx.query("SELECT id FROM $fact WHERE account = $account AND workspace = $workspace FOR UPDATE;")
            .bind(("fact", fact.as_record_id().clone())).bind(("account", owner.account().as_record_id().clone())).bind(("workspace", owner.workspace().as_record_id().clone()))
            .await.map_err(internal)?.check().map_err(internal)?.take(0).map_err(internal)?;
        if rows.len() != 1 {
            return Err(StorageError::Stale);
        }
        let mut state = self
            .state_in_tx(tx, owner, fact)
            .await?
            .ok_or(StorageError::Stale)?;
        state.relation_versions.sort_by(|a, b| a.edge.cmp(&b.edge));
        // A scoped projection must not mask a later foreign reference from rollback.
        let mut all = tx
            .query(
                "SELECT VALUE id FROM contains_memory WHERE out = $fact; \
            SELECT VALUE id FROM contains_session_memory WHERE out = $fact; \
            SELECT VALUE id FROM has_provenance WHERE in = $fact; \
            SELECT VALUE id FROM extracted_from WHERE in = $fact; \
            SELECT VALUE id FROM memory_edge WHERE in = $fact OR out = $fact;",
            )
            .bind(("fact", fact.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let mut count = 0;
        for index in 0..5 {
            count += all.take::<Vec<RecordId>>(index).map_err(internal)?.len();
        }
        if count != state.relation_versions.len() {
            return Err(StorageError::Stale);
        }
        Ok(state)
    }

    pub(super) async fn refresh_quality_images(
        &self,
        tx: &Transaction<Any>,
        owner: &OwnerContext,
        images: &mut [SemanticSnapshot],
    ) -> Result<(), StorageError> {
        for image in images {
            match image {
                SemanticSnapshot::Fact(fact) => {
                    *fact = self.quality_state(tx, owner, &fact.id).await?
                }
                SemanticSnapshot::Cluster(cluster) => {
                    *image = self.cluster_image_in_tx(tx, owner, &cluster.id).await?
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) async fn guard_quality_relations(
        &self,
        tx: &Transaction<Any>,
        change: &PreparedChangeSet,
        expected: &[ExpectedState],
    ) -> Result<(), StorageError> {
        for (addition, relation) in change
            .edge_additions
            .iter()
            .map(|edge| (true, edge))
            .chain(change.edge_removals.iter().map(|edge| (false, edge)))
        {
            let (RelationTarget::Existing(from), RelationTarget::Existing(to)) =
                (&relation.from, &relation.to)
            else {
                if addition {
                    continue;
                }
                return Err(invalid_quality());
            };
            self.guard_quality_relation(tx, expected, relation.kind.clone(), from, to, addition)
                .await?;
        }
        for cluster in &change.cluster_changes {
            if let ClusterChange::Effect {
                expectation,
                add_member,
                remove_member,
                ..
            } = cluster
            {
                if let Some(RelationTarget::Existing(member)) = add_member {
                    self.guard_quality_relation(
                        tx,
                        expected,
                        EdgeKind::ClusterMembership,
                        &expectation.cluster,
                        member,
                        true,
                    )
                    .await?;
                }
                if let Some(member) = remove_member {
                    self.guard_quality_relation(
                        tx,
                        expected,
                        EdgeKind::ClusterMembership,
                        &expectation.cluster,
                        member,
                        false,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn guard_quality_relation(
        &self,
        tx: &Transaction<Any>,
        expected: &[ExpectedState],
        kind: EdgeKind,
        from: &RecordId,
        to: &RecordId,
        addition: bool,
    ) -> Result<(), StorageError> {
        let guard = if addition {
            RelationExpectation::Absent {
                kind,
                from: from.clone(),
                to: to.clone(),
            }
        } else {
            expected.iter().flat_map(|state| &state.graph).find(|edge| matches!(edge,
                RelationExpectation::Present { kind: actual, from: source, to: target, edge: Some(_) }
                    if actual == &kind && source == from && target == to
            )).cloned().ok_or(StorageError::Stale)?
        };
        if !self.matches_relation(tx, &guard).await? {
            return Err(StorageError::Stale);
        }
        Ok(())
    }

    pub(super) async fn write_generated_sessions(
        &self,
        tx: &Transaction<Any>,
        change: &PreparedChangeSet,
        minted: &[RecordId],
    ) -> Result<(), StorageError> {
        let Some(details) = &change.quality_details else {
            return Ok(());
        };
        for link in &details.generated_sessions {
            let target = resolve(&link.target, minted, &[], &[])?;
            let source = link.source.as_record_id();
            if !self
                .matches_relation(
                    tx,
                    &RelationExpectation::Present {
                        kind: EdgeKind::SessionMembership,
                        from: link.session.clone(),
                        to: source.clone(),
                        edge: None,
                    },
                )
                .await?
                || !self
                    .matches_relation(
                        tx,
                        &RelationExpectation::Absent {
                            kind: EdgeKind::SessionMembership,
                            from: link.session.clone(),
                            to: target.clone(),
                        },
                    )
                    .await?
            {
                return Err(StorageError::Stale);
            }
            tx.query("RELATE $sess->contains_session_memory->$fact SET generated_from = $source RETURN NONE;")
                .bind(("sess", link.session.clone())).bind(("fact", target)).bind(("source", source.clone()))
                .await.map_err(internal)?.check().map_err(internal)?;
        }
        Ok(())
    }

    pub(crate) async fn revert_quality(
        &self,
        owner: &OwnerContext,
        applied: &SemanticOperationId,
        operation: &OperationId,
    ) -> Result<AppliedChange, StorageError> {
        let db = self.db.clone();
        let access = MutationContext::issued_to(owner);
        let applied = applied.clone();
        let operation = operation.clone();
        revert_quality(db, access, applied, operation).await
    }
}

async fn revert_quality(
    db: Surreal<Any>,
    access: MutationContext,
    applied: SemanticOperationId,
    operation: OperationId,
) -> Result<AppliedChange, StorageError> {
    let mutation = MutationRepo::new(&db);
    // A rollback never rebases its postimage; retries only rediscover a committed duplicate or
    // repeat the exact guard in a new transaction if the engine lost a lock race.
    for _ in 0..crate::repos::MAX_PREPARATIONS {
        let tx = db.clone().begin().await.map_err(internal)?;
        let result = revert_in_tx(&mutation, &tx, &access, &applied, &operation).await;
        let outcome = match result {
            Ok(applied) => MutationRepo::commit(tx).await.map(|()| applied),
            Err(error) => {
                tx.cancel().await.map_err(internal)?;
                Err(error)
            }
        };
        match outcome {
            Err(StorageError::RetryableBookkeepingConflict) => tokio::task::yield_now().await,
            _ => return outcome,
        }
    }
    Err(StorageError::Stale)
}

async fn revert_in_tx(
    mutation: &MutationRepo<'_>,
    tx: &Transaction<Any>,
    access: &MutationContext,
    applied: &SemanticOperationId,
    operation: &OperationId,
) -> Result<AppliedChange, StorageError> {
    let now = Utc::now();
    let owner = access.owner();
    if !claim_authority(tx, owner, now).await? {
        return Err(StorageError::Stale);
    }
    let workspace = owner.workspace().as_record_id();
    let mut change = PreparedChangeSet::new(operation_kind::REVERT).effect("quality:revert");
    change.quality_details = Some(QualityDetails {
        reverts: Some(applied.clone()),
        ..QualityDetails::default()
    });
    let fingerprint = change.request_fingerprint();
    let replay: Vec<JournalRow> = tx
        .query(OPERATION_SQL)
        .bind(("workspace", workspace.clone()))
        .bind(("key", operation.as_str().to_owned()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    if let Some(replay) = replay.into_iter().next() {
        return if replay.request_fingerprint == fingerprint {
            Ok(replay.into_replay())
        } else {
            Err(invalid_quality())
        };
    }
    let original: Vec<JournalRow> = tx
        .query(OPERATION_BY_ID_SQL)
        .bind(("id", applied.as_record_id().clone()))
        .bind(("key", record_key(applied.as_record_id())?))
        .bind(("workspace", workspace.clone()))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?
        .take(0)
        .map_err(internal)?;
    let original = original.into_iter().next().ok_or(StorageError::NotFound)?;
    if original.kind != operation_kind::QUALITY
        || original.actor.as_ref() != Some(owner.account().as_record_id())
        || original.committed_at.is_none()
        || original.reverted_by.is_some()
        || original.postimages.is_empty()
    {
        return Err(StorageError::Stale);
    }
    let pre = read_images(tx, workspace, &original.id, &original.preimages).await?;
    let post = read_images(tx, workspace, &original.id, &original.postimages).await?;
    let expected = inverse(mutation, tx, owner, &pre, &post, &mut change).await?;
    let request = PassRequest {
        access,
        operation,
        fingerprint: &fingerprint,
        expected: &expected,
        change: &change,
        now,
    };
    mutation.run(tx, &request).await
}

async fn read_images(
    tx: &Transaction<Any>,
    workspace: &RecordId,
    operation: &RecordId,
    ids: &[RecordId],
) -> Result<Vec<SemanticSnapshot>, StorageError> {
    let mut images = Vec::with_capacity(ids.len());
    for id in ids {
        if id.table.as_str() != "semantic_snapshot" {
            return Err(StorageError::Stale);
        }
        let rows: Vec<SnapshotRow> = tx
            .query("SELECT * FROM $id WHERE workspace = $workspace AND operation = $operation;")
            .bind(("id", id.clone()))
            .bind(("workspace", workspace.clone()))
            .bind(("operation", operation.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        let row = rows.into_iter().next().ok_or(StorageError::Stale)?;
        if row.kind.as_deref() == Some(snapshot_kind::FACT) && row.quality_graph.is_none() {
            return Err(StorageError::Stale);
        }
        images.push(row.into_snapshot(workspace)?);
    }
    Ok(images)
}

async fn inverse(
    mutation: &MutationRepo<'_>,
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    pre: &[SemanticSnapshot],
    post: &[SemanticSnapshot],
    change: &mut PreparedChangeSet,
) -> Result<Vec<ExpectedState>, StorageError> {
    let mut expected = Vec::new();
    // `FactId` has interior mutability, so it cannot key a `BTreeMap` (`mutable_key_type`); the
    // branch's established shape is a sorted, deduplicated Vec plus explicit lookups. The sort
    // reproduces the key order the map iterated in, which the journal fingerprint depends on.
    let mut before_facts: Vec<(&FactId, &FactSnapshot)> = pre
        .iter()
        .filter_map(SemanticSnapshot::fact)
        .map(|fact| (&fact.id, fact))
        .collect();
    before_facts.sort_by(|a, b| a.0.cmp(b.0));
    before_facts.dedup_by(|a, b| a.0 == b.0);
    for snapshot in post {
        match snapshot {
            SemanticSnapshot::Fact(after) => {
                let current = mutation.quality_state(tx, owner, &after.id).await?;
                let mut after = after.clone();
                after.relation_versions.sort_by(|a, b| a.edge.cmp(&b.edge));
                if current != after {
                    return Err(StorageError::Stale);
                }
                expected.push(ExpectedState::of(&current));
                let prior = fact_snapshot(&before_facts, &after.id);
                if let Some(prior) = prior {
                    if prior.scope != current.scope {
                        return Err(StorageError::Stale);
                    }
                    if prior == &current {
                        continue;
                    }
                }
                change.facts.push(FactChange::Update {
                    fact: after.id,
                    expected: current.version,
                    scope: None,
                    content: prior.map(|fact| fact.content.clone()),
                    tags: prior.map(|fact| fact.tags.clone()),
                    confidence: prior.map(|fact| fact.confidence),
                    embedding: prior.and_then(|fact| fact.embedding.clone()),
                    visibility: Some(
                        prior.map_or_else(Visibility::deleted, |fact| fact.visibility),
                    ),
                });
            }
            SemanticSnapshot::Cluster(after) => {
                if mutation.cluster_image_in_tx(tx, owner, &after.id).await? != *snapshot {
                    return Err(StorageError::Stale);
                }
                let before = pre
                    .iter()
                    .find_map(|snapshot| match snapshot {
                        SemanticSnapshot::Cluster(before) if before.id == after.id => Some(before),
                        _ => None,
                    })
                    .ok_or(StorageError::Stale)?;
                inverse_cluster(mutation, tx, owner, &before_facts, before, after, change).await?;
            }
            SemanticSnapshot::Edge(_) => {} // The complete endpoint images guard these IDs too.
            SemanticSnapshot::Reminder(_) => return Err(StorageError::Stale),
        }
    }
    inverse_relations(pre, post, change)?;
    Ok(expected)
}

/// Linear lookup on a sorted, deduplicated preimage list — the `BTreeMap::get` it replaces.
fn fact_snapshot<'a>(
    rows: &[(&'a FactId, &'a FactSnapshot)],
    id: &FactId,
) -> Option<&'a FactSnapshot> {
    rows.iter().find(|(key, _)| *key == id).map(|(_, row)| *row)
}

async fn inverse_cluster(
    mutation: &MutationRepo<'_>,
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    sources: &[(&FactId, &FactSnapshot)],
    before: &ClusterSnapshot,
    after: &ClusterSnapshot,
    change: &mut PreparedChangeSet,
) -> Result<(), StorageError> {
    let add: Vec<_> = before
        .members
        .iter()
        .filter(|id| !after.members.contains(id))
        .cloned()
        .collect();
    let remove: Vec<_> = after
        .members
        .iter()
        .filter(|id| !before.members.contains(id))
        .cloned()
        .collect();
    let mut scope = None;
    for id in before.members.iter().chain(&after.members) {
        let fact = FactId::try_from(id.clone())?;
        let current = mutation
            .state_in_tx(tx, owner, &fact)
            .await?
            .ok_or(StorageError::Stale)?;
        let intended = fact_snapshot(sources, &fact).map_or(&current.scope, |source| &source.scope);
        if scope.as_ref().is_some_and(|seen| seen != intended) {
            return Err(StorageError::Stale);
        }
        scope = Some(intended.clone());
    }
    let expectation = ClusterWriteExpectation {
        cluster: after.id.clone(),
        membership_revision: after.membership_revision,
        member_embedding_revision: after.member_embedding_revision,
        centroid_revision: after.centroid_revision,
        population: mutation
            .locked_population(tx, owner, &after.members)
            .await?,
    };
    change.cluster_changes.push(ClusterChange::Effect {
        expectation,
        add_member: add.first().cloned().map(RelationTarget::Existing),
        remove_member: remove.first().cloned(),
        centroid: Some(before.centroid.clone()),
    });
    for member in add.into_iter().skip(1) {
        change.edge_additions.push(RelationChange::membership(
            after.id.clone(),
            RelationTarget::Existing(member),
        ));
    }
    for member in remove.into_iter().skip(1) {
        change.edge_removals.push(RelationChange::membership(
            after.id.clone(),
            RelationTarget::Existing(member),
        ));
    }
    Ok(())
}

fn inverse_relations(
    pre: &[SemanticSnapshot],
    post: &[SemanticSnapshot],
    change: &mut PreparedChangeSet,
) -> Result<(), StorageError> {
    fn edges(images: &[SemanticSnapshot]) -> Vec<(&RecordId, &EdgeSnapshot)> {
        let mut edges: Vec<(&RecordId, &EdgeSnapshot)> = images
            .iter()
            .filter_map(SemanticSnapshot::fact)
            .flat_map(|fact| &fact.relation_versions)
            .filter(|edge| edge.kind != EdgeKind::ClusterMembership)
            .map(|edge| (&edge.edge, edge))
            .collect();
        // Same pattern as `before_facts`: `RecordId` has interior mutability, and the sort keeps
        // the map's key order so removals are journalled deterministically.
        edges.sort_by(|a, b| a.0.cmp(b.0));
        edges.dedup_by(|a, b| a.0 == b.0);
        edges
    }
    let pre = edges(pre);
    let post = edges(post);
    for (id, edge) in post.iter().copied() {
        match edge_of(&pre, id) {
            None => {
                change.edge_removals.push(RelationChange {
                    kind: edge.kind.clone(),
                    from: RelationTarget::Existing(edge.from.clone()),
                    to: RelationTarget::Existing(edge.to.clone()),
                    strength: edge.strength,
                });
            }
            Some(prior) if prior != edge => return Err(StorageError::Stale),
            Some(_) => {}
        }
    }
    // The forward quality transforms never remove non-membership historical graph edges.
    if pre.iter().any(|(id, _)| edge_of(&post, id).is_none()) {
        return Err(StorageError::Stale);
    }
    Ok(())
}

/// Linear lookup on a sorted, deduplicated edge list — the `BTreeMap::get` it replaces.
fn edge_of<'a>(
    edges: &[(&'a RecordId, &'a EdgeSnapshot)],
    id: &RecordId,
) -> Option<&'a EdgeSnapshot> {
    edges
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, edge)| *edge)
}

pub(super) async fn link_reversal(
    tx: &Transaction<Any>,
    owner: &OwnerContext,
    original: &SemanticOperationId,
    journal: &RecordId,
) -> Result<(), StorageError> {
    let mut response = tx.query("UPDATE $original SET reverted_by = $journal WHERE workspace = $workspace AND reverted_by = NONE RETURN AFTER; \
        UPDATE $journal SET reverts = $original RETURN AFTER; \
        UPDATE refinement_proposal SET state = 'reverted' WHERE workspace = $workspace AND account = $account AND operation = $original AND state = 'applied' RETURN AFTER;")
        .bind(("original", original.as_record_id().clone())).bind(("journal", journal.clone()))
        .bind(("workspace", owner.workspace().as_record_id().clone())).bind(("account", owner.account().as_record_id().clone()))
        .await.map_err(internal)?.check().map_err(internal)?;
    for index in 0..3 {
        let rows: Vec<IdRow> = response.take(index).map_err(internal)?;
        if rows.len() != 1 {
            return Err(StorageError::Stale);
        }
    }
    Ok(())
}
