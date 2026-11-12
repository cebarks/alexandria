use super::approval_atomicity::seed_scope_vector;
use super::*;
use alexandria_storage::models::maintenance::{disposition, job};
use alexandria_storage::models::mutation::{
    ClassificationApproval, ClusterChange, ExpectedState, MaintenanceSummary, ProvenanceRow,
    RawDocument, RelationChange, SessionLink,
};

#[tokio::test]
async fn classification_review_envelope_rejects_unrelated_writes() {
    let mut failures = Vec::new();
    for case in [
        "raw",
        "provenance",
        "session",
        "add-edge",
        "remove-edge",
        "maintenance",
        "heat",
        "embedding-space",
        "kind",
        "effects",
        "recenter",
        "detach-other",
        "create-other",
        "two-arrivals",
        "create-wrong-vector",
        "missing-detach",
        "no-expectation",
        "ordinary-raw-control",
    ] {
        let f = Fixture::enrolled(&format!("envelope-{case}")).await;
        let fact_target = if case == "missing-detach" {
            WriteTarget::Personal
        } else {
            WriteTarget::Unassigned
        };
        let destination = if case == "missing-detach" {
            MemoryScope::Unassigned
        } else {
            MemoryScope::Personal
        };
        let fact = seed_scope_vector(&f, "placement subject", &[1.0, 0.0], fact_target).await;
        let witness = seed_scope_vector(
            &f,
            "unrelated cluster member",
            &[0.0, 1.0],
            WriteTarget::Personal,
        )
        .await;
        let read = f.read().await;
        let center = if case == "missing-detach" {
            alexandria_storage::models::cluster::mean_centroid(&[&[1.0, 0.0], &[0.0, 1.0]]).unwrap()
        } else {
            vec![0.0, 1.0]
        };
        let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
            .create(None, &center, Utc::now())
            .await
            .unwrap()
            .id
            .unwrap();
        f.attach(&cluster, &witness).await;
        if case == "missing-detach" {
            f.attach(&cluster, &fact).await;
        }
        let session = RecordId::new("session", "envelope-session");
        f.db.inner().query("CREATE $sess SET external_id = 'envelope-session', account = $account, workspace = $workspace;")
            .bind(("sess", session.clone())).bind(("account", f.owner.account().as_record_id().clone()))
            .bind(("workspace", f.owner.workspace().as_record_id().clone())).await.unwrap().check().unwrap();
        if case == "remove-edge" {
            f.db.inner()
                .query("RELATE $from->memory_edge->$to SET edge_type = 'related', strength = 1.0;")
                .bind(("from", fact.as_record_id().clone()))
                .bind(("to", witness.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let proposal = ClassificationRepo::new(f.db.inner(), &f.owner)
            .propose(&fact, Version::ZERO, destination.clone(), None)
            .await
            .unwrap();
        let mutation = MutationRepo::new(f.db.inner());
        let original = mutation.state(&f.owner, &fact).await.unwrap().unwrap();
        let expected = [ExpectedState::of(&original)];
        let mut base = PreparedChangeSet::new(operation_kind::MOVE)
            .effect(effect::SCOPE)
            .fact(FactChange::Update {
                fact: fact.clone(),
                expected: original.version,
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                visibility: None,
                scope: Some(destination.clone()),
            })
            .approve_classification(ClassificationApproval {
                proposal: RecordId::parse_simple(&proposal.id).unwrap(),
                proposal_revision: proposal.revision,
                fact: fact.clone(),
                source_revision: original.version,
                destination,
            });
        if case == "missing-detach" {
            base.cluster_changes.push(ClusterChange::Detach {
                expectation: mutation
                    .cluster_expectation(&f.owner, &cluster)
                    .await
                    .unwrap(),
                member: fact.as_record_id().clone(),
                surviving_centroid: Some(vec![0.0, 1.0]),
            });
            base.effects.push(EdgeKind::ClusterMembership.effect());
            base.effects.push(effect::CLUSTER_CENTROID.to_string());
        }
        let mut invalid = base.clone();
        match case {
            "raw" | "ordinary-raw-control" => {
                invalid.raw_documents.push(RawDocument {
                    slot: 0,
                    content: "unrelated source document".into(),
                });
                if case == "ordinary-raw-control" {
                    invalid.classification_approval = None;
                }
            }
            "provenance" => invalid.provenance_rows.push(ProvenanceRow {
                slot: 0,
                kind: "agent".into(),
                timestamp: Utc::now(),
                fact: RelationTarget::Existing(fact.as_record_id().clone()),
                metadata: None,
            }),
            "session" => invalid.session_links.push(SessionLink {
                session,
                to: RelationTarget::Existing(fact.as_record_id().clone()),
            }),
            "add-edge" => invalid.edge_additions.push(RelationChange::memory(
                "related",
                RelationTarget::Existing(fact.as_record_id().clone()),
                RelationTarget::Existing(witness.as_record_id().clone()),
                1.0,
            )),
            "remove-edge" => invalid.edge_removals.push(RelationChange::memory(
                "related",
                RelationTarget::Existing(fact.as_record_id().clone()),
                RelationTarget::Existing(witness.as_record_id().clone()),
                1.0,
            )),
            "maintenance" => {
                invalid.maintenance_summary = Some(MaintenanceSummary {
                    action: "demote".into(),
                    source_id: alexandria_storage::record_id_to_string(fact.as_record_id()),
                    target_ids: Vec::new(),
                    members_moved: 1,
                    run_id: "run-envelope".into(),
                    job: job::APPRAISE.into(),
                    disposition: disposition::SOFT_DELETE.into(),
                    previous_value: Some(0.5),
                })
            }
            "heat" => invalid.heat_seed = Some(1.0),
            "embedding-space" => invalid.embedding_space = Some("unrelated-identity".into()),
            "kind" => invalid.kind = operation_kind::UPDATE,
            "effects" => invalid.effects.push(effect::SCOPE.to_string()),
            "missing-detach" => {
                invalid.cluster_changes.clear();
                invalid.effects = vec![effect::SCOPE.to_string()];
            }
            "two-arrivals" => {
                for _ in 0..2 {
                    invalid.cluster_changes.push(ClusterChange::Create {
                        centroid: vec![1.0, 0.0],
                        label: None,
                        member: RelationTarget::Existing(fact.as_record_id().clone()),
                    });
                }
            }
            "create-wrong-vector" => invalid.cluster_changes.push(ClusterChange::Create {
                centroid: vec![0.0, 1.0],
                label: None,
                member: RelationTarget::Existing(fact.as_record_id().clone()),
            }),
            "recenter" => invalid.cluster_changes.push(ClusterChange::Effect {
                expectation: mutation
                    .cluster_expectation(&f.owner, &cluster)
                    .await
                    .unwrap(),
                add_member: None,
                remove_member: None,
                centroid: Some(vec![0.0, 1.0]),
            }),
            "detach-other" => invalid.cluster_changes.push(ClusterChange::Detach {
                expectation: mutation
                    .cluster_expectation(&f.owner, &cluster)
                    .await
                    .unwrap(),
                member: witness.as_record_id().clone(),
                surviving_centroid: None,
            }),
            "create-other" => invalid.cluster_changes.push(ClusterChange::Create {
                centroid: vec![0.0, 1.0],
                label: None,
                member: RelationTarget::Existing(witness.as_record_id().clone()),
            }),
            "no-expectation" => {}
            _ => unreachable!(),
        }
        match case {
            "recenter" => invalid.effects.push(effect::CLUSTER_CENTROID.to_string()),
            "detach-other" | "create-other" | "create-wrong-vector" => {
                invalid.effects.push(EdgeKind::ClusterMembership.effect())
            }
            "two-arrivals" => invalid.effects.extend([
                EdgeKind::ClusterMembership.effect(),
                EdgeKind::ClusterMembership.effect(),
            ]),
            _ => {}
        }
        let before = f.semantic_state().await;
        let result = mutation
            .apply(
                &MutationContext::issued_to(&f.owner),
                &op(case),
                if case == "no-expectation" {
                    &[]
                } else {
                    &expected
                },
                &invalid,
            )
            .await;
        if case == "ordinary-raw-control" {
            result
                .expect("the ordinary kernel retains its non-classification raw-document support");
            let mut rows =
                f.db.inner()
                    .query("SELECT * FROM raw;")
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            let raw: Vec<serde_json::Value> = rows.take(0).unwrap();
            assert_eq!(raw.len(), 1);
            continue;
        }
        if !matches!(result, Err(StorageError::InvalidInput(_))) {
            failures.push(format!("{case}: {result:?}"));
            continue;
        }
        assert_eq!(f.semantic_state().await, before, "{case}");
        mutation
            .apply(
                &MutationContext::issued_to(&f.owner),
                &op("lawful-placement"),
                &expected,
                &base,
            )
            .await
            .expect("the lawful scope-only approval must still commit");
        assert_eq!(
            f.proposal_row(&proposal.id).await.unwrap().state,
            "approved"
        );
    }
    assert!(
        failures.is_empty(),
        "scope-only envelope accepted or misclassified unrelated writes: {failures:?}"
    );
}
