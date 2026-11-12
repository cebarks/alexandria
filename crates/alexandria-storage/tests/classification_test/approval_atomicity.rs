use super::*;
use alexandria_storage::models::mutation::DestinationMembership;
use alexandria_storage::repos::ClassificationDestination;

pub(super) async fn seed_scope_vector(
    f: &Fixture,
    content: &str,
    vector: &[f32],
    target: WriteTarget,
) -> FactId {
    let read = if matches!(&target, WriteTarget::Unassigned) {
        f.unassigned().await
    } else {
        f.read().await
    };
    let row = ScopedMemoryRepo::new(f.db.inner(), &f.owner, &read)
        .create_fact(content, 0.5, vector, &[], target, Utc::now())
        .await
        .unwrap();
    FactId::try_from(row.id.unwrap()).unwrap()
}

async fn ungrouped_batch(
    repo: &ClassificationRepo<'_>,
    refs: &[alexandria_storage::repos::ScopeProposalRef],
) -> alexandria_storage::repos::PreparedClassificationBatch {
    let captured = repo.prepare_batch(refs).await.unwrap();
    let choices: Vec<_> = captured
        .candidates()
        .iter()
        .map(|candidate| ClassificationDestination::Chosen {
            reference: candidate.reference().clone(),
            arriving: DestinationMembership::Unclustered,
        })
        .collect();
    captured.seal(&choices).unwrap()
}

#[tokio::test]
async fn classification_review_sealed_reexecution_can_refuse_before_kernel_replay() {
    let f = Fixture::enrolled("sealed-reexecution").await;
    let fact = seed_scope_vector(
        &f,
        "sealed move subject",
        &[1.0, 0.0],
        WriteTarget::Unassigned,
    )
    .await;
    let witness = seed_scope_vector(
        &f,
        "sealed destination witness",
        &[1.0, 0.0],
        WriteTarget::Personal,
    )
    .await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    f.attach(&cluster, &witness).await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let captured = repo
        .prepare_batch(std::slice::from_ref(&reference))
        .await
        .unwrap();
    let mutation = MutationRepo::new(f.db.inner());
    let sealed = captured
        .seal(&[ClassificationDestination::Chosen {
            reference: reference.clone(),
            arriving: DestinationMembership::Join {
                expectation: mutation
                    .cluster_expectation(&f.owner, &cluster)
                    .await
                    .unwrap(),
                centroid: vec![1.0, 0.0],
            },
        }])
        .unwrap();
    let operation = OperationId::new("sealed-once");
    let first = repo
        .apply_prepared_batch(&sealed, &operation)
        .await
        .unwrap();
    assert_eq!(first.applied.len(), 1);
    assert!(first.conflicts.is_empty() && first.stopped.is_none());
    assert_eq!(
        mutation
            .state(&f.owner, &fact)
            .await
            .unwrap()
            .unwrap()
            .scope,
        MemoryScope::Personal
    );
    let key = format!("sealed-once:{}", reference.id);
    assert!(mutation.operation(&f.owner, &key).await.unwrap().is_some());

    // Synthetic legacy drift only: the sealed intent still names this destination, but preparation
    // can no longer sample it. No recovery API is implied by the prepared kernel's idempotency.
    f.db.inner()
        .query("DELETE $cluster;")
        .bind(("cluster", cluster))
        .await
        .unwrap()
        .check()
        .unwrap();
    let before = f.semantic_state().await;
    let second = repo
        .apply_prepared_batch(&sealed, &operation)
        .await
        .unwrap();
    assert!(second.applied.is_empty() && second.stopped.is_none());
    assert_eq!(second.conflicts.len(), 1);
    assert!(matches!(
        &second.conflicts[0].reason,
        alexandria_storage::models::mutation::BatchConflictReason::NotFound
    ));
    assert_eq!(
        f.semantic_state().await,
        before,
        "preparation refusal cannot perform any second write"
    );
    assert!(
        mutation.operation(&f.owner, &key).await.unwrap().is_some(),
        "the original receipt remains owner-readable"
    );
}

#[tokio::test]
async fn classification_review_same_cluster_approval_is_scope_only() {
    use alexandria_storage::models::cluster::mean_centroid;
    let f = Fixture::enrolled("review-same-cluster").await;
    let fact = seed_scope_vector(&f, "already clustered", &[1.0, 0.0], WriteTarget::Personal).await;
    let witness = seed_scope_vector(
        &f,
        "same-cluster witness",
        &[0.0, 1.0],
        WriteTarget::Personal,
    )
    .await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(
            None,
            &mean_centroid(&[&[1.0, 0.0], &[0.0, 1.0]]).unwrap(),
            Utc::now(),
        )
        .await
        .unwrap()
        .id
        .unwrap();
    f.attach(&cluster, &fact).await;
    f.attach(&cluster, &witness).await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let captured = repo
        .prepare_batch(std::slice::from_ref(&reference))
        .await
        .unwrap();
    let expectation = MutationRepo::new(f.db.inner())
        .cluster_expectation(&f.owner, &cluster)
        .await
        .unwrap();
    let sealed = captured
        .seal(&[ClassificationDestination::Chosen {
            reference: reference.clone(),
            arriving: DestinationMembership::Join {
                expectation,
                centroid: mean_centroid(&[&[1.0, 0.0], &[0.0, 1.0]]).unwrap(),
            },
        }])
        .unwrap();
    let mut before =
        f.db.inner()
            .query("SELECT * FROM $cluster; SELECT * FROM contains_memory ORDER BY id;")
            .bind(("cluster", cluster.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    let cluster_before: Vec<surrealdb::types::Value> = before.take(0).unwrap();
    let memberships_before: Vec<surrealdb::types::Value> = before.take(1).unwrap();
    let operation = op("same-cluster");
    let result = repo
        .apply_prepared_batch(&sealed, &operation)
        .await
        .unwrap();
    assert_eq!(result.applied_count(), 1);
    assert!(result.is_complete());
    assert_eq!(
        f.proposal_row(&reference.id).await.unwrap().state,
        "approved"
    );
    let mut after =
        f.db.inner()
            .query("SELECT * FROM $cluster; SELECT * FROM contains_memory ORDER BY id;")
            .bind(("cluster", cluster))
            .await
            .unwrap()
            .check()
            .unwrap();
    let cluster_after: Vec<surrealdb::types::Value> = after.take(0).unwrap();
    let memberships_after: Vec<surrealdb::types::Value> = after.take(1).unwrap();
    assert_eq!(cluster_after, cluster_before);
    assert_eq!(memberships_after, memberships_before);
    let mut response =
        f.db.inner()
            .query("SELECT expected_effects FROM semantic_operation WHERE operation_key = $key;")
            .bind(("key", format!("{}:{}", operation.as_str(), reference.id)))
            .await
            .unwrap()
            .check()
            .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["expected_effects"],
        serde_json::json!([effect::SCOPE])
    );
}

#[tokio::test]
async fn classification_review_second_opinion_survives_first_refusal() {
    use alexandria_storage::models::mutation::BatchConflictReason;
    for mode in ["dismissed", "revised", "ranking"] {
        let f = Fixture::enrolled(&format!("review-opinions-{mode}")).await;
        let fact = seed_scope_vector(
            &f,
            "two distinct opinions",
            &[1.0, 0.0],
            WriteTarget::Unassigned,
        )
        .await;
        let project = f.project("second-opinion-project").await;
        let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
        let first = repo
            .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
            .await
            .unwrap();
        let second = repo
            .propose(
                &fact,
                Version::ZERO,
                MemoryScope::Project(project.clone()),
                None,
            )
            .await
            .unwrap();
        let captured = repo
            .prepare_batch(&[first.clone(), second.clone()])
            .await
            .unwrap();
        let choices: Vec<_> = captured
            .candidates()
            .iter()
            .map(|candidate| {
                if mode == "ranking" && candidate.reference() == &first {
                    ClassificationDestination::Refused {
                        reference: candidate.reference().clone(),
                        reason: BatchConflictReason::InvalidDestination,
                    }
                } else {
                    ClassificationDestination::Chosen {
                        reference: candidate.reference().clone(),
                        arriving: DestinationMembership::Unclustered,
                    }
                }
            })
            .collect();
        let sealed = captured.seal(&choices).unwrap();
        match mode {
            "dismissed" => repo.dismiss(&first).await.unwrap(),
            "revised" => {
                repo.revise_destination(first.clone(), MemoryScope::Unassigned)
                    .await
                    .unwrap();
            }
            _ => (),
        }
        let before = journal_rows(&f.db).await;
        let result = repo
            .apply_prepared_batch(&sealed, &op("independent-opinions"))
            .await
            .unwrap();
        assert_eq!(result.applied_count(), 1, "{mode}: {result:?}");
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.stopped.is_none());
        assert_eq!(journal_rows(&f.db).await, before + 1);
        assert_eq!(f.proposal_row(&second.id).await.unwrap().state, "approved");
        let original = f.proposal_row(&first.id).await.unwrap();
        assert_eq!(
            original.state,
            if mode == "dismissed" {
                "dismissed"
            } else {
                "pending"
            }
        );
        if mode == "revised" {
            assert_eq!(original.revision, 1);
            assert_eq!(original.destination_kind, "unassigned");
        }
        assert_eq!(
            MutationRepo::new(f.db.inner())
                .state(&f.owner, &fact)
                .await
                .unwrap()
                .unwrap()
                .scope,
            MemoryScope::Project(project)
        );
    }
}

#[tokio::test]
async fn classification_repair_numeric_proposal_is_not_the_textual_key() {
    use alexandria_storage::access::MutationContext;
    use alexandria_storage::models::mutation::{ClassificationApproval, ExpectedState};
    let f = Fixture::enrolled("repair-numeric-clause").await;
    let fact = seed_scope_vector(
        &f,
        "numeric clause subject",
        &[1.0, 0.0],
        WriteTarget::Unassigned,
    )
    .await;
    for id in [
        RecordId::new("classification_proposal", 17_i64),
        RecordId::new("classification_proposal", "17"),
    ] {
        f.db.inner()
            .query(
                "CREATE $id SET account = $account, workspace = $workspace, fact = $fact, \
            source_revision = 0, revision = 0, destination_kind = 'personal', state = 'pending';",
            )
            .bind(("id", id))
            .bind(("account", f.owner.account().as_record_id().clone()))
            .bind(("workspace", f.owner.workspace().as_record_id().clone()))
            .bind(("fact", fact.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
    let mutation = MutationRepo::new(f.db.inner());
    let original = mutation.state(&f.owner, &fact).await.unwrap().unwrap();
    let change = PreparedChangeSet::new(operation_kind::MOVE)
        .effect(effect::SCOPE)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: original.version,
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: None,
            scope: Some(MemoryScope::Personal),
        })
        .approve_classification(ClassificationApproval {
            proposal: RecordId::new("classification_proposal", 17_i64),
            proposal_revision: Version::ZERO,
            fact: fact.clone(),
            source_revision: original.version,
            destination: MemoryScope::Personal,
        });
    let before = f.semantic_state().await;
    for case in 0..5 {
        let mut invalid = change.clone();
        let clause = invalid.classification_approval.as_mut().unwrap();
        match case {
            0 => clause.fact = FactId::try_from(RecordId::new("fact", "another-subject")).unwrap(),
            1 => clause.source_revision = Version::new(1),
            2 => clause.destination = MemoryScope::Unassigned,
            3 => clause.proposal = fact.as_record_id().clone(),
            _ => clause.proposal_revision = Version::new(u64::MAX),
        }
        assert!(matches!(
            mutation
                .apply(
                    &MutationContext::issued_to(&f.owner),
                    &op(&format!("invalid-clause-{case}")),
                    &[ExpectedState::of(&original)],
                    &invalid
                )
                .await,
            Err(StorageError::InvalidInput(_))
        ));
        assert_eq!(f.semantic_state().await, before, "clause case {case}");
    }
    mutation
        .apply(
            &MutationContext::issued_to(&f.owner),
            &op("numeric-clause"),
            &[ExpectedState::of(&original)],
            &change,
        )
        .await
        .unwrap();
    let mut response =
        f.db.inner()
            .query("SELECT state FROM $numeric; SELECT state FROM $textual;")
            .bind(("numeric", RecordId::new("classification_proposal", 17_i64)))
            .bind(("textual", RecordId::new("classification_proposal", "17")))
            .await
            .unwrap()
            .check()
            .unwrap();
    let numeric: Vec<serde_json::Value> = response.take(0).unwrap();
    let textual: Vec<serde_json::Value> = response.take(1).unwrap();
    assert_eq!(numeric[0]["state"], "approved");
    assert_eq!(textual[0]["state"], "pending");
    let committed = f.semantic_state().await;
    let replay = mutation
        .apply(
            &MutationContext::issued_to(&f.owner),
            &op("numeric-clause"),
            &[ExpectedState::of(&original)],
            &change,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    let mut changed_citation = change.clone();
    changed_citation
        .classification_approval
        .as_mut()
        .unwrap()
        .proposal_revision = Version::new(1);
    assert!(matches!(
        mutation
            .apply(
                &MutationContext::issued_to(&f.owner),
                &op("numeric-clause"),
                &[ExpectedState::of(&original)],
                &changed_citation
            )
            .await,
        Err(StorageError::InvalidInput(_))
    ));
    assert_eq!(f.semantic_state().await, committed);
}

#[tokio::test]
async fn classification_repair_selection_guards_do_not_mutate() {
    use alexandria_storage::models::mutation::BatchConflictReason;
    use alexandria_storage::repos::ScopeProposalRef;
    let f = Fixture::enrolled("repair-selection-guards").await;
    let fact = seed_scope_vector(
        &f,
        "selection subject",
        &[1.0, 0.0],
        WriteTarget::Unassigned,
    )
    .await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let before = f.semantic_state().await;
    assert!(matches!(
        repo.prepare_batch(&[reference.clone(), reference.clone()])
            .await,
        Err(StorageError::InvalidInput(_))
    ));
    let overflow = ScopeProposalRef {
        revision: Version::new(u64::MAX),
        ..reference.clone()
    };
    assert!(matches!(
        repo.prepare_batch(std::slice::from_ref(&overflow)).await,
        Err(StorageError::InvalidInput(_))
    ));
    assert!(matches!(
        repo.dismiss(&overflow).await,
        Err(StorageError::InvalidInput(_))
    ));
    let wrong_table = ScopeProposalRef {
        id: alexandria_storage::record_id_to_string(fact.as_record_id()),
        revision: Version::ZERO,
    };
    let wrong = repo
        .apply_batch(&[wrong_table], &op("wrong-table"))
        .await
        .unwrap();
    assert_eq!(wrong.conflicts.len(), 1);
    assert_eq!(wrong.conflicts[0].reason, BatchConflictReason::NotFound);
    assert!(wrong.applied.is_empty() && wrong.stopped.is_none());
    assert_eq!(f.semantic_state().await, before);
    let second = repo
        .propose(&fact, Version::ZERO, MemoryScope::Unassigned, None)
        .await
        .unwrap();
    let result = repo
        .apply_batch(&[reference, second.clone()], &op("same-fact-opinions"))
        .await
        .unwrap();
    assert_eq!(result.applied_count(), 1);
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(f.proposal_row(&second.id).await.unwrap().state, "stale");
}

#[tokio::test]
async fn classification_repair_suspension_after_capture_cannot_decide() {
    let f = Fixture::enrolled("repair-suspended-owner").await;
    let fact = seed_scope_vector(
        &f,
        "suspended arrival",
        &[1.0, 0.0],
        WriteTarget::Unassigned,
    )
    .await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let batch = ungrouped_batch(&repo, &[reference]).await;
    f.db.inner()
        .query("UPDATE $account SET status = 'suspended';")
        .bind(("account", f.owner.account().as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let before = f.semantic_state().await;
    let refused = repo
        .apply_prepared_batch(&batch, &op("suspended"))
        .await
        .unwrap();
    assert!(refused.applied.is_empty() && refused.stopped.is_none());
    assert_eq!(refused.conflicts.len(), 1);
    assert_eq!(f.semantic_state().await, before);
    f.db.inner()
        .query("UPDATE $account SET status = 'active';")
        .bind(("account", f.owner.account().as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        repo.apply_prepared_batch(&batch, &op("reactivated"))
            .await
            .unwrap()
            .applied_count(),
        1
    );
}

#[tokio::test]
async fn classification_repair_driver_apply_side_nonretryables_return_once() {
    use alexandria_storage::access::MutationContext;
    use alexandria_storage::models::mutation::{ClassificationApproval, ExpectedState};
    use alexandria_storage::repos::commit_prepared;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for mode in ["positive", "stale", "missing", "engine"] {
        let f = Fixture::enrolled(&format!("apply-error-{mode}")).await;
        let fact = seed_scope_vector(&f, mode, &[1.0, 0.0], WriteTarget::Unassigned).await;
        let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
        let reference = repo
            .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
            .await
            .unwrap();
        let mutation = MutationRepo::new(f.db.inner());
        let original = mutation.state(&f.owner, &fact).await.unwrap().unwrap();
        let expected = [ExpectedState::of(&original)];
        let change = PreparedChangeSet::new(operation_kind::MOVE)
            .effect(effect::SCOPE)
            .fact(FactChange::Update {
                fact: fact.clone(),
                expected: original.version,
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: Some(MemoryScope::Personal),
            })
            .approve_classification(ClassificationApproval {
                proposal: RecordId::parse_simple(&reference.id).unwrap(),
                proposal_revision: reference.revision,
                fact: fact.clone(),
                source_revision: original.version,
                destination: MemoryScope::Personal,
            });
        match mode {
            "stale" => {
                f.db.inner()
                    .query("UPDATE $fact SET semantic_revision = 1;")
                    .bind(("fact", fact.as_record_id().clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
            "missing" => {
                f.db.inner()
                    .query("DELETE $fact;")
                    .bind(("fact", fact.as_record_id().clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
            "engine" => {
                f.db.inner()
                    .query(
                        "DEFINE EVENT OVERWRITE refuse_apply_driver ON TABLE semantic_operation \
                WHEN $event = 'CREATE' THEN { THROW 'fixture transaction conflict lookalike'; };",
                    )
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
            _ => {}
        }
        let before = f.semantic_state().await;
        let attempts = AtomicUsize::new(0);
        let result = commit_prepared(
            &mutation,
            &MutationContext::issued_to(&f.owner),
            &op("apply-error"),
            &expected,
            &change.effects,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(change.clone()))
            },
        )
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "{mode}");
        match mode {
            "positive" => {
                result.unwrap();
                assert_eq!(
                    f.proposal_row(&reference.id).await.unwrap().state,
                    "approved"
                );
            }
            "stale" => assert!(matches!(result, Err(StorageError::Stale)), "{result:?}"),
            "missing" => assert!(matches!(result, Err(StorageError::NotFound)), "{result:?}"),
            "engine" => assert!(
                matches!(result, Err(StorageError::Internal(_))),
                "{result:?}"
            ),
            _ => unreachable!(),
        }
        if mode != "positive" {
            assert_eq!(f.semantic_state().await, before, "{mode}");
        }
    }
}

#[tokio::test]
async fn classification_repair_preflight_stop_accounts_for_every_selection() {
    let f = Fixture::enrolled("repair-preflight-stop").await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let mut refs = Vec::new();
    let mut facts = Vec::new();
    for content in ["preflight first", "preflight second", "preflight third"] {
        let fact = seed_scope_vector(&f, content, &[1.0, 0.0], WriteTarget::Unassigned).await;
        refs.push(
            repo.propose(&fact, Version::ZERO, MemoryScope::Personal, None)
                .await
                .unwrap(),
        );
        facts.push(fact);
    }
    f.db.inner()
        .query("UPDATE $fact SET semantic_revision = 1;")
        .bind(("fact", facts[1].as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    f.db.inner().query("DEFINE EVENT OVERWRITE refuse_preflight_retirement ON TABLE classification_proposal \
        WHEN $event = 'UPDATE' AND $after.state = 'stale' THEN { THROW 'fixture refuses preflight retirement'; };")
        .await.unwrap().check().unwrap();
    let before = f.semantic_state().await;
    let captured = repo.prepare_batch(&refs).await.unwrap();
    assert!(captured.candidates().is_empty());
    let batch = captured.seal(&[]).unwrap();
    let result = repo
        .apply_prepared_batch(&batch, &op("preflight-stop"))
        .await
        .unwrap();
    assert!(result.applied.is_empty() && result.conflicts.is_empty());
    let stopped = result.stopped.unwrap();
    assert_eq!(stopped.proposal, refs[1].id);
    assert_eq!(
        stopped.unattempted,
        vec![refs[0].id.clone(), refs[2].id.clone()]
    );
    assert_eq!(f.semantic_state().await, before);
    f.db.inner()
        .query(
            "REMOVE EVENT IF EXISTS refuse_preflight_retirement ON TABLE classification_proposal;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let positive = repo
        .apply_batch(&refs, &op("preflight-positive"))
        .await
        .unwrap();
    assert_eq!(positive.applied_count(), 2);
    assert_eq!(positive.conflicts.len(), 1);
}

#[tokio::test]
async fn classification_repair_unexpected_error_keeps_partial_and_stops() {
    use alexandria_storage::repos::ScopeProposalRef;
    let f = Fixture::enrolled("repair-partial-stop").await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let mut refs = Vec::new();
    for (index, content) in ["partial first", "partial second", "partial third"]
        .into_iter()
        .enumerate()
    {
        let fact = seed_scope_vector(&f, content, &[1.0, 0.0], WriteTarget::Unassigned).await;
        if index == 1 {
            let id = RecordId::new("classification_proposal", "repair_second");
            f.db.inner().query("CREATE $id SET workspace = $workspace, account = $account, fact = $fact, \
                source_revision = 0, revision = 0, destination_kind = 'personal', state = 'pending';")
                .bind(("id", id)).bind(("workspace", f.owner.workspace().as_record_id().clone()))
                .bind(("account", f.owner.account().as_record_id().clone()))
                .bind(("fact", fact.as_record_id().clone())).await.unwrap().check().unwrap();
            refs.push(ScopeProposalRef {
                id: "classification_proposal:repair_second".into(),
                revision: Version::ZERO,
            });
        } else {
            refs.push(
                repo.propose(&fact, Version::ZERO, MemoryScope::Personal, None)
                    .await
                    .unwrap(),
            );
        }
    }
    let batch = ungrouped_batch(&repo, &refs).await;
    f.db.inner().query("DEFINE EVENT OVERWRITE refuse_repair_second ON TABLE semantic_operation \
        WHEN $event = 'CREATE' THEN { IF string::ends_with($after.operation_key, ':classification_proposal:repair_second') \
        { THROW 'fixture refuses second repair'; }; };").await.unwrap().check().unwrap();
    let before = journal_rows(&f.db).await;
    let result = repo
        .apply_prepared_batch(&batch, &op("partial-stop"))
        .await
        .unwrap();
    assert_eq!(result.applied_count(), 1);
    assert!(result.conflicts.is_empty());
    let stopped = result.stopped.unwrap();
    assert_eq!(stopped.proposal, refs[1].id);
    assert_eq!(stopped.unattempted, vec![refs[2].id.clone()]);
    assert_eq!(journal_rows(&f.db).await, before + 1);
    for (index, reference) in refs.iter().enumerate() {
        assert_eq!(
            f.proposal_row(&reference.id).await.unwrap().state,
            if index == 0 { "approved" } else { "pending" }
        );
    }
    f.db.inner()
        .query("REMOVE EVENT IF EXISTS refuse_repair_second ON TABLE semantic_operation;")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        repo.apply_batch(&refs[1..], &op("partial-positive"))
            .await
            .unwrap()
            .applied_count(),
        2
    );
}

#[tokio::test]
async fn classification_repair_decision_rolls_back_with_journal_failure() {
    let f = Fixture::enrolled("repair-journal-rollback").await;
    let fact =
        seed_scope_vector(&f, "rollback subject", &[1.0, 0.0], WriteTarget::Unassigned).await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let batch = ungrouped_batch(&repo, std::slice::from_ref(&reference)).await;
    let before = f.semantic_state().await;
    f.db.inner()
        .query(
            "DEFINE EVENT OVERWRITE refuse_classification_repair ON TABLE semantic_operation \
        WHEN $event = 'CREATE' THEN { THROW 'fixture refuses repair journal'; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let failed = repo
        .apply_prepared_batch(&batch, &op("repair-rollback"))
        .await
        .unwrap();
    assert!(failed.applied.is_empty() && failed.conflicts.is_empty() && failed.stopped.is_some());
    assert_eq!(f.semantic_state().await, before);
    assert_eq!(
        f.proposal_row(&reference.id).await.unwrap().state,
        "pending"
    );
    f.db.inner()
        .query("REMOVE EVENT IF EXISTS refuse_classification_repair ON TABLE semantic_operation;")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        repo.apply_prepared_batch(&batch, &op("repair-rollback"))
            .await
            .unwrap()
            .applied_count(),
        1
    );
}

#[tokio::test]
async fn classification_repair_revision_and_dismissal_win_after_capture() {
    let f = Fixture::enrolled("repair-captured-citation").await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let project = f.project("revised-destination").await;
    for dismiss in [false, true] {
        let fact = seed_scope_vector(
            &f,
            &format!("captured {dismiss}"),
            &[1.0, 0.0],
            WriteTarget::Unassigned,
        )
        .await;
        let original = repo
            .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
            .await
            .unwrap();
        let batch = ungrouped_batch(&repo, std::slice::from_ref(&original)).await;
        let current = if dismiss {
            repo.dismiss(&original).await.unwrap();
            None
        } else {
            Some(
                repo.revise_destination(original.clone(), MemoryScope::Project(project.clone()))
                    .await
                    .unwrap(),
            )
        };
        let before = f.semantic_state().await;
        let result = repo
            .apply_prepared_batch(&batch, &op(&format!("captured-{dismiss}")))
            .await
            .unwrap();
        assert_eq!(result.applied_count(), 0);
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.stopped.is_none());
        assert_eq!(f.semantic_state().await, before);
        if let Some(current) = current {
            assert!(repo.dismiss(&original).await.is_err());
            assert_eq!(
                repo.apply_batch(&[current], &op("fresh-citation"))
                    .await
                    .unwrap()
                    .applied_count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn classification_repair_retirement_stop_is_not_also_a_refusal() {
    let f = Fixture::enrolled("repair-retirement-stop").await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let mut facts = Vec::new();
    let mut refs = Vec::new();
    for content in ["retirement first", "retirement second", "retirement third"] {
        let fact = seed_scope_vector(&f, content, &[1.0, 0.0], WriteTarget::Unassigned).await;
        refs.push(
            repo.propose(&fact, Version::ZERO, MemoryScope::Personal, None)
                .await
                .unwrap(),
        );
        facts.push(fact);
    }
    let batch = ungrouped_batch(&repo, &refs).await;
    f.db.inner()
        .query("UPDATE $fact SET semantic_revision = 1;")
        .bind(("fact", facts[1].as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    f.db.inner().query("DEFINE EVENT OVERWRITE refuse_repair_retirement ON TABLE classification_proposal \
        WHEN $event = 'UPDATE' AND $after.state = 'stale' THEN { THROW 'fixture refuses retirement'; };")
        .await.unwrap().check().unwrap();
    let result = repo
        .apply_prepared_batch(&batch, &op("retirement-stop"))
        .await
        .unwrap();
    assert_eq!(result.applied_count(), 1);
    assert!(
        result.conflicts.is_empty(),
        "the unconfirmed slot cannot also be refused: {result:?}"
    );
    let stop = result.stopped.unwrap();
    assert_eq!(stop.proposal, refs[1].id);
    assert_eq!(stop.unattempted, vec![refs[2].id.clone()]);
    assert_eq!(f.proposal_row(&refs[0].id).await.unwrap().state, "approved");
    assert_eq!(f.proposal_row(&refs[1].id).await.unwrap().state, "pending");
    assert_eq!(f.proposal_row(&refs[2].id).await.unwrap().state, "pending");
    f.db.inner()
        .query("REMOVE EVENT IF EXISTS refuse_repair_retirement ON TABLE classification_proposal;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let remaining = repo
        .apply_batch(&refs[1..], &op("retirement-positive"))
        .await
        .unwrap();
    assert_eq!(remaining.applied_count(), 1);
    assert_eq!(remaining.conflicts.len(), 1);
    assert_eq!(f.proposal_row(&refs[1].id).await.unwrap().state, "stale");
}

#[tokio::test]
async fn classification_repair_hidden_arrival_does_not_enter_live_mean() {
    let f = Fixture::enrolled("repair-hidden-mean").await;
    let witness = seed_scope_vector(&f, "live witness", &[1.0, 0.0], WriteTarget::Personal).await;
    let hidden = f.seed_quarantined_unassigned("hidden arrival").await;
    let before_hidden = f.row(&hidden).await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    f.attach(&cluster, &witness).await;
    let mutation = MutationRepo::new(f.db.inner());
    let expectation = mutation
        .cluster_expectation(&f.owner, &cluster)
        .await
        .unwrap();
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let proposal = repo
        .propose(&hidden, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let captured = repo
        .prepare_batch(std::slice::from_ref(&proposal))
        .await
        .unwrap();
    let batch = captured
        .seal(&[ClassificationDestination::Chosen {
            reference: proposal.clone(),
            arriving: DestinationMembership::Join {
                expectation,
                centroid: vec![1.0, 0.0],
            },
        }])
        .unwrap();
    let result = repo
        .apply_prepared_batch(&batch, &op("hidden-mean"))
        .await
        .unwrap();
    assert_eq!(result.applied_count(), 1);
    assert_eq!(
        f.row(&hidden).await.quarantined_at,
        before_hidden.quarantined_at
    );
    assert_eq!(f.cluster_row(&cluster).await.centroid, vec![1.0, 0.0]);
    let decided = f.proposal_row(&proposal.id).await.unwrap();
    assert_eq!(decided.state, "approved");
    assert_eq!(
        decided.decided_by.as_ref(),
        Some(f.owner.account().as_record_id())
    );
    assert!(decided.decided_at.is_some());
    assert!(decided.destination_project.is_none());
}

#[tokio::test]
async fn classification_repair_batch_is_bound_to_capturing_owner() {
    let f = Fixture::enrolled("repair-capture-owner").await;
    let foreign = Fixture::enrolled("repair-other-owner").await;
    let fact = seed_scope_vector(&f, "owned capture", &[1.0, 0.0], WriteTarget::Unassigned).await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let captured = repo
        .prepare_batch(std::slice::from_ref(&reference))
        .await
        .unwrap();
    let batch = captured
        .seal(&[ClassificationDestination::Chosen {
            reference,
            arriving: DestinationMembership::Unclustered,
        }])
        .unwrap();
    let before = f.semantic_state().await;
    let foreign_result = ClassificationRepo::new(f.db.inner(), &foreign.owner)
        .apply_prepared_batch(&batch, &op("wrong-owner-batch"))
        .await;
    assert!(
        matches!(foreign_result, Err(StorageError::InvalidInput(_))),
        "a sealed command cannot inherit another owner's authority"
    );
    assert_eq!(f.semantic_state().await, before);
    assert_eq!(
        repo.apply_prepared_batch(&batch, &op("right-owner-batch"))
            .await
            .unwrap()
            .applied_count(),
        1
    );
}

#[tokio::test]
async fn classification_repair_source_last_member_detaches_without_centroid() {
    let f = Fixture::enrolled("repair-last-departure").await;
    let fact =
        seed_scope_vector(&f, "last source member", &[1.0, 0.0], WriteTarget::Personal).await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    f.attach(&cluster, &fact).await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let proposal = repo
        .propose(&fact, Version::ZERO, MemoryScope::Unassigned, None)
        .await
        .unwrap();
    let result = repo
        .apply_batch(std::slice::from_ref(&proposal), &op("last-departure"))
        .await
        .unwrap();
    assert_eq!(
        result.applied_count(),
        1,
        "an initially empty survivor set is a lawful detach: {result:?}"
    );
    assert!(f.cluster_members(&cluster).await.is_empty());
    assert_eq!(
        f.proposal_row(&proposal.id).await.unwrap().state,
        "approved"
    );
}

async fn exercise_driver_policy(mode: u8) {
    use alexandria_storage::models::cluster::mean_centroid;
    use alexandria_storage::models::mutation::{
        ClassificationApproval, ClusterChange, ExpectedState,
    };
    use alexandria_storage::repos::{MAX_PREPARATIONS, commit_prepared};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = Fixture::enrolled(&format!("repair-policy-{mode}")).await;
    let witness = seed_scope_vector(&f, "policy witness", &[1.0, 0.0], WriteTarget::Personal).await;
    let arrival =
        seed_scope_vector(&f, "policy arrival", &[0.0, 1.0], WriteTarget::Unassigned).await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    f.attach(&cluster, &witness).await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&arrival, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let mutation = MutationRepo::new(f.db.inner());
    let snapshot = mutation.state(&f.owner, &arrival).await.unwrap().unwrap();
    let expected = [ExpectedState::of(&snapshot)];
    let base = PreparedChangeSet::new(operation_kind::MOVE)
        .effect(effect::SCOPE)
        .effect(EdgeKind::ClusterMembership.effect())
        .effect(effect::CLUSTER_CENTROID)
        .fact(FactChange::Update {
            fact: arrival.clone(),
            expected: Version::ZERO,
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: None,
            scope: Some(MemoryScope::Personal),
        })
        .approve_classification(ClassificationApproval {
            proposal: RecordId::parse_simple(&reference.id).unwrap(),
            proposal_revision: reference.revision,
            fact: arrival.clone(),
            source_revision: Version::ZERO,
            destination: MemoryScope::Personal,
        });
    let attempts = AtomicUsize::new(0);
    let last_prepared = std::sync::Mutex::new(None);
    let access = MutationContext::issued_to(&f.owner);
    let before = journal_rows(&f.db).await;
    let result = commit_prepared(
        &mutation,
        &access,
        &op("driver-policy"),
        &expected,
        &base.effects,
        || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            let mutation = &mutation;
            let owner = &f.owner;
            let db = f.db.inner();
            let cluster = &cluster;
            let witness = &witness;
            let arrival = &arrival;
            let base = &base;
            let classification = &repo;
            let reference = &reference;
            let last_prepared = &last_prepared;
            async move {
                let expectation = mutation.cluster_expectation(owner, cluster).await?;
                let mut vectors: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .map(|member| member.embedding.as_slice())
                    .collect();
                vectors.push(&[0.0, 1.0]);
                let centroid = mean_centroid(&vectors).unwrap();
                let mut change = base.clone().cluster(ClusterChange::Effect {
                    expectation,
                    add_member: Some(RelationTarget::Existing(arrival.as_record_id().clone())),
                    remove_member: None,
                    centroid: Some(centroid),
                });
                if mode == 1 || (matches!(mode, 0 | 3 | 4) && attempt == 0) {
                    // A real compatibility writer changes the sampled vector without incrementing guards.
                    let vector: Vec<f32> = if attempt.is_multiple_of(2) {
                        vec![0.0, 1.0]
                    } else {
                        vec![1.0, 0.0]
                    };
                    db.query("UPDATE $fact SET embedding = $vector;")
                        .bind(("fact", witness.as_record_id().clone()))
                        .bind(("vector", vector))
                        .await
                        .unwrap()
                        .check()
                        .unwrap();
                }
                if matches!(mode, 3 | 4) && attempt == 1 {
                    if mode == 3 {
                        classification
                            .revise_destination(reference.clone(), MemoryScope::Unassigned)
                            .await
                            .unwrap();
                    } else {
                        classification.dismiss(reference).await.unwrap();
                    }
                }
                if mode == 2 {
                    change.effects.push(effect::TAGS.into());
                }
                *last_prepared.lock().unwrap() = Some(change.clone());
                Ok(change)
            }
        },
    )
    .await;
    match mode {
        0 => {
            let applied = result.unwrap();
            assert!(!applied.replayed);
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            assert_eq!(journal_rows(&f.db).await, before + 1);
            assert_eq!(
                f.proposal_row(&reference.id).await.unwrap().state,
                "approved"
            );
            assert_eq!(f.cluster_row(&cluster).await.centroid, vec![0.0, 1.0]);
            let frozen = last_prepared.lock().unwrap().clone().unwrap();
            f.db.inner()
                .query("UPDATE $id SET embedding = [-1.0, 0.0];")
                .bind(("id", witness.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
            let committed = f.semantic_state().await;
            let replay = mutation
                .apply(&access, &op("driver-policy"), &expected, &frozen)
                .await
                .unwrap();
            assert!(replay.replayed);
            assert_eq!(f.semantic_state().await, committed);
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
        }
        1 => {
            assert!(matches!(
                result,
                Err(StorageError::RetryableBookkeepingConflict)
            ));
            assert_eq!(attempts.load(Ordering::SeqCst), MAX_PREPARATIONS);
        }
        _ => {
            assert!(matches!(result, Err(StorageError::Stale)));
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                if mode == 2 { 1 } else { 2 }
            );
        }
    }
    if mode != 0 {
        assert_eq!(journal_rows(&f.db).await, before);
        let proposal = f.proposal_row(&reference.id).await.unwrap();
        assert_eq!(
            proposal.state,
            if mode == 4 { "dismissed" } else { "pending" }
        );
        if mode == 3 {
            assert_eq!(proposal.revision, 1);
            assert_eq!(proposal.destination_kind, "unassigned");
        }
        assert_eq!(f.row(&arrival).await.scope.as_deref(), Some("unassigned"));
    }
}

#[tokio::test]
async fn classification_repair_driver_attempt_two() {
    exercise_driver_policy(0).await;
}
#[tokio::test]
async fn classification_repair_driver_exhaustion() {
    exercise_driver_policy(1).await;
}
#[tokio::test]
async fn classification_repair_driver_effect_mismatch() {
    exercise_driver_policy(2).await;
}
#[tokio::test]
async fn classification_repair_driver_retains_citation_on_retry() {
    exercise_driver_policy(3).await;
    exercise_driver_policy(4).await;
}

#[tokio::test]
async fn classification_repair_driver_preparation_errors_return_once() {
    use alexandria_storage::repos::commit_prepared;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = Fixture::enrolled("repair-prepare-errors").await;
    let mutation = MutationRepo::new(f.db.inner());
    let access = MutationContext::issued_to(&f.owner);
    let before = f.semantic_state().await;
    for case in 0..5 {
        let callbacks = AtomicUsize::new(0);
        let result = commit_prepared(&mutation, &access, &op("prepare-errors"), &[], &[], || {
            callbacks.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(match case {
                    0 => StorageError::Stale,
                    1 => StorageError::NotFound,
                    2 => StorageError::InvalidInput("fixture".into()),
                    3 => StorageError::Internal(anyhow::anyhow!("fixture")),
                    _ => StorageError::RetryableBookkeepingConflict,
                })
            }
        })
        .await;
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert!(matches!(
            (case, result),
            (0, Err(StorageError::Stale))
                | (1, Err(StorageError::NotFound))
                | (2, Err(StorageError::InvalidInput(_)))
                | (3, Err(StorageError::Internal(_)))
                | (4, Err(StorageError::RetryableBookkeepingConflict))
        ));
        assert_eq!(f.semantic_state().await, before);
    }
}

#[tokio::test]
async fn classification_repair_scope_only_clause_rejects_extra_changes() {
    use alexandria_storage::models::mutation::{ClassificationApproval, ExpectedState};
    let f = Fixture::enrolled("repair-scope-only").await;
    let fact = seed_scope_vector(
        &f,
        "scope-only source",
        &[1.0, 0.0],
        WriteTarget::Unassigned,
    )
    .await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let mutation = MutationRepo::new(f.db.inner());
    let snapshot = mutation.state(&f.owner, &fact).await.unwrap().unwrap();
    let expected = [ExpectedState::of(&snapshot)];
    let access = MutationContext::issued_to(&f.owner);
    let base = PreparedChangeSet::new(operation_kind::MOVE)
        .effect(effect::SCOPE)
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: Version::ZERO,
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: None,
            scope: Some(MemoryScope::Personal),
        })
        .approve_classification(ClassificationApproval {
            proposal: RecordId::parse_simple(&reference.id).unwrap(),
            proposal_revision: reference.revision,
            fact: fact.clone(),
            source_revision: Version::ZERO,
            destination: MemoryScope::Personal,
        });
    let before = f.semantic_state().await;
    for case in 0..8 {
        let mut change = base.clone();
        if let FactChange::Update {
            content,
            tags,
            confidence,
            embedding,
            visibility,
            ..
        } = &mut change.facts[0]
        {
            match case {
                0 => *content = Some("smuggled edit".into()),
                1 => *tags = Some(vec![]),
                2 => *confidence = Some(0.9),
                3 => *embedding = snapshot.embedding.clone(),
                4 => {
                    *visibility = Some(Visibility {
                        deleted: true,
                        quarantined_at: None,
                    })
                }
                5 => *visibility = Some(Visibility::quarantined(Utc::now())),
                _ => (),
            }
        }
        if case == 6 {
            change.facts.push(change.facts[0].clone());
        }
        if case == 7 {
            change.facts.push(FactChange::Create {
                scope: MemoryScope::Personal,
                content: "extra fact".into(),
                tags: vec![],
                confidence: 0.5,
                embedding: snapshot.embedding.clone().unwrap(),
                visibility: Visibility::live(),
                metadata: None,
            });
        }
        let result = mutation
            .apply(
                &access,
                &op(&format!("scope-only-{case}")),
                &expected,
                &change,
            )
            .await;
        assert!(
            matches!(result, Err(StorageError::InvalidInput(_))),
            "forbidden field {case}: {result:?}"
        );
        assert_eq!(f.semantic_state().await, before);
    }
    let applied = mutation
        .apply(&access, &op("scope-only-positive"), &expected, &base)
        .await
        .unwrap();
    assert!(!applied.replayed);
    assert_eq!(
        f.proposal_row(&reference.id).await.unwrap().state,
        "approved"
    );
}

#[tokio::test]
async fn classification_repair_seal_stop_accounts_for_every_candidate() {
    let f = Fixture::enrolled("repair-seal-stop").await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let mut references = Vec::new();
    for text in ["before stop", "unconfirmed", "after stop"] {
        let fact = seed_scope_vector(&f, text, &[1.0, 0.0], WriteTarget::Unassigned).await;
        references.push(
            repo.propose(&fact, Version::ZERO, MemoryScope::Personal, None)
                .await
                .unwrap(),
        );
    }
    let before = f.semantic_state().await;
    let captured = repo.prepare_batch(&references).await.unwrap();
    let batch = captured
        .seal(&[
            ClassificationDestination::Chosen {
                reference: references[0].clone(),
                arriving: DestinationMembership::Unclustered,
            },
            ClassificationDestination::Unconfirmed {
                reference: references[1].clone(),
            },
        ])
        .unwrap();
    let result = repo
        .apply_prepared_batch(&batch, &op("repair-seal-stop"))
        .await
        .unwrap();
    assert!(result.applied.is_empty() && result.conflicts.is_empty());
    let stopped = result.stopped.unwrap();
    assert_eq!(stopped.proposal, references[1].id);
    assert_eq!(
        stopped.unattempted,
        vec![references[0].id.clone(), references[2].id.clone()]
    );
    assert_eq!(f.semantic_state().await, before);
    let positive = repo
        .apply_batch(&references, &op("repair-seal-stop-positive"))
        .await
        .unwrap();
    assert_eq!(positive.applied_count(), 3);
}

#[tokio::test]
async fn classification_repair_seal_requires_exact_citation() {
    let f = Fixture::enrolled("repair-seal-citation").await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let fact = seed_scope_vector(&f, "exact citation", &[1.0, 0.0], WriteTarget::Unassigned).await;
    let reference = repo
        .propose(&fact, Version::ZERO, MemoryScope::Personal, None)
        .await
        .unwrap();
    let captured = repo
        .prepare_batch(std::slice::from_ref(&reference))
        .await
        .unwrap();
    let mut wrong = reference.clone();
    wrong.revision = Version::new(1);
    let rejected = captured.seal(&[ClassificationDestination::Chosen {
        reference: wrong,
        arriving: DestinationMembership::Unclustered,
    }]);
    assert!(
        matches!(
            rejected,
            Err(alexandria_storage::error::StorageError::InvalidInput(_))
        ),
        "sealing must not accept a different proposal revision"
    );
    let positive = repo
        .apply_batch(&[reference], &op("repair-seal-citation-positive"))
        .await
        .unwrap();
    assert_eq!(positive.applied_count(), 1);
}

#[tokio::test]
async fn classification_repair_legacy_apply_time_refusal_is_reported() {
    let f = Fixture::enrolled("repair-apply-time-refusal").await;
    let first = seed_scope_vector(&f, "first to file", &[1.0, 0.0], WriteTarget::Unassigned).await;
    let second = seed_scope_vector(
        &f,
        "repair second to edit",
        &[0.0, 1.0],
        WriteTarget::Unassigned,
    )
    .await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let a = repo
        .propose(&first, Version::new(0), MemoryScope::Personal, None)
        .await
        .unwrap();
    let b = repo
        .propose(&second, Version::new(0), MemoryScope::Personal, None)
        .await
        .unwrap();
    // A real conditional writer between batch preflight and the second dispatch, not a mocked error.
    f.db.inner()
        .query(
            "DEFINE EVENT OVERWRITE repair_edit_second ON TABLE semantic_operation \
        WHEN $event = 'CREATE' THEN { UPDATE fact SET content = 'repair independently edited', \
        semantic_revision = 1 WHERE content = 'repair second to edit'; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let before = journal_rows(&f.db).await;
    let result = repo
        .apply_batch(&[a.clone(), b.clone()], &op("repair-apply-time-refusal"))
        .await
        .unwrap();
    assert_eq!(result.applied_count(), 1);
    assert_eq!(
        f.row(&second).await.semantic_revision,
        Some(1),
        "the event must actually change the second source"
    );
    assert_eq!(
        result.conflicts.len(),
        1,
        "an apply-time refusal was discarded"
    );
    assert_eq!(result.conflicts[0].proposal, b.id);
    assert_eq!(
        result.conflicts[0].reason,
        alexandria_storage::models::mutation::BatchConflictReason::Stale
    );
    assert_eq!(f.proposal_row(&a.id).await.unwrap().state, "approved");
    assert_ne!(f.proposal_row(&b.id).await.unwrap().state, "approved");
    assert_eq!(journal_rows(&f.db).await, before + 1);
    f.db.inner()
        .query("REMOVE EVENT IF EXISTS repair_edit_second ON TABLE semantic_operation;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let current = repo
        .propose(&second, Version::new(1), MemoryScope::Personal, None)
        .await
        .unwrap();
    let positive = repo
        .apply_batch(&[current], &op("repair-refusal-positive"))
        .await
        .unwrap();
    assert_eq!(positive.applied_count(), 1);
}

#[tokio::test]
async fn classification_repair_legacy_proposal_change_is_not_overwritten() {
    let f = Fixture::enrolled("repair-proposal-decision").await;
    let fact = seed_scope_vector(
        &f,
        "proposal race subject",
        &[1.0, 0.0],
        WriteTarget::Unassigned,
    )
    .await;
    let repo = ClassificationRepo::new(f.db.inner(), &f.owner);
    let reference = repo
        .propose(&fact, Version::new(0), MemoryScope::Personal, None)
        .await
        .unwrap();
    f.db.inner()
        .query(
            "DEFINE EVENT OVERWRITE repair_revise_opinion ON TABLE semantic_operation \
        WHEN $event = 'CREATE' THEN { UPDATE classification_proposal SET revision = revision + 1, \
        destination_kind = 'unassigned', destination_project = NONE \
        WHERE state = 'pending' AND revision = 0; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let result = repo
        .apply_batch(
            std::slice::from_ref(&reference),
            &op("repair-proposal-decision"),
        )
        .await
        .unwrap();
    let opinion = f.proposal_row(&reference.id).await.unwrap();
    let actual = f.row(&fact).await;
    let approval_won = opinion.revision == 0
        && opinion.state == "approved"
        && opinion.destination_kind == "personal"
        && actual.scope.as_deref() == Some("personal");
    let revision_won = opinion.revision == 1
        && opinion.state == "pending"
        && opinion.destination_kind == "unassigned"
        && actual.scope.as_deref() == Some("unassigned");
    assert!(
        approval_won || revision_won,
        "old approval overwrote a new opinion: revision={}, state={}, destination={}, fact_scope={:?}",
        opinion.revision,
        opinion.state,
        opinion.destination_kind,
        actual.scope
    );
    if approval_won {
        assert_eq!(result.applied_count(), 1);
    } else {
        assert_eq!(result.applied_count(), 0);
        assert_eq!(result.conflicts.len(), 1);
    }
    f.db.inner()
        .query("REMOVE EVENT IF EXISTS repair_revise_opinion ON TABLE semantic_operation;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let current = repo
        .revise_destination(reference, MemoryScope::Unassigned)
        .await;
    assert!(
        current.is_err(),
        "a stale or decided citation must not revise the winner"
    );
}

#[tokio::test]
async fn classification_repair_two_arrivals_refresh_same_destination() {
    let f = Fixture::enrolled("repair-shared-destination").await;
    let project = f.project("destination").await;
    let witness = seed_scope_vector(
        &f,
        "destination witness",
        &[1.0, 0.0],
        WriteTarget::Project(project.clone()),
    )
    .await;
    let first = seed_scope_vector(&f, "first arrival", &[0.0, 1.0], WriteTarget::Unassigned).await;
    let second =
        seed_scope_vector(&f, "second arrival", &[1.0, 0.0], WriteTarget::Unassigned).await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    f.attach(&cluster, &witness).await;
    let classification = ClassificationRepo::new(f.db.inner(), &f.owner);
    let a = classification
        .propose(
            &first,
            Version::new(0),
            MemoryScope::Project(project.clone()),
            None,
        )
        .await
        .unwrap();
    let b = classification
        .propose(
            &second,
            Version::new(0),
            MemoryScope::Project(project.clone()),
            None,
        )
        .await
        .unwrap();
    let mutation = MutationRepo::new(f.db.inner());
    let initial = mutation
        .cluster_expectation(&f.owner, &cluster)
        .await
        .unwrap();
    let candidates = classification
        .prepare_batch(&[a.clone(), b.clone()])
        .await
        .unwrap();
    // Both arrivals join the SAME existing witness cluster; seal strips the stale guards and keeps
    // only the cluster identity, so the second entry's destination must be refreshed, not refused.
    let destinations: Vec<ClassificationDestination> = candidates
        .candidates()
        .iter()
        .map(|candidate| ClassificationDestination::Chosen {
            reference: candidate.reference().clone(),
            arriving: DestinationMembership::Join {
                expectation: initial.clone(),
                centroid: vec![1.0, 0.0],
            },
        })
        .collect();
    let prepared = candidates.seal(&destinations).unwrap();
    let before = journal_rows(&f.db).await;
    let result = classification
        .apply_prepared_batch(&prepared, &op("repair-shared-destination"))
        .await
        .expect("an earlier arrival must not abort the later arrival's fixed destination");
    assert_eq!(result.applied_count(), 2);
    assert!(result.conflicts.is_empty());
    assert_ne!(result.applied[0], result.applied[1]);
    assert_eq!(journal_rows(&f.db).await, before + 2);
    assert_eq!(f.proposal_row(&a.id).await.unwrap().state, "approved");
    assert_eq!(f.proposal_row(&b.id).await.unwrap().state, "approved");
    assert_eq!(
        f.cluster_row(&cluster).await.centroid,
        vec![2.0f32 / 3.0, 1.0f32 / 3.0]
    );
    let mut expected = vec![
        key(witness.as_record_id()),
        key(first.as_record_id()),
        key(second.as_record_id()),
    ];
    expected.sort();
    assert_eq!(f.cluster_members(&cluster).await, expected);
    for fact in [&first, &second] {
        let row = f.row(fact).await;
        assert_eq!(row.project.as_ref(), Some(project.as_record_id()));
        assert!(row.quarantined_at.is_none());
    }
}
