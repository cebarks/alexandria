//! Integration tests for cluster split and merge mechanics.
//!
//! These test the `execute_split` and `execute_merge` methods on `ClusterRepo`,
//! which are the same methods the maintenance loop calls.

use alexandria_engine::clusters::maintenance::{
    MaintenanceAction, MergeCheck, check_cohesion, check_merge,
};
use alexandria_storage::record_id_to_string;
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{Database, schema};

#[tokio::test]
async fn test_cluster_split_mechanics() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = MemoryRepo::new(db.inner());

    // Create a cluster with 6 members from two distinct groups
    let cid = cluster_repo
        .create(Some("mixed"), &[0.5, 0.5, 0.0])
        .await
        .unwrap();
    let embeddings = [
        vec![1.0, 0.0, 0.0],
        vec![0.95, 0.05, 0.0],
        vec![0.98, 0.02, 0.0],
        vec![0.0, 1.0, 0.0],
        vec![0.05, 0.95, 0.0],
        vec![0.02, 0.98, 0.0],
    ];
    for (i, emb) in embeddings.iter().enumerate() {
        let (_fid, fid_t) = memory_repo
            .create_fact_with_id(&format!("fact{i}"), 0.5, emb, &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &fid_t).await.unwrap();
    }

    // Verify cohesion check says split
    let members = cluster_repo.get_members(&cid).await.unwrap();
    let member_embeddings: Vec<Vec<f32>> = members.iter().map(|f| f.embedding.clone()).collect();
    let action = check_cohesion(
        &alexandria_storage::record_id_to_string(&cid),
        &[0.5, 0.5, 0.0],
        &member_embeddings,
        0.85,
    );
    let (group_a, group_b, centroid_a, centroid_b) = match action {
        MaintenanceAction::Split {
            group_a,
            group_b,
            centroid_a,
            centroid_b,
            ..
        } => (group_a, group_b, centroid_a, centroid_b),
        MaintenanceAction::Healthy => panic!("Expected split"),
    };

    // Execute split via the extracted method
    let (cid_a, cid_b) = cluster_repo
        .execute_split(&cid, &members, &group_a, &group_b, &centroid_a, &centroid_b)
        .await
        .unwrap();

    // Verify: old cluster gone, two new ones exist, all facts accounted for
    let all = cluster_repo.list_with_counts().await.unwrap();
    assert_eq!(all.len(), 2);
    let total_members: usize = all.iter().map(|(_, c)| c).sum();
    assert_eq!(total_members, 6);

    // Verify each new cluster has members
    let a_members = cluster_repo.get_members(&cid_a).await.unwrap();
    let b_members = cluster_repo.get_members(&cid_b).await.unwrap();
    assert_eq!(a_members.len(), group_a.len());
    assert_eq!(b_members.len(), group_b.len());
}

#[tokio::test]
async fn test_cluster_merge_mechanics() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = MemoryRepo::new(db.inner());

    // Create two very similar clusters
    let c1 = cluster_repo
        .create(Some("c1"), &[1.0, 0.0, 0.0])
        .await
        .unwrap();
    let c2 = cluster_repo
        .create(Some("c2"), &[0.98, 0.02, 0.0])
        .await
        .unwrap();

    let (_f1, f1_t) = memory_repo
        .create_fact_with_id("in c1", 0.5, &[1.0, 0.0, 0.0], &[])
        .await
        .unwrap();
    let (_f2, f2_t) = memory_repo
        .create_fact_with_id("in c2", 0.5, &[0.98, 0.02, 0.0], &[])
        .await
        .unwrap();
    cluster_repo.add_member(&c1, &f1_t).await.unwrap();
    cluster_repo.add_member(&c2, &f2_t).await.unwrap();

    // check_merge says merge (equal counts, c1 kept by convention)
    let merge = check_merge(
        &alexandria_storage::record_id_to_string(&c1),
        &[1.0, 0.0, 0.0],
        1,
        &alexandria_storage::record_id_to_string(&c2),
        &[0.98, 0.02, 0.0],
        1,
        0.9,
    );
    let (keep_id, _remove_id, merged_centroid) = match merge {
        MergeCheck::Merge {
            keep_id,
            remove_id,
            merged_centroid,
        } => (keep_id, remove_id, merged_centroid),
        MergeCheck::Distinct => panic!("Expected merge"),
    };

    // Execute merge via the extracted method: the engine's verdict labels are display strings,
    // the typed ids the fixture minted are the authority.
    let (keep, remove) = if keep_id == alexandria_storage::record_id_to_string(&c1) {
        (&c1, &c2)
    } else {
        (&c2, &c1)
    };
    cluster_repo
        .execute_merge(keep, remove, &merged_centroid)
        .await
        .unwrap();

    // Verify: one cluster with two members
    let all = cluster_repo.list_with_counts().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1, 2);

    // Verify the surviving cluster has the weighted-average centroid
    let (surviving, _) = &all[0];
    // Equal-weight average of [1.0, 0.0, 0.0] and [0.98, 0.02, 0.0] = [0.99, 0.01, 0.0]
    assert!((surviving.centroid[0] - 0.99).abs() < 0.001);
    assert!((surviving.centroid[1] - 0.01).abs() < 0.001);
}

#[tokio::test]
async fn test_split_leaves_no_orphan_on_partial_failure() {
    // This test verifies that facts remain in their original cluster if
    // the original cluster isn't deleted — the execute_split method returns
    // an error if cluster creation fails, but we can at least verify
    // the happy path doesn't orphan any facts.
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = MemoryRepo::new(db.inner());

    let cid = cluster_repo.create(Some("c"), &[0.5, 0.5]).await.unwrap();
    let mut fact_ids = Vec::new();
    for i in 0..4 {
        let emb = if i < 2 {
            vec![1.0, 0.0]
        } else {
            vec![0.0, 1.0]
        };
        let (fid, fid_t) = memory_repo
            .create_fact_with_id(&format!("f{i}"), 0.5, &emb, &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &fid_t).await.unwrap();
        fact_ids.push(fid);
    }

    // Execute split with explicit groups
    let members = cluster_repo.get_members(&cid).await.unwrap();
    let (cid_a, cid_b) = cluster_repo
        .execute_split(&cid, &members, &[0, 1], &[2, 3], &[1.0, 0.0], &[0.0, 1.0])
        .await
        .unwrap();

    // Every fact should have exactly one cluster assignment
    for fid in &fact_ids {
        let cluster = memory_repo.cluster_for_fact(fid).await.unwrap();
        assert!(cluster.is_some(), "Fact {fid} has no cluster assignment");
        let cluster = cluster.unwrap();
        // cid_a/cid_b are typed now; compare the typed ids directly.
        let cluster_id = cluster.id.as_ref();
        assert!(
            cluster_id == Some(&cid_a) || cluster_id == Some(&cid_b),
            "Fact {fid} assigned to unexpected cluster {:?}",
            cluster_id.map(record_id_to_string)
        );
    }
}
