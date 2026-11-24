use super::*;
use alexandria_storage::models::mutation::{
    ClusterSnapshot, DestinationMembership, LeavingCluster, MoveReconciliation,
};
use alexandria_storage::repos::ClassificationDestination;

async fn seed_in(f: &Fixture, target: WriteTarget) -> FactId {
    let read = if matches!(&target, WriteTarget::Unassigned) {
        f.unassigned().await
    } else {
        f.read().await
    };
    let row = ScopedMemoryRepo::new(f.db.inner(), &f.owner, &read)
        .create_fact(
            "constant-vector partition fixture",
            0.5,
            &[1.0, 0.0],
            &[],
            target,
            Utc::now(),
        )
        .await
        .expect("seed assigned row");
    FactId::try_from(row.id.expect("fact id")).unwrap()
}

async fn cluster(f: &Fixture, members: &[&FactId]) -> RecordId {
    let read = f.read().await;
    let id = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    for member in members {
        f.attach(&id, member).await;
    }
    id
}

async fn candidate_ids(f: &Fixture, scope: &MemoryScope) -> Vec<RecordId> {
    let read = f.read().await;
    ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .candidates_for_partition(scope)
        .await
        .expect("partition candidates")
        .into_iter()
        .map(|(c, _)| c.id.expect("candidate id"))
        .collect()
}

fn image<'a>(images: &'a [SemanticSnapshot], id: &RecordId) -> &'a ClusterSnapshot {
    images
        .iter()
        .find_map(|image| match image {
            SemanticSnapshot::Cluster(c) if &c.id == id => Some(c),
            _ => None,
        })
        .expect("affected existing cluster must have an exact journal image")
}

async fn foreign_population(f: &Fixture) -> (FactId, RecordId) {
    use alexandria_storage::repos::AuthRepo;
    use chrono::TimeDelta;
    let ids = IdentityRepo::new(f.db.inner())
        .bootstrap_owner(
            "foreign-candidate",
            HASH,
            &op("foreign-candidate-bootstrap"),
        )
        .await
        .unwrap();
    let auth = AuthRepo::new(f.db.inner());
    let session = auth
        .mint_browser_session(
            &ids.account,
            TimeDelta::minutes(30),
            TimeDelta::hours(24),
            Utc::now(),
        )
        .await
        .unwrap();
    let cookie = session.cookie.render().expose().to_string();
    let principal = auth
        .authenticate_browser_session(&cookie, TimeDelta::minutes(30), Utc::now())
        .await
        .unwrap();
    let access = AccessRepo::new(&f.db);
    let owner = access.authorize(&principal).await.unwrap();
    let read = access
        .select(&owner, ReadSelection::Assigned)
        .await
        .unwrap();
    let fact = ScopedMemoryRepo::new(f.db.inner(), &owner, &read)
        .create_fact(
            "foreign workspace member",
            0.5,
            &[1.0, 0.0],
            &[],
            WriteTarget::Personal,
            Utc::now(),
        )
        .await
        .unwrap()
        .id
        .unwrap();
    let cluster = ScopedClusterRepo::new(f.db.inner(), &owner, &read)
        .create(None, &[1.0, 0.0], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    let fact = FactId::try_from(fact).unwrap();
    f.attach(&cluster, &fact).await;
    (fact, cluster)
}

#[tokio::test]
async fn destination_candidates_exclude_other_partitions_and_mixed_or_empty_clusters() {
    let f = Fixture::enrolled("candidate-purity").await;
    let a = f.project("a").await;
    let b = f.project("b").await;
    let personal = seed_in(&f, WriteTarget::Personal).await;
    let a_member = seed_in(&f, WriteTarget::Project(a.clone())).await;
    let b_member = seed_in(&f, WriteTarget::Project(b.clone())).await;
    let personal_cluster = cluster(&f, &[&personal]).await;
    let a_cluster = cluster(&f, &[&a_member]).await;
    let b_cluster = cluster(&f, &[&b_member]).await;
    let mixed = cluster(&f, &[&personal, &b_member]).await;
    let empty = cluster(&f, &[]).await;
    let unassigned = seed_in(&f, WriteTarget::Unassigned).await;
    cluster(&f, &[&unassigned]).await;
    cluster(&f, &[&b_member, &unassigned]).await;
    let unstamped = seed_in(&f, WriteTarget::Personal).await;
    f.db.inner()
        .query("UPDATE $fact UNSET scope, project;")
        .bind(("fact", unstamped.as_record_id().clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    cluster(&f, &[&b_member, &unstamped]).await;
    let (foreign, _) = foreign_population(&f).await;
    cluster(&f, &[&b_member, &foreign]).await;
    assert_eq!(
        candidate_ids(&f, &MemoryScope::Personal).await,
        vec![personal_cluster]
    );
    assert_eq!(
        candidate_ids(&f, &MemoryScope::Project(a)).await,
        vec![a_cluster]
    );
    assert_eq!(
        candidate_ids(&f, &MemoryScope::Project(b)).await,
        vec![b_cluster]
    );
    assert_eq!(f.cluster_members(&mixed).await.len(), 2);
    assert!(f.cluster_members(&empty).await.is_empty());
}

#[tokio::test]
async fn classified_b_then_personal_to_a_to_b_preserves_exact_cluster_history() {
    let f = Fixture::enrolled("constant-move").await;
    let a = f.project("a").await;
    let b = f.project("b").await;
    let repo = MutationRepo::new(f.db.inner());
    let classify = ClassificationRepo::new(f.db.inner(), &f.owner);
    let b_member = seed_in(&f, WriteTarget::Personal).await;
    let proposal = classify
        .propose(
            &b_member,
            Version::ZERO,
            MemoryScope::Project(b.clone()),
            None,
        )
        .await
        .unwrap();
    let candidates = classify.prepare_batch(&[proposal]).await.unwrap();
    let destinations: Vec<ClassificationDestination> = candidates
        .candidates()
        .iter()
        .map(|candidate| ClassificationDestination::Chosen {
            reference: candidate.reference().clone(),
            arriving: DestinationMembership::Create {
                centroid: vec![1.0, 0.0],
            },
        })
        .collect();
    let prepared = candidates.seal(&destinations).unwrap();
    let result = classify
        .apply_prepared_batch(&prepared, &op("classify-b"))
        .await
        .unwrap();
    assert_eq!(
        result.applied_count(),
        1,
        "classification must establish B: {result:?}"
    );
    let b_cluster = repo
        .cluster_of(&f.owner, b_member.as_record_id())
        .await
        .unwrap()
        .unwrap()
        .0;
    let creation = repo
        .operation_by_id(
            &f.owner,
            &RecordId::parse_simple(&result.applied[0]).unwrap(),
        )
        .await
        .unwrap()
        .expect("classification operation");
    assert!(
        !creation
            .preimages
            .iter()
            .any(|image| matches!(image, SemanticSnapshot::Cluster(_))),
        "a newly created cluster has no invented earlier state"
    );
    assert_eq!(
        image(&creation.postimages, &b_cluster).members,
        vec![b_member.as_record_id().clone()]
    );
    assert_eq!(
        image(&creation.postimages, &b_cluster).centroid,
        vec![1.0, 0.0]
    );
    let a_member = seed_in(&f, WriteTarget::Project(a.clone())).await;
    let a_cluster = cluster(&f, &[&a_member]).await;
    let mover = seed_in(&f, WriteTarget::Personal).await;
    let personal_stayer = seed_in(&f, WriteTarget::Personal).await;
    let personal_cluster = cluster(&f, &[&mover, &personal_stayer]).await;

    for (version, source_cluster, destination, destination_cluster, source_survivor, key) in [
        (
            0,
            &personal_cluster,
            MemoryScope::Project(a.clone()),
            &a_cluster,
            &personal_stayer,
            "into-a",
        ),
        (
            1,
            &a_cluster,
            MemoryScope::Project(b.clone()),
            &b_cluster,
            &a_member,
            "into-b",
        ),
    ] {
        assert_eq!(
            candidate_ids(&f, &destination).await,
            vec![destination_cluster.clone()]
        );
        let source_guards = repo
            .cluster_expectation(&f.owner, source_cluster)
            .await
            .unwrap();
        let destination_guards = repo
            .cluster_expectation(&f.owner, destination_cluster)
            .await
            .unwrap();
        let before_count = journal_rows(&f.db).await;
        repo.move_scope_reconciled(
            &f.owner,
            mover.clone(),
            Version::new(version),
            destination.clone(),
            &op(key),
            &MoveReconciliation {
                leaving: None,
                arriving: DestinationMembership::Join {
                    expectation: destination_guards.clone(),
                    centroid: vec![1.0, 0.0],
                },
            },
        )
        .await
        .expect("constant-vector cross-partition move must commit");
        assert_eq!(journal_rows(&f.db).await, before_count + 1);
        assert_eq!(
            f.row(&mover).await.semantic_revision,
            Some(version as i64 + 1)
        );
        assert_eq!(
            repo.expected_state(&f.owner, &mover).await.unwrap().scope,
            destination
        );
        assert_eq!(
            f.cluster_members(source_cluster).await,
            vec![key_of(source_survivor)]
        );
        assert_eq!(f.cluster_members(destination_cluster).await.len(), 2);
        let detail = repo.operation(&f.owner, key).await.unwrap().unwrap();
        let source_before = image(&detail.preimages, source_cluster);
        let mut expected_members = vec![
            mover.as_record_id().clone(),
            source_survivor.as_record_id().clone(),
        ];
        expected_members.sort();
        assert_eq!(source_before.members, expected_members);
        assert_eq!(source_before.centroid, vec![1.0, 0.0]);
        assert_eq!(
            source_before.membership_revision,
            source_guards.membership_revision
        );
        assert_eq!(
            source_before.member_embedding_revision,
            source_guards.member_embedding_revision
        );
        assert_eq!(
            source_before.centroid_revision,
            source_guards.centroid_revision
        );
        let source_after = image(&detail.postimages, source_cluster);
        assert_eq!(
            source_after.members,
            vec![source_survivor.as_record_id().clone()]
        );
        assert_eq!(
            source_after.centroid,
            f.cluster_row(source_cluster).await.centroid
        );
        assert_eq!(
            source_after.membership_revision.get(),
            source_guards.membership_revision.get() + 1
        );
        assert_eq!(
            source_after.centroid_revision.get(),
            source_guards.centroid_revision.get() + 1
        );
        let destination_before = image(&detail.preimages, destination_cluster);
        assert_eq!(destination_before.members.len(), 1);
        assert_eq!(destination_before.centroid, vec![1.0, 0.0]);
        assert_eq!(
            destination_before.membership_revision,
            destination_guards.membership_revision
        );
        let destination_after = image(&detail.postimages, destination_cluster);
        assert_eq!(destination_after.members.len(), 2);
        assert!(destination_after.members.contains(mover.as_record_id()));
        assert_eq!(
            destination_after.centroid,
            f.cluster_row(destination_cluster).await.centroid
        );
        assert_eq!(
            destination_after.membership_revision.get(),
            destination_guards.membership_revision.get() + 1
        );
    }
    let before = f.semantic_state().await;
    assert!(matches!(
        repo.move_scope(
            &f.owner,
            mover.clone(),
            Version::ZERO,
            MemoryScope::Personal,
            &op("stale-move")
        )
        .await,
        Err(StorageError::Stale)
    ));
    let b_guards = repo
        .cluster_expectation(&f.owner, &b_cluster)
        .await
        .unwrap();
    let invalid = repo
        .move_scope_reconciled(
            &f.owner,
            mover,
            Version::new(2),
            MemoryScope::Project(a),
            &op("same-cluster"),
            &MoveReconciliation {
                leaving: None,
                arriving: DestinationMembership::Join {
                    expectation: b_guards,
                    centroid: vec![1.0, 0.0],
                },
            },
        )
        .await;
    assert!(
        matches!(invalid, Err(StorageError::InvalidInput(_))),
        "same-cluster cross-scope move must refuse, not self-CAS: {invalid:?}"
    );
    assert_eq!(f.semantic_state().await, before);
}

fn key_of(fact: &FactId) -> String {
    key(fact.as_record_id())
}

#[tokio::test]
async fn destination_membership_race_refuses_even_when_the_cas_guards_are_unchanged() {
    let f = Fixture::enrolled("destination-race").await;
    let a = f.project("a").await;
    let member = seed_in(&f, WriteTarget::Project(a.clone())).await;
    let destination = cluster(&f, &[&member]).await;
    let mover = seed_in(&f, WriteTarget::Personal).await;
    let intruder = seed_in(&f, WriteTarget::Personal).await;
    let repo = MutationRepo::new(f.db.inner());
    let expected = repo
        .cluster_expectation(&f.owner, &destination)
        .await
        .unwrap();
    // An old/untracked writer inserts a wrong-scope member without moving the revision guards.
    f.attach(&destination, &intruder).await;
    let current = repo
        .cluster_expectation(&f.owner, &destination)
        .await
        .unwrap();
    assert_eq!(
        [
            current.membership_revision,
            current.member_embedding_revision,
            current.centroid_revision
        ],
        [
            expected.membership_revision,
            expected.member_embedding_revision,
            expected.centroid_revision
        ]
    );
    assert_ne!(current.population, expected.population);
    let before = f.semantic_state().await;
    let outcome = repo
        .move_scope_reconciled(
            &f.owner,
            mover.clone(),
            Version::ZERO,
            MemoryScope::Project(a),
            &op("race"),
            &MoveReconciliation {
                leaving: None,
                arriving: DestinationMembership::Join {
                    // Current mixed population makes this isolate purity, not population mismatch.
                    expectation: current,
                    centroid: vec![1.0, 0.0],
                },
            },
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
        "the transaction admitted a mixed partition: {outcome:?}"
    );
    assert_eq!(f.semantic_state().await, before);
    // Historical mixed membership is still safely detachable, preserving the unrelated project row.
    repo.move_scope(
        &f.owner,
        intruder,
        Version::ZERO,
        MemoryScope::Unassigned,
        &op("detach-legacy-mixed"),
    )
    .await
    .unwrap();
    assert_eq!(f.cluster_members(&destination).await, vec![key_of(&member)]);
    assert_eq!(f.cluster_row(&destination).await.centroid, vec![1.0, 0.0]);
}

async fn hide_member(f: &Fixture, fact: &FactId, visibility: Visibility) {
    let repo = MutationRepo::new(f.db.inner());
    let expected = repo.expected_state(&f.owner, fact).await.unwrap();
    let change = PreparedChangeSet::new(operation_kind::UPDATE)
        .effect(if visibility.deleted {
            effect::DELETE
        } else {
            effect::QUARANTINE
        })
        .fact(FactChange::Update {
            fact: fact.clone(),
            expected: expected.version,
            content: None,
            tags: None,
            confidence: None,
            embedding: None,
            visibility: Some(visibility),
            scope: None,
        });
    repo.apply(
        &MutationContext::issued_to(&f.owner),
        &op(&format!("hide-{}", key_of(fact))),
        &[expected],
        &change,
    )
    .await
    .expect("a real journaled visibility change");
}

#[tokio::test]
async fn review_destination_losing_its_last_eligible_member_cannot_be_adopted() {
    let f = Fixture::enrolled("empty-destination-race").await;
    let project = f.project("destination").await;
    let scope = MemoryScope::Project(project.clone());
    let member = seed_in(&f, WriteTarget::Project(project.clone())).await;
    let destination = cluster(&f, &[&member]).await;
    let mover = seed_in(&f, WriteTarget::Personal).await;
    let repo = MutationRepo::new(f.db.inner());
    assert_eq!(candidate_ids(&f, &scope).await, vec![destination.clone()]);
    let guards = repo
        .cluster_expectation(&f.owner, &destination)
        .await
        .unwrap();
    hide_member(&f, &member, Visibility::deleted()).await;
    let current = repo
        .cluster_expectation(&f.owner, &destination)
        .await
        .unwrap();
    assert_eq!(
        [
            current.membership_revision,
            current.member_embedding_revision,
            current.centroid_revision
        ],
        [
            guards.membership_revision,
            guards.member_embedding_revision,
            guards.centroid_revision
        ],
        "soft deletion leaves the three cluster CAS revisions unchanged"
    );
    assert!(candidate_ids(&f, &scope).await.is_empty());
    let before = f.semantic_state().await;
    // Use the current EMPTY population so this exercises admission, not the earlier population check.
    let reconciliation = MoveReconciliation {
        leaving: None,
        arriving: DestinationMembership::Join {
            expectation: current,
            centroid: vec![1.0, 0.0],
        },
    };
    let outcome = repo
        .move_scope_reconciled(
            &f.owner,
            mover.clone(),
            Version::ZERO,
            scope.clone(),
            &op("empty-race"),
            &reconciliation,
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
        "a destination with no eligible witness was adopted: {outcome:?}"
    );
    assert_eq!(
        f.semantic_state().await,
        before,
        "the refusal must roll back the whole move"
    );
    let replacement = seed_in(&f, WriteTarget::Project(project)).await;
    f.attach(&destination, &replacement).await;
    repo.move_scope_reconciled(
        &f.owner,
        mover.clone(),
        Version::ZERO,
        scope,
        &op("nonempty-control"),
        &MoveReconciliation {
            leaving: None,
            arriving: DestinationMembership::Join {
                expectation: repo
                    .cluster_expectation(&f.owner, &destination)
                    .await
                    .unwrap(),
                centroid: vec![1.0, 0.0],
            },
        },
    )
    .await
    .expect("a remaining eligible witness permits the same lawful join");
    assert!(
        f.cluster_members(&destination)
            .await
            .contains(&key_of(&mover))
    );
}

#[tokio::test]
async fn review_population_locked_point_read_detects_a_post_check_visibility_change() {
    for locked in [false, true] {
        let f = Fixture::enrolled(&format!("population-read-lock-{locked}")).await;
        let member = f.seed_vector("population read control", &[1.0, 0.0]).await;
        let group = cluster(&f, &[&member]).await;
        let tx = f.db.inner().clone().begin().await.unwrap();
        let sql = if locked {
            "SELECT id, embedding FROM $member FOR UPDATE;"
        } else {
            "SELECT id, embedding FROM $member;"
        };
        let mut read = tx
            .query(sql)
            .bind(("member", member.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<surrealdb::types::Value> = read.take(0).unwrap();
        assert_eq!(rows.len(), 1, "the fixture must actually read a member");
        f.db.inner()
            .query("UPDATE $member SET deleted = true;")
            .bind(("member", member.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        tx.query("UPDATE $cluster SET centroid = $centroid;")
            .bind(("cluster", group.clone()))
            .bind(("centroid", vec![0.5_f32, 0.5]))
            .await
            .unwrap()
            .check()
            .unwrap();
        let outcome = tx.commit().await;
        assert_eq!(outcome.is_err(), locked, "locked={locked}: {outcome:?}");
        if let Err(error) = outcome {
            assert!(
                matches!(
                    error.query_details(),
                    Some(surrealdb::types::QueryError::TransactionConflict)
                ),
                "locked commit must reject with a typed write conflict: {error:?}"
            );
            assert_eq!(f.cluster_row(&group).await.centroid, vec![1.0, 0.0]);
        } else {
            assert_eq!(f.cluster_row(&group).await.centroid, vec![0.5, 0.5]);
        }
    }
}

#[tokio::test]
async fn review_population_untracked_vector_activation_and_identity_changes_are_refused() {
    for kind in ["vector", "activation", "identity"] {
        let f = Fixture::enrolled(&format!("population-{kind}")).await;
        let project = f.project("destination").await;
        let scope = MemoryScope::Project(project.clone());
        let a = seed_in(&f, WriteTarget::Project(project.clone())).await;
        let b = seed_in(&f, WriteTarget::Project(project.clone())).await;
        f.db.inner()
            .query("UPDATE $fact SET embedding = $vector;")
            .bind(("fact", b.as_record_id().clone()))
            .bind(("vector", vec![0.0_f32, 1.0]))
            .await
            .unwrap()
            .check()
            .unwrap();
        if kind == "activation" {
            hide_member(&f, &b, Visibility::deleted()).await;
        }
        let destination = cluster(&f, &[&a, &b]).await;
        let mover = seed_in(&f, WriteTarget::Personal).await;
        let repo = MutationRepo::new(f.db.inner());
        let prepared = repo
            .cluster_expectation(&f.owner, &destination)
            .await
            .unwrap();
        let old_mean = if kind == "activation" {
            vec![1.0, 0.0]
        } else {
            vec![2.0 / 3.0, 1.0 / 3.0]
        };
        match kind {
            "vector" => {
                f.db.inner()
                    .query("UPDATE $fact SET embedding = $vector;")
                    .bind(("fact", b.as_record_id().clone()))
                    .bind(("vector", vec![0.0_f32, 2.0]))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
            "activation" => {
                f.db.inner()
                    .query("UPDATE $fact SET deleted = false;")
                    .bind(("fact", b.as_record_id().clone()))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
            "identity" => {
                let replacement = seed_in(&f, WriteTarget::Project(project)).await;
                f.db.inner().query("UPDATE $fact SET embedding = $vector; DELETE contains_memory WHERE in = $cluster AND out = $old;")
                    .bind(("fact", replacement.as_record_id().clone()))
                    .bind(("vector", vec![0.0_f32, 1.0]))
                    .bind(("cluster", destination.clone()))
                    .bind(("old", b.as_record_id().clone()))
                    .await.unwrap().check().unwrap();
                f.attach(&destination, &replacement).await;
            }
            _ => unreachable!(),
        }
        let current = repo
            .cluster_expectation(&f.owner, &destination)
            .await
            .unwrap();
        assert_eq!(
            [
                prepared.membership_revision,
                prepared.member_embedding_revision,
                prepared.centroid_revision
            ],
            [
                current.membership_revision,
                current.member_embedding_revision,
                current.centroid_revision
            ],
            "{kind}: legacy writes must leave the numeric guards unchanged"
        );
        assert_ne!(prepared.population, current.population, "{kind}");
        assert_eq!(candidate_ids(&f, &scope).await, vec![destination.clone()]);
        let before = f.semantic_state().await;
        let outcome = repo
            .move_scope_reconciled(
                &f.owner,
                mover.clone(),
                Version::ZERO,
                scope.clone(),
                &op("old-population"),
                &MoveReconciliation {
                    leaving: None,
                    arriving: DestinationMembership::Join {
                        expectation: prepared,
                        centroid: old_mean,
                    },
                },
            )
            .await;
        assert!(
            matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
            "{kind}: {outcome:?}"
        );
        assert_eq!(
            f.semantic_state().await,
            before,
            "{kind}: refusal must be atomic"
        );
        let fresh_mean = if kind == "vector" {
            vec![2.0 / 3.0, 2.0 / 3.0]
        } else {
            vec![2.0 / 3.0, 1.0 / 3.0]
        };
        repo.move_scope_reconciled(
            &f.owner,
            mover,
            Version::ZERO,
            scope,
            &op("fresh-population"),
            &MoveReconciliation {
                leaving: None,
                arriving: DestinationMembership::Join {
                    expectation: current,
                    centroid: fresh_mean.clone(),
                },
            },
        )
        .await
        .expect("refreshing the SAME named effect remains lawful");
        assert_eq!(
            f.cluster_row(&destination).await.centroid,
            fresh_mean,
            "{kind}"
        );
    }
}

#[tokio::test]
async fn review_population_source_survivor_change_refuses_the_prepared_mean() {
    let f = Fixture::enrolled("source-population-race").await;
    let mover = f.seed_vector("source mover", &[1.0, 0.0]).await;
    let b = f.seed_vector("source hidden later", &[0.0, 1.0]).await;
    let c = f.seed_vector("source live survivor", &[1.0, 0.0]).await;
    let source = cluster(&f, &[&mover, &b, &c]).await;
    let repo = MutationRepo::new(f.db.inner());
    let prepared = repo.cluster_expectation(&f.owner, &source).await.unwrap();
    hide_member(&f, &b, Visibility::deleted()).await;
    let current = repo.cluster_expectation(&f.owner, &source).await.unwrap();
    assert_eq!(
        [
            prepared.membership_revision,
            prepared.member_embedding_revision,
            prepared.centroid_revision
        ],
        [
            current.membership_revision,
            current.member_embedding_revision,
            current.centroid_revision
        ]
    );
    let before = f.semantic_state().await;
    let outcome = repo
        .move_scope_reconciled(
            &f.owner,
            mover.clone(),
            Version::ZERO,
            MemoryScope::Unassigned,
            &op("old-source-population"),
            &MoveReconciliation {
                leaving: Some(LeavingCluster {
                    expectation: prepared,
                    surviving_centroid: Some(vec![0.5, 0.5]),
                }),
                arriving: DestinationMembership::Unclustered,
            },
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
        "{outcome:?}"
    );
    assert_eq!(f.semantic_state().await, before);
    repo.move_scope_reconciled(
        &f.owner,
        mover,
        Version::ZERO,
        MemoryScope::Unassigned,
        &op("fresh-source-population"),
        &MoveReconciliation {
            leaving: Some(LeavingCluster {
                expectation: current,
                surviving_centroid: Some(vec![1.0, 0.0]),
            }),
            arriving: DestinationMembership::Unclustered,
        },
    )
    .await
    .expect("fresh SAME source effect remains lawful");
    assert_eq!(f.cluster_row(&source).await.centroid, vec![1.0, 0.0]);
    assert!(
        f.cluster_members(&source).await.contains(&key_of(&b)),
        "hidden structural endpoint remains"
    );
}

#[tokio::test]
async fn review_population_change_with_a_surviving_witness_refuses_the_prepared_mean() {
    let f = Fixture::enrolled("population-race-surviving-witness").await;
    let project = f.project("population-destination").await;
    let scope = MemoryScope::Project(project.clone());
    let a = seed_in(&f, WriteTarget::Project(project.clone())).await;
    let b = seed_in(&f, WriteTarget::Project(project)).await;
    f.db.inner()
        .query("UPDATE $fact SET embedding = $vector;")
        .bind(("fact", b.as_record_id().clone()))
        .bind(("vector", vec![0.0_f32, 1.0]))
        .await
        .unwrap()
        .check()
        .unwrap();
    let destination = cluster(&f, &[&a, &b]).await;
    let mover = seed_in(&f, WriteTarget::Personal).await;
    let repo = MutationRepo::new(f.db.inner());
    let prepared = repo
        .cluster_expectation(&f.owner, &destination)
        .await
        .unwrap();
    let reconciliation = MoveReconciliation {
        leaving: None,
        arriving: DestinationMembership::Join {
            expectation: prepared.clone(),
            centroid: vec![2.0 / 3.0, 1.0 / 3.0],
        },
    };
    hide_member(&f, &b, Visibility::deleted()).await;
    let current = repo
        .cluster_expectation(&f.owner, &destination)
        .await
        .unwrap();
    assert_eq!(
        [
            prepared.membership_revision,
            prepared.member_embedding_revision,
            prepared.centroid_revision
        ],
        [
            current.membership_revision,
            current.member_embedding_revision,
            current.centroid_revision
        ],
        "this race must not depend on a changed cluster revision"
    );
    assert_eq!(candidate_ids(&f, &scope).await, vec![destination.clone()]);
    let before = f.semantic_state().await;
    let outcome = repo
        .move_scope_reconciled(
            &f.owner,
            mover.clone(),
            Version::ZERO,
            scope.clone(),
            &op("population-race"),
            &reconciliation,
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
        "a surviving witness admitted a centroid containing a hidden vector: {outcome:?}"
    );
    assert_eq!(f.semantic_state().await, before, "refusal must be atomic");
    repo.move_scope_reconciled(
        &f.owner,
        mover,
        Version::ZERO,
        scope,
        &op("population-current-control"),
        &MoveReconciliation {
            leaving: None,
            arriving: DestinationMembership::Join {
                expectation: current,
                centroid: vec![1.0, 0.0],
            },
        },
    )
    .await
    .expect("fresh preparation of the SAME destination remains lawful");
    assert_eq!(f.cluster_row(&destination).await.centroid, vec![1.0, 0.0]);
}

#[tokio::test]
async fn review_cluster_images_include_retained_hidden_members_without_polluting_centroids() {
    let f = Fixture::enrolled("complete-private-membership").await;
    let mover = f.seed_vector("live mover", &[0.1, 0.2]).await;
    let stayer = f.seed_vector("live survivor", &[0.9, 0.2]).await;
    let deleted = f
        .seed_vector("deleted retained endpoint", &[50.0, 50.0])
        .await;
    let quarantined = f
        .seed_vector("quarantined retained endpoint", &[-50.0, -50.0])
        .await;
    hide_member(&f, &deleted, Visibility::deleted()).await;
    hide_member(&f, &quarantined, Visibility::quarantined(Utc::now())).await;
    let read = f.read().await;
    let cluster = ScopedClusterRepo::new(f.db.inner(), &f.owner, &read)
        .create(None, &[0.5, 0.2], Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    for member in [&mover, &stayer, &deleted, &quarantined] {
        f.attach(&cluster, member).await;
    }
    assert_eq!(
        f.cluster_members(&cluster).await.len(),
        4,
        "hidden memberships really remain"
    );
    let repo = MutationRepo::new(f.db.inner());
    let guards = repo.cluster_expectation(&f.owner, &cluster).await.unwrap();
    repo.move_scope(
        &f.owner,
        mover.clone(),
        Version::ZERO,
        MemoryScope::Unassigned,
        &op("complete-images"),
    )
    .await
    .unwrap();
    let detail = repo
        .operation(&f.owner, "complete-images")
        .await
        .unwrap()
        .unwrap();
    let before = image(&detail.preimages, &cluster);
    let after = image(&detail.postimages, &cluster);
    let mut all = vec![
        mover.as_record_id().clone(),
        stayer.as_record_id().clone(),
        deleted.as_record_id().clone(),
        quarantined.as_record_id().clone(),
    ];
    all.sort();
    assert_eq!(
        before.members, all,
        "private history must not drop hidden structural endpoints"
    );
    all.retain(|id| id != mover.as_record_id());
    assert_eq!(after.members, all);
    assert_eq!(before.centroid, vec![0.5, 0.2]);
    assert_eq!(
        after.centroid,
        vec![0.9, 0.2],
        "hidden vectors must stay out of live survivor arithmetic"
    );
    assert_eq!(f.cluster_row(&cluster).await.centroid, after.centroid);
    assert_eq!(before.membership_revision, guards.membership_revision);
    assert_eq!(
        after.membership_revision.get(),
        guards.membership_revision.get() + 1
    );
    assert_eq!(
        after.centroid_revision.get(),
        guards.centroid_revision.get() + 1
    );
    assert_eq!(
        after.member_embedding_revision,
        guards.member_embedding_revision
    );
}
