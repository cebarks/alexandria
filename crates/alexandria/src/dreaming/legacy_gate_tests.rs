use super::*;
use alexandria_storage::Database;
use alexandria_storage::models::auth::PrincipalIds;
use alexandria_storage::models::identity::OperationId;
use alexandria_storage::models::{Cluster, Fact};
use alexandria_storage::repos::IdentityRepo;
use serde_json::Value;

async fn seed_cluster(
    db: &Database,
    label: &str,
    points: &[[f32; 2]],
    owner: Option<&PrincipalIds>,
) -> (Cluster, Vec<Fact>) {
    let clusters = ClusterRepo::new(db.inner());
    let memories = MemoryRepo::new(db.inner());
    let handle = clusters.create(Some(label), &points[0]).await.unwrap();
    let cluster = clusters.get(&handle).await.unwrap().unwrap();
    let mut facts = Vec::new();
    for (i, point) in points.iter().enumerate() {
        let fact_handle = memories
            .create_fact(&format!("{label}-{i}"), 0.5, point, &[])
            .await
            .unwrap();
        clusters.add_member(&handle, &fact_handle).await.unwrap();
        let fact = memories.get_fact(&fact_handle).await.unwrap().unwrap();
        if let Some(owner) = owner {
            db.inner().query("UPDATE $fact SET account = $account, workspace = $workspace, scope = 'personal';")
                .bind(("fact", fact.id.clone().unwrap())).bind(("account", owner.account.as_record_id().clone()))
                .bind(("workspace", owner.workspace.as_record_id().clone())).await.unwrap().check().unwrap();
        }
        facts.push(fact);
    }
    if let Some(owner) = owner {
        db.inner()
            .query("UPDATE $cluster SET workspace = $workspace;")
            .bind(("cluster", cluster.id.clone().unwrap()))
            .bind(("workspace", owner.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }
    (cluster, facts)
}

async fn protected_state(db: &Database, clusters: &[Cluster], facts: &[Fact]) -> Vec<Vec<Value>> {
    let mut response = db
        .inner()
        .query(
            "SELECT * FROM cluster WHERE id IN $clusters ORDER BY id; \
         SELECT * FROM fact WHERE id IN $facts ORDER BY id; \
         SELECT * FROM contains_memory WHERE in IN $clusters ORDER BY id;",
        )
        .bind((
            "clusters",
            clusters
                .iter()
                .map(|row| row.id.clone().unwrap())
                .collect::<Vec<_>>(),
        ))
        .bind((
            "facts",
            facts
                .iter()
                .map(|row| row.id.clone().unwrap())
                .collect::<Vec<_>>(),
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    (0..3).map(|index| response.take(index).unwrap()).collect()
}

#[tokio::test]
async fn temporary_legacy_maintenance_skips_owned_and_runs_unowned() {
    for job in [Job::Cluster, Job::Merge] {
        let db = Arc::new(Database::connect_embedded().await.unwrap());
        alexandria_storage::schema::migrate(db.inner())
            .await
            .unwrap();
        let owner = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "legacy-job-gate",
                "$argon2id$fixture-job-gate-padding",
                &OperationId::new("bootstrap"),
            )
            .await
            .unwrap();
        let split = matches!(job, Job::Cluster);
        let points = if split {
            vec![[1.0, 0.0], [1.0, 0.0], [0.0, 1.0], [0.0, 1.0]]
        } else {
            vec![[1.0, 0.0]]
        };
        let mut protected_clusters = Vec::new();
        let mut protected_facts = Vec::new();
        for label in ["owned-a", "owned-b"] {
            let (cluster, facts) = seed_cluster(&db, label, &points, Some(&owner)).await;
            protected_clusters.push(cluster);
            protected_facts.extend(facts);
        }
        let mixed_points = if split {
            points.clone()
        } else {
            vec![[-1.0, 0.0]]
        };
        let (mixed, mixed_facts) = seed_cluster(&db, "mixed-hidden", &mixed_points, None).await;
        let memories = MemoryRepo::new(db.inner());
        let hidden_handle = memories
            .create_fact("owned hidden witness", 0.5, &[1.0, 0.0], &[])
            .await
            .unwrap();
        let hidden = memories.get_fact(&hidden_handle).await.unwrap().unwrap();
        db.inner().query("UPDATE $fact SET account = $account, workspace = $workspace, scope = 'personal', deleted = true;")
            .bind(("fact", hidden.id.clone().unwrap())).bind(("account", owner.account.as_record_id().clone()))
            .bind(("workspace", owner.workspace.as_record_id().clone())).await.unwrap().check().unwrap();
        ClusterRepo::new(db.inner())
            .add_member(
                &record_id_to_string(mixed.id.as_ref().unwrap()),
                &hidden_handle,
            )
            .await
            .unwrap();
        protected_clusters.push(mixed);
        protected_facts.extend(mixed_facts);
        protected_facts.push(hidden);
        if split {
            seed_cluster(&db, "useful legacy split", &points, None).await;
        } else {
            seed_cluster(&db, "useful legacy merge-a", &[[0.0, 1.0]], None).await;
            seed_cluster(&db, "useful legacy merge-b", &[[0.0, 1.0]], None).await;
        }
        let before = protected_state(&db, &protected_clusters, &protected_facts).await;
        let jobs = Jobs::new(db.clone(), &Config::default());
        let report = jobs.run_job(job, "run-temporary-gate").await.unwrap();
        assert_eq!(
            protected_state(&db, &protected_clusters, &protected_facts).await,
            before,
            "owned and mixed retained data must not be split or merged"
        );
        assert_eq!(
            report.acted, 1,
            "the eligible unowned control must really act, not be starved"
        );
        let candidates = jobs.all_clusters().await.unwrap();
        assert!(
            candidates.iter().all(|row| !protected_clusters
                .iter()
                .any(|protected| protected.id == row.id)),
            "candidate enumeration must skip owned and mixed clusters too"
        );
    }
}
