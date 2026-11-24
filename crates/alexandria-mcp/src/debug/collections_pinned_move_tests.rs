use super::super::test_support::{AuthenticatedTestApp, authenticated_on_server, test_server};
use crate::mutation::{ApplyService, ApprovedCluster, Plan};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::mutation::DestinationMembership;
use alexandria_storage::models::{
    ExpectedState, FactChange, FactId, FactSnapshot, MemoryScope, OperationId, Visibility, effect,
    operation_kind,
};
use alexandria_storage::repos::{
    ClassificationDestination, ClassificationRepo, MutationRepo, ScopedClusterRepo,
    ScopedMemoryRepo, WriteTarget,
};
use chrono::Utc;
use surrealdb::types::RecordId;

#[tokio::test]
async fn classification_repair_vector_moves_into_unassigned() {
    use alexandria_storage::access::AccessRepo;
    use alexandria_storage::models::ReadSelection;
    let app = authenticated_on_server(test_server().await).await;
    let (source, _) = population(
        &app,
        MemoryScope::Personal,
        "unassigned source witness",
        &[0.1, 0.2],
    )
    .await;
    for classification in [false, true] {
        let (id, id_t) = app
            .seed_personal(&format!("unassigned vector {classification}"))
            .await;
        app.add_cluster_member(&source, &id_t).await;
        app.db
            .inner()
            .query("UPDATE $id SET quarantined_at = $at;")
            .bind(("id", id_t.clone()))
            .bind(("at", Utc::now()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let original = snapshot(&app, &id_t).await;
        assert!(original.embedding.is_some());
        let before = ledger(&app).await;
        let (_, location, _) = if classification {
            let reference = ClassificationRepo::new(app.db.inner(), &app.owner)
                .propose(
                    &original.id,
                    original.version,
                    MemoryScope::Unassigned,
                    None,
                )
                .await
                .unwrap();
            let selected = format!("{}:{}", reference.id, reference.revision.get());
            app.post_form_located(
                "/debug/classification/decide",
                &[("decision", "approve"), ("selected", &selected)],
            )
            .await
        } else {
            app.post_form_located(
                "/debug/collections/move",
                &[
                    ("id", &id),
                    ("revision", &original.version.get().to_string()),
                    ("destination", "unassigned"),
                ],
            )
            .await
        };
        assert!(!location.contains("error="), "{location}");
        let final_state = snapshot(&app, &id_t).await;
        assert_eq!(final_state.id, original.id);
        assert_eq!(final_state.scope, MemoryScope::Unassigned);
        assert_eq!(final_state.visibility, original.visibility);
        assert_eq!(ledger(&app).await.0, before.0 + 1);
        assert!(
            MutationRepo::new(app.db.inner())
                .cluster_of(&app.owner, original.id.as_record_id())
                .await
                .unwrap()
                .is_none()
        );
    }
    let access = AccessRepo::new(&app.db);
    assert!(
        access
            .select(&app.owner, ReadSelection::One(MemoryScope::Unassigned))
            .await
            .is_err()
    );
    assert!(
        access
            .select(&app.owner, ReadSelection::UnassignedReview)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn classification_repair_route_two_approvals() {
    let app = authenticated_on_server(test_server().await).await;
    population(&app, MemoryScope::Personal, "approval witness", &[0.1, 0.2]).await;
    let mut citations = Vec::new();
    let mut facts = Vec::new();
    for content in ["route first arrival", "route second arrival"] {
        let (_id, id_t) = app.seed_quarantined_unassigned(content).await;
        let state = snapshot(&app, &id_t).await;
        let reference = ClassificationRepo::new(app.db.inner(), &app.owner)
            .propose(&state.id, state.version, MemoryScope::Personal, None)
            .await
            .unwrap();
        citations.push(format!("{}:{}", reference.id, reference.revision.get()));
        facts.push((id_t, state));
    }
    let before = ledger(&app).await;
    let (denied, _, _) = app
        .post_exact(
            "/debug/classification/decide",
            format!(
                "decision=approve&selected={}&selected={}",
                citations[0], citations[1]
            ),
        )
        .await;
    assert_eq!(denied, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(ledger(&app).await, before);
    let (status, location, _) = app
        .post_form_located(
            "/debug/classification/decide",
            &[
                ("decision", "approve"),
                ("selected", &citations[0]),
                ("selected", &citations[1]),
            ],
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    let (_, page) = app.get(&location).await;
    assert!(page.contains("2 moved"), "{page}");
    assert_eq!(ledger(&app).await.0, before.0 + 2);
    for (id, original) in facts {
        let final_state = snapshot(&app, &id).await;
        assert_eq!(final_state.scope, MemoryScope::Personal);
        assert_eq!(final_state.visibility, original.visibility);
    }
    assert!(
        ClassificationRepo::new(app.db.inner(), &app.owner)
            .list_pending()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn classification_repair_route_partial_message() {
    use alexandria_storage::repos::ScopeProposalRef;
    let app = authenticated_on_server(test_server().await).await;
    population(&app, MemoryScope::Personal, "partial witness", &[0.1, 0.2]).await;
    let repo = ClassificationRepo::new(app.db.inner(), &app.owner);
    let mut refs = Vec::new();
    for (index, content) in [
        "route stale",
        "route success",
        "route failure",
        "route unattempted",
    ]
    .into_iter()
    .enumerate()
    {
        let (_id, id_t) = app.seed_quarantined_unassigned(content).await;
        let state = snapshot(&app, &id_t).await;
        if index == 2 {
            app.db.inner().query("CREATE $id SET account = $account, workspace = $workspace, fact = $fact, \
                source_revision = 0, revision = 0, destination_kind = 'personal', state = 'pending';")
                .bind(("id", RecordId::new("classification_proposal", "repair_second")))
                .bind(("account", app.owner.account().as_record_id().clone()))
                .bind(("workspace", app.owner.workspace().as_record_id().clone()))
                .bind(("fact", state.id.as_record_id().clone())).await.unwrap().check().unwrap();
            refs.push(ScopeProposalRef {
                id: "classification_proposal:repair_second".into(),
                revision: state.version,
            });
        } else {
            refs.push(
                repo.propose(&state.id, state.version, MemoryScope::Personal, None)
                    .await
                    .unwrap(),
            );
        }
        if index == 0 {
            app.db
                .inner()
                .query("UPDATE $id SET semantic_revision = 1;")
                .bind(("id", state.id.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
    }
    app.db.inner().query("DEFINE EVENT OVERWRITE refuse_route_repair ON TABLE semantic_operation \
        WHEN $event = 'CREATE' THEN { IF string::ends_with($after.operation_key, ':classification_proposal:repair_second') \
        { THROW 'private repair engine detail'; }; };").await.unwrap().check().unwrap();
    let selected: Vec<String> = refs
        .iter()
        .map(|reference| format!("{}:{}", reference.id, reference.revision.get()))
        .collect();
    let before = ledger(&app).await;
    let (status, location, _) = app
        .post_form_located(
            "/debug/classification/decide",
            &[
                ("decision", "approve"),
                ("selected", &selected[0]),
                ("selected", &selected[1]),
                ("selected", &selected[2]),
                ("selected", &selected[3]),
            ],
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    let (_, page) = app.get(&location).await;
    for text in [
        "1 moved",
        "1 refused",
        "could not confirm one entry",
        "1 not attempted",
    ] {
        assert!(page.contains(text), "missing {text}: {page}");
    }
    assert!(!page.contains("0 moved") && !page.contains("private repair engine detail"));
    assert_eq!(ledger(&app).await.0, before.0 + 1);
    app.db
        .inner()
        .query("REMOVE EVENT IF EXISTS refuse_route_repair ON TABLE semantic_operation;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let (status, location, _) = app
        .post_form_located(
            "/debug/classification/decide",
            &[
                ("decision", "approve"),
                ("selected", &selected[2]),
                ("selected", &selected[3]),
            ],
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    let (_, page) = app.get(&location).await;
    assert!(page.contains("2 moved"));
    assert_eq!(ledger(&app).await.0, before.0 + 3);
}

#[tokio::test]
async fn classification_repair_ranking_failure_is_not_ungrouped() {
    let app = authenticated_on_server(test_server().await).await;
    let centroid = [0.1, 0.2];
    let (cluster, _) = population(&app, MemoryScope::Personal, "ranking witness", &centroid).await;
    let (_id, id_t) = app
        .seed_quarantined_unassigned("ranking failure subject")
        .await;
    let state = snapshot(&app, &id_t).await;
    let reference = ClassificationRepo::new(app.db.inner(), &app.owner)
        .propose(&state.id, state.version, MemoryScope::Personal, None)
        .await
        .unwrap();
    let selected = format!("{}:{}", reference.id, reference.revision.get());
    app.db.inner().query("DEFINE FIELD OVERWRITE centroid ON cluster TYPE any; UPDATE $cluster SET centroid = 'undecodable';")
        .bind(("cluster", cluster.clone())).await.unwrap().check().unwrap();
    let before = ledger(&app).await;
    let (_, location, _) = app
        .post_form_located(
            "/debug/classification/decide",
            &[("decision", "approve"), ("selected", &selected)],
        )
        .await;
    let (_, page) = app.get(&location).await;
    assert!(
        page.contains("0 moved") && page.contains("could not confirm one entry"),
        "{page}"
    );
    assert_eq!(ledger(&app).await, before);
    assert_eq!(snapshot(&app, &id_t).await, state);
    // Only the value needs restoring in this disposable fixture.
    app.db
        .inner()
        .query("UPDATE $cluster SET centroid = $centroid;")
        .bind(("cluster", cluster.clone()))
        .bind(("centroid", centroid.to_vec()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let (_, location, _) = app
        .post_form_located(
            "/debug/classification/decide",
            &[("decision", "approve"), ("selected", &selected)],
        )
        .await;
    let (_, page) = app.get(&location).await;
    assert!(page.contains("1 moved"), "{page}");
    assert_eq!(ledger(&app).await, (before.0 + 1, before.1));
    let destination = MutationRepo::new(app.db.inner())
        .cluster_of(&app.owner, state.id.as_record_id())
        .await
        .unwrap();
    assert_eq!(destination.map(|(id, _)| id), Some(cluster));
}

#[tokio::test]
async fn classification_review_typed_arrival_preserves_original_cluster_identity() {
    use alexandria_storage::access::AccessRepo;
    use alexandria_storage::models::ReadSelection;

    for key in ["plain-rank-target", "`literal`"] {
        let app = authenticated_on_server(test_server().await).await;
        let read = app.population().await;
        let source = ScopedMemoryRepo::new(app.db.inner(), &app.owner, &read)
            .create_fact(
                "typed destination source",
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
        let preferred = RecordId::new("cluster", key);
        let mut destinations = vec![(preferred.clone(), vec![1.0, 0.0])];
        if key == "`literal`" {
            destinations.push((RecordId::new("cluster", "literal"), vec![0.0, 1.0]));
        }
        let count = destinations.len();
        let mut preferred_member = None;
        for (id, vector) in destinations {
            let member = typed_personal_population(&app, &id, &vector).await;
            if id == preferred {
                preferred_member = Some(member);
            }
        }
        let read = AccessRepo::new(&app.db)
            .select(&app.owner, ReadSelection::One(MemoryScope::Personal))
            .await
            .unwrap();
        let admitted = ScopedClusterRepo::new(app.db.inner(), &app.owner, &read)
            .partition_candidates()
            .await
            .unwrap();
        assert_eq!(admitted.len(), count);
        assert!(
            admitted
                .iter()
                .any(|(row, _)| row.id.as_ref() == Some(&preferred))
        );
        let probe = MutationRepo::new(app.db.inner())
            .cluster_expectation(&app.owner, &preferred)
            .await
            .expect("the original typed candidate is readable before ranking");
        assert_eq!(probe.cluster, preferred);
        assert_eq!(probe.population.len(), 1);
        assert_eq!(probe.population[0].id, preferred_member.unwrap());

        let source = snapshot(&app, &source).await;
        let scope = app.corpus_scope().await;
        let before = ledger(&app).await;
        let arrival = app
            .server()
            .prepare_arrival(&scope, &source, &MemoryScope::Personal)
            .await
            .expect("a lawful admitted candidate must be selectable");
        match &arrival.arriving {
            DestinationMembership::Join { expectation, .. } => {
                assert_eq!(expectation.cluster, preferred)
            }
            other => panic!("expected the original typed destination, got {other:?}"),
        }
        assert_eq!(ledger(&app).await, before);
    }
}

async fn typed_personal_population(
    app: &AuthenticatedTestApp,
    cluster: &RecordId,
    vector: &[f32],
) -> RecordId {
    let read = app.population().await;
    let member = ScopedMemoryRepo::new(app.db.inner(), &app.owner, &read)
        .create_fact(
            "typed candidate witness",
            0.5,
            vector,
            &[],
            WriteTarget::Personal,
            Utc::now(),
        )
        .await
        .unwrap()
        .id
        .unwrap();
    // Use the scoped constructor's row shape but bind the original typed handle. The legacy fixture
    // helper reparses display strings and would lose the test's identity before ranking ever ran.
    app.db.inner().query(
        "CREATE $cluster SET label = 'typed identity candidate', centroid = $centroid, depth = 0, \
         account = $account, workspace = $workspace; RELATE $cluster->contains_memory->$member;",
    )
        .bind(("cluster", cluster.clone())).bind(("centroid", vector.to_vec()))
        .bind(("account", app.owner.account().as_record_id().clone()))
        .bind(("workspace", app.owner.workspace().as_record_id().clone()))
        .bind(("member", member.clone())).await.unwrap().check().unwrap();
    member
}

#[tokio::test]
async fn deletion_hidden_member_keeps_live_mean_and_historical_membership() {
    let app = authenticated_on_server(test_server().await).await;
    let (cluster, _) = population(
        &app,
        MemoryScope::Personal,
        "live delete witness",
        &[1.0, 0.0],
    )
    .await;
    let (hidden, hidden_t) = app
        .seed_quarantined_unassigned("hidden delete contribution")
        .await;
    app.add_cluster_member(&cluster, &hidden_t).await;
    let before = deletion_observation(&app).await;
    let (status, location, _) = app
        .post_form_located(
            "/debug/collections/edit",
            &[("id", hidden.as_str()), ("action", "delete")],
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER, "{location}");
    assert!(!location.contains("error="), "{location}");
    let after = deletion_observation(&app).await;
    let mut expected_facts = before[0].clone();
    let row = expected_facts
        .iter_mut()
        .find(|row| row["id"].as_str() == Some(hidden.as_str()))
        .unwrap();
    assert!(
        row["quarantined_at"].is_string(),
        "fixture must preserve a real quarantine timestamp"
    );
    row["deleted"] = serde_json::json!(true);
    row["semantic_revision"] = serde_json::json!(row["semantic_revision"].as_u64().unwrap() + 1);
    assert_eq!(
        after[0], expected_facts,
        "all non-deletion fact state and the live witness must survive"
    );
    assert_eq!(
        after[1], before[1],
        "a hidden contribution cannot rewrite any cluster field"
    );
    assert_eq!(after[2], before[2], "retain full historical membership");
    assert_eq!(
        &after[5..],
        &before[5..],
        "no edge, heat or proposal effects"
    );
    assert_eq!(after[3].len(), before[3].len() + 1);
    let operation = after[3].iter().find(|row| row["kind"] == "delete").unwrap();
    assert_eq!(
        operation["expected_effects"],
        serde_json::json!(["visibility:delete"])
    );
    assert_eq!(operation["actual_effects"], operation["expected_effects"]);
    assert_eq!(operation["preimages"].as_array().unwrap().len(), 1);
    assert_eq!(operation["postimages"].as_array().unwrap().len(), 1);
    assert_eq!(after[4].len(), before[4].len() + 2);
    assert!(
        after[4].iter().all(|row| row["kind"] == "fact"),
        "a guard must not manufacture cluster images"
    );
}

#[tokio::test]
async fn deletion_pins_centroid_effect_presence_in_both_population_drift_directions() {
    for initially_present in [false, true] {
        let app = authenticated_on_server(test_server().await).await;
        let (cluster, member) = population(
            &app,
            MemoryScope::Personal,
            "pinned delete member",
            &[1.0, 0.0],
        )
        .await;
        let read = app.population().await;
        let extra = ScopedMemoryRepo::new(app.db.inner(), &app.owner, &read)
            .create_fact(
                "delete population drift",
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
        if initially_present {
            app.add_cluster_member(&cluster, &extra).await;
        }
        let original = snapshot(&app, &member).await;
        let service = ApplyService::new(&app.db, &app.owner, &read);
        let approved = service
            .contribution_removal(original.id.as_record_id())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&approved, ApprovedCluster::RemoveContribution { contributes: true, writes_centroid, .. }
            if *writes_centroid == initially_present)
        );
        let plan = Plan::new(operation_kind::DELETE)
            .effect(effect::DELETE)
            .fact(FactChange::Update {
                fact: original.id.clone(),
                expected: original.version,
                content: None,
                tags: None,
                confidence: None,
                embedding: None,
                scope: None,
                visibility: Some(Visibility {
                    deleted: true,
                    quarantined_at: original.visibility.quarantined_at,
                }),
            })
            .cluster(approved);
        // Both changes deliberately leave all cluster counters untouched. Presence follows actual
        // eligibility, not a counter or whether the numerical mean happens to stay the same.
        if initially_present {
            app.db
                .inner()
                .query("UPDATE $member SET deleted = true;")
                .bind(("member", extra))
                .await
                .unwrap()
                .check()
                .unwrap();
        } else {
            app.add_cluster_member(&cluster, &extra).await;
        }
        let before = deletion_observation(&app).await;
        let result = service
            .commit(
                &OperationId::new("pinned-delete-drift"),
                &[ExpectedState::of(&original)],
                &plan,
            )
            .await;
        assert!(
            matches!(result, Err(StorageError::RetryableBookkeepingConflict)),
            "initial centroid presence={initially_present} must not be widened/reduced: {result:?}"
        );
        assert_eq!(
            deletion_observation(&app).await,
            before,
            "a changed effect shape must commit nothing"
        );
    }
}

async fn deletion_observation(app: &AuthenticatedTestApp) -> Vec<Vec<serde_json::Value>> {
    let mut response = app
        .db
        .inner()
        .query(
            "SELECT * FROM fact ORDER BY id; SELECT * FROM cluster ORDER BY id; \
         SELECT * FROM contains_memory ORDER BY id; SELECT * FROM semantic_operation ORDER BY id; \
         SELECT * FROM semantic_snapshot ORDER BY id; SELECT * FROM memory_edge ORDER BY id; \
         SELECT * FROM heat_state ORDER BY id; SELECT * FROM classification_proposal ORDER BY id;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    (0..8).map(|index| response.take(index).unwrap()).collect()
}

async fn population(
    app: &AuthenticatedTestApp,
    scope: MemoryScope,
    label: &str,
    vector: &[f32],
) -> (surrealdb::types::RecordId, surrealdb::types::RecordId) {
    let read = app.population().await;
    let target = match scope {
        MemoryScope::Personal => WriteTarget::Personal,
        MemoryScope::Project(project) => WriteTarget::Project(project),
        MemoryScope::Unassigned => panic!("this fixture requires an assigned partition"),
    };
    let fact = ScopedMemoryRepo::new(app.db.inner(), &app.owner, &read)
        .create_fact(label, 0.5, vector, &[], target, Utc::now())
        .await
        .unwrap()
        .id
        .unwrap();
    let cluster = app.seed_cluster(Some(label), vector).await;
    app.add_cluster_member(&cluster, &fact).await;
    (cluster, fact)
}

async fn snapshot(app: &AuthenticatedTestApp, id: &surrealdb::types::RecordId) -> FactSnapshot {
    MutationRepo::new(app.db.inner())
        .state(&app.owner, &FactId::try_from(id.clone()).unwrap())
        .await
        .unwrap()
        .unwrap()
}

async fn ledger(app: &AuthenticatedTestApp) -> (usize, usize) {
    let mut response = app
        .db
        .inner()
        .query("SELECT id FROM semantic_operation; SELECT id FROM cluster;")
        .await
        .unwrap()
        .check()
        .unwrap();
    let operations: Vec<serde_json::Value> = response.take(0).unwrap();
    let clusters: Vec<serde_json::Value> = response.take(1).unwrap();
    (operations.len(), clusters.len())
}

#[tokio::test]
async fn pinned_move_refreshes_guards_and_centroids_without_selecting_a_new_winner() {
    let app = authenticated_on_server(test_server().await).await;
    let project_id = app.own_project_id("pinned-project").await;
    let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
    let (original, _) = population(
        &app,
        destination.clone(),
        "original destination",
        &[0.2, 0.1],
    )
    .await;
    let (source, _) = population(&app, MemoryScope::Personal, "source survivor", &[0.1, 0.2]).await;
    let (_mover, mover_t) = app.seed_personal("pinned mover").await;
    app.add_cluster_member(&source, &mover_t).await;
    let existing = snapshot(&app, &mover_t).await;
    let scope = app.corpus_scope().await;
    let server = app.server();
    let plan = server
        .prepare_move_plan(&scope, &existing, &destination)
        .await
        .unwrap();
    let read = app.population().await;
    let source_id = source.clone();
    let repo = MutationRepo::new(app.db.inner());
    let before_guard = repo
        .cluster_expectation(&app.owner, &source_id)
        .await
        .unwrap();
    let original_id = original.clone();
    let destination_guard = repo
        .cluster_expectation(&app.owner, &original_id)
        .await
        .unwrap();
    // Real unrelated journaled writers move both clusters' guards and means after pinning.
    for (cluster, scope, key) in [
        (source_id.clone(), MemoryScope::Personal, "later-source"),
        (
            original_id.clone(),
            destination.clone(),
            "later-destination",
        ),
    ] {
        let add = Plan::new(operation_kind::STORE)
            .effect(effect::STORE)
            .effect(effect::VECTOR)
            .fact(FactChange::Create {
                scope,
                content: key.into(),
                tags: vec![],
                confidence: 0.5,
                embedding: alexandria_storage::models::EmbeddingSnapshot::new(vec![0.2, 0.2], None),
                visibility: Visibility::live(),
                metadata: None,
            })
            .embedding(&[0.2, 0.2])
            .cluster(ApprovedCluster::Join { cluster, member: 0 });
        ApplyService::new(app.db.as_ref(), &app.owner, &read)
            .commit(&OperationId::new(key), &[], &add)
            .await
            .unwrap();
    }
    assert_ne!(
        repo.cluster_expectation(&app.owner, &original_id)
            .await
            .unwrap()
            .membership_revision,
        destination_guard.membership_revision
    );
    assert_ne!(
        repo.cluster_expectation(&app.owner, &source_id)
            .await
            .unwrap()
            .membership_revision,
        before_guard.membership_revision
    );
    let (competitor, _) = population(
        &app,
        destination.clone(),
        "new nearer destination",
        &[0.1, 0.2],
    )
    .await;
    let fresh = server
        .prepare_arrival(&scope, &existing, &destination)
        .await
        .unwrap();
    assert!(
        matches!(fresh.arriving, DestinationMembership::Join { expectation, .. }
        if expectation.cluster == competitor.clone()),
        "the new ranking must actually differ"
    );
    ApplyService::new(app.db.as_ref(), &app.owner, &read)
        .commit(
            &OperationId::new("pinned-move"),
            &[ExpectedState::of(&existing)],
            &plan,
        )
        .await
        .unwrap();
    assert_eq!(
        repo.cluster_of(&app.owner, existing.id.as_record_id())
            .await
            .unwrap()
            .unwrap()
            .0,
        original.clone(),
        "refresh cannot change the approved destination"
    );
    let source = ScopedClusterRepo::new(app.db.inner(), &app.owner, &read)
        .get(&alexandria_storage::record_id_to_string(&source))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        source.centroid,
        vec![0.15, 0.2],
        "the source mean must be refreshed, not frozen with its guards"
    );
    let destination = ScopedClusterRepo::new(app.db.inner(), &app.owner, &read)
        .get(&alexandria_storage::record_id_to_string(&original))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        destination.centroid,
        vec![0.166_666_67, 0.166_666_67],
        "the destination mean must also reflect the later member"
    );
}

#[tokio::test]
async fn unlawful_pinned_destination_refuses_instead_of_reranking_or_creating() {
    for change in ["empty", "mixed"] {
        let app = authenticated_on_server(test_server().await).await;
        let project_id = app.own_project_id("pinned-project").await;
        let project = alexandria_storage::record_id_to_string(&project_id);
        let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
        let (original, member) = population(
            &app,
            destination.clone(),
            "original destination",
            &[0.2, 0.1],
        )
        .await;
        let (mover, mover_t) = app.seed_personal("pinned mover").await;
        let existing = snapshot(&app, &mover_t).await;
        let scope = app.corpus_scope().await;
        let server = app.server();
        let plan = server
            .prepare_move_plan(&scope, &existing, &destination)
            .await
            .unwrap();
        if change == "empty" {
            // The earlier real-storage regression establishes this tracked-delete state and unchanged guards.
            app.db
                .inner()
                .query("UPDATE $member SET deleted = true;")
                .bind(("member", member.clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        } else {
            let (_intruder, intruder_t) = app.seed_personal("wrong partition intruder").await;
            app.add_cluster_member(&original, &intruder_t).await;
        }
        let (competitor, _) =
            population(&app, destination.clone(), "lawful alternative", &[0.1, 0.2]).await;
        let fresh = server
            .prepare_arrival(&scope, &existing, &destination)
            .await
            .unwrap();
        assert!(
            matches!(fresh.arriving, DestinationMembership::Join { expectation, .. }
            if expectation.cluster == competitor.clone())
        );
        let before = ledger(&app).await;
        let read = app.population().await;
        let outcome = ApplyService::new(app.db.as_ref(), &app.owner, &read)
            .commit(
                &OperationId::new("unlawful-pinned"),
                &[ExpectedState::of(&existing)],
                &plan,
            )
            .await;
        assert!(
            matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
            "{change}: {outcome:?}"
        );
        assert_eq!(
            ledger(&app).await,
            before,
            "a refusal must not create a fallback cluster or journal row"
        );
        assert_eq!(snapshot(&app, &mover_t).await, existing);
        // A new owner action is allowed to choose the now-lawful alternative.
        server
            .move_within(&scope, &mover, "project", Some(&project), existing.version)
            .await
            .unwrap();
        assert_eq!(
            MutationRepo::new(app.db.inner())
                .cluster_of(&app.owner, existing.id.as_record_id())
                .await
                .unwrap()
                .unwrap()
                .0,
            competitor.clone()
        );
    }
}

#[tokio::test]
async fn pinned_source_absence_does_not_adopt_a_new_membership_at_the_same_fact_revision() {
    let app = authenticated_on_server(test_server().await).await;
    let project_id = app.own_project_id("pinned-project").await;
    let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
    population(&app, destination.clone(), "destination", &[0.1, 0.2]).await;
    let (_mover, mover_t) = app.seed_personal("initially unclustered mover").await;
    let existing = snapshot(&app, &mover_t).await;
    let scope = app.corpus_scope().await;
    let plan = app
        .server()
        .prepare_move_plan(&scope, &existing, &destination)
        .await
        .unwrap();
    let (source, _) = population(&app, MemoryScope::Personal, "new source", &[0.1, 0.2]).await;
    app.add_cluster_member(&source, &mover_t).await;
    assert_eq!(
        app.semantic_revision(&mover_t).await,
        existing.version.get()
    );
    let before = ledger(&app).await;
    let read = app.population().await;
    let outcome = ApplyService::new(app.db.as_ref(), &app.owner, &read)
        .commit(
            &OperationId::new("new-source-refusal"),
            &[ExpectedState::of(&existing)],
            &plan,
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "the original incident-graph expectation must remain fixed: {outcome:?}"
    );
    assert_eq!(ledger(&app).await, before);
    assert_eq!(snapshot(&app, &mover_t).await.scope, MemoryScope::Personal);
}

#[tokio::test]
async fn pinned_creation_does_not_switch_to_a_later_join() {
    let app = authenticated_on_server(test_server().await).await;
    let project_id = app.own_project_id("pinned-project").await;
    let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
    let (_mover, mover_t) = app
        .seed_personal("mover with initially empty destination")
        .await;
    let source = app
        .seed_cluster(Some("single-member source"), &[0.1, 0.2])
        .await;
    app.add_cluster_member(&source, &mover_t).await;
    let source_id = source.clone();
    let repo = MutationRepo::new(app.db.inner());
    let source_before = repo
        .cluster_expectation(&app.owner, &source_id)
        .await
        .unwrap();
    let existing = snapshot(&app, &mover_t).await;
    let scope = app.corpus_scope().await;
    let plan = app
        .server()
        .prepare_move_plan(&scope, &existing, &destination)
        .await
        .unwrap();
    let (later, _) = population(&app, destination, "later candidate", &[0.1, 0.2]).await;
    let before = ledger(&app).await;
    let read = app.population().await;
    ApplyService::new(app.db.as_ref(), &app.owner, &read)
        .commit(
            &OperationId::new("pinned-create"),
            &[ExpectedState::of(&existing)],
            &plan,
        )
        .await
        .unwrap();
    let actual = MutationRepo::new(app.db.inner())
        .cluster_of(&app.owner, existing.id.as_record_id())
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_ne!(
        actual,
        later.clone(),
        "Create and Join are different approved effects"
    );
    assert_eq!(ledger(&app).await, (before.0 + 1, before.1 + 1));
    let source_after = repo
        .cluster_expectation(&app.owner, &source_id)
        .await
        .unwrap();
    assert_eq!(
        source_after.membership_revision.get(),
        source_before.membership_revision.get() + 1
    );
    assert_eq!(
        source_after.centroid_revision, source_before.centroid_revision,
        "an empty source has no new centroid to write"
    );
}

#[tokio::test]
async fn pinned_move_refuses_same_cluster_without_banning_unclustered_quarantined_rows() {
    let app = authenticated_on_server(test_server().await).await;
    let project_id = app.own_project_id("pinned-project").await;
    let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
    let (cluster, _) = population(
        &app,
        destination.clone(),
        "live project witness",
        &[0.1, 0.2],
    )
    .await;
    let (_mover, mover_t) = app
        .seed_quarantined_unassigned("historical hidden member")
        .await;
    app.add_cluster_member(&cluster, &mover_t).await;
    let existing = snapshot(&app, &mover_t).await;
    let scope = app.corpus_scope().await;
    let before = ledger(&app).await;
    let outcome = app
        .server()
        .prepare_move_plan(&scope, &existing, &destination)
        .await;
    assert!(
        outcome.is_err(),
        "the source and arrival cannot name the same cluster across scopes"
    );
    assert_eq!(ledger(&app).await, before);
    let (_unclustered, unclustered_t) = app
        .seed_quarantined_unassigned("quarantine remains movable")
        .await;
    let existing = snapshot(&app, &unclustered_t).await;
    let plan = app
        .server()
        .prepare_move_plan(&scope, &existing, &destination)
        .await
        .unwrap();
    let read = app.population().await;
    ApplyService::new(app.db.as_ref(), &app.owner, &read)
        .commit(
            &OperationId::new("quarantine-positive"),
            &[ExpectedState::of(&existing)],
            &plan,
        )
        .await
        .unwrap();
    let after = snapshot(&app, &unclustered_t).await;
    assert_eq!(after.scope, destination);
    assert_eq!(
        after.visibility, existing.visibility,
        "filing must not clear quarantine"
    );
}

#[tokio::test]
async fn pinned_source_centroid_effect_cannot_appear_or_disappear_during_refresh() {
    for change in ["appeared", "disappeared"] {
        let app = authenticated_on_server(test_server().await).await;
        let project_id = app.own_project_id("pinned-project").await;
        let project = alexandria_storage::record_id_to_string(&project_id);
        let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
        let (mover, mover_t) = app.seed_personal("mover with a pinned source effect").await;
        let source = app.seed_cluster(Some("source"), &[0.1, 0.2]).await;
        app.add_cluster_member(&source, &mover_t).await;
        let (_survivor, survivor_t) = app.seed_personal("source survivor").await;
        if change == "disappeared" {
            app.add_cluster_member(&source, &survivor_t).await;
        }
        let existing = snapshot(&app, &mover_t).await;
        let scope = app.corpus_scope().await;
        let server = app.server();
        let plan = server
            .prepare_move_plan(&scope, &existing, &destination)
            .await
            .unwrap();
        if change == "appeared" {
            app.add_cluster_member(&source, &survivor_t).await;
        } else {
            app.db
                .inner()
                .query("UPDATE $member SET deleted = true;")
                .bind(("member", survivor_t.clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let before = ledger(&app).await;
        let read = app.population().await;
        let outcome = ApplyService::new(app.db.as_ref(), &app.owner, &read)
            .commit(
                &OperationId::new("changed-source-effect"),
                &[ExpectedState::of(&existing)],
                &plan,
            )
            .await;
        assert!(
            matches!(outcome, Err(StorageError::RetryableBookkeepingConflict)),
            "{change}: a refresh must not change whether a centroid write was approved: {outcome:?}"
        );
        assert_eq!(ledger(&app).await, before);
        assert_eq!(snapshot(&app, &mover_t).await, existing);
        server
            .move_within(&scope, &mover, "project", Some(&project), existing.version)
            .await
            .unwrap();
        assert!(matches!(
            snapshot(&app, &mover_t).await.scope,
            MemoryScope::Project(_)
        ));
    }
}

#[tokio::test]
async fn quarantined_changes_do_not_enter_live_centroid_arithmetic() {
    let mut centers = Vec::new();
    for path in ["move", "classification", "edit"] {
        let app = authenticated_on_server(test_server().await).await;
        let project_id = app.own_project_id("eligible-project").await;
        let destination = MemoryScope::Project(project_id.clone().try_into().unwrap());
        let (cluster, _) =
            population(&app, destination.clone(), "one live vector", &[1.0, 0.5]).await;
        let (id, id_t) = app
            .seed_quarantined_unassigned("inactive off-axis vector")
            .await;
        app.db
            .inner()
            .query("UPDATE $fact SET embedding = [1.0, 0.0];")
            .bind(("fact", id_t.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        if path == "edit" {
            assert!(
                app.move_row(&id_t, destination.clone(), "fixture-assigned-quarantine")
                    .await
            );
            app.add_cluster_member(&cluster, &id_t).await;
        }
        let existing = snapshot(&app, &id_t).await;
        assert!(existing.visibility.quarantined_at.is_some());
        let scope = app.corpus_scope().await;
        let server = app.server();
        match path {
            "move" => {
                let plan = server
                    .prepare_move_plan(&scope, &existing, &destination)
                    .await
                    .unwrap();
                let read = app.population().await;
                ApplyService::new(app.db.as_ref(), &app.owner, &read)
                    .commit(
                        &OperationId::new("hidden-move"),
                        &[ExpectedState::of(&existing)],
                        &plan,
                    )
                    .await
                    .unwrap();
            }
            "classification" => {
                let classify = ClassificationRepo::new(app.db.inner(), &app.owner);
                let proposal = classify
                    .propose(&existing.id, existing.version, destination.clone(), None)
                    .await
                    .unwrap();
                let candidates = classify.prepare_batch(&[proposal]).await.unwrap();
                let mut destinations = Vec::new();
                for candidate in candidates.candidates() {
                    let arriving = server
                        .prepare_arrival(&scope, candidate.fact(), candidate.destination())
                        .await
                        .unwrap()
                        .arriving;
                    destinations.push(ClassificationDestination::Chosen {
                        reference: candidate.reference().clone(),
                        arriving,
                    });
                }
                let prepared = candidates.seal(&destinations).unwrap();
                let result = classify
                    .apply_prepared_batch(&prepared, &OperationId::new("hidden-classification"))
                    .await
                    .unwrap();
                assert_eq!(
                    result.applied_count(),
                    1,
                    "classification must remain available: {result:?}"
                );
            }
            "edit" => {
                let params = serde_json::from_value(serde_json::json!({
                    "id": id, "content": "edited inactive memory",
                }))
                .unwrap();
                server
                    .update_within(&scope, params)
                    .await
                    .expect("assigned quarantined memory remains editable");
            }
            _ => unreachable!(),
        }
        let after = snapshot(&app, &id_t).await;
        assert_eq!(after.scope, destination);
        assert_eq!(
            after.visibility, existing.visibility,
            "{path} cannot clear quarantine"
        );
        assert_eq!(
            MutationRepo::new(app.db.inner())
                .cluster_of(&app.owner, after.id.as_record_id())
                .await
                .unwrap()
                .unwrap()
                .0,
            cluster.clone(),
            "this must exercise the existing cluster path"
        );
        let read = app.population().await;
        let after_cluster = ScopedClusterRepo::new(app.db.inner(), &app.owner, &read)
            .get(&alexandria_storage::record_id_to_string(&cluster))
            .await
            .unwrap()
            .unwrap();
        centers.push((path, after_cluster.centroid));
    }
    assert_eq!(
        centers,
        vec![
            ("move", vec![1.0, 0.5]),
            ("classification", vec![1.0, 0.5]),
            ("edit", vec![1.0, 0.5]),
        ],
        "only the live eligible vector may contribute; hidden mutations must still succeed"
    );
}
