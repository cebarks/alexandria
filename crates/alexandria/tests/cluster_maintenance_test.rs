//! Integration tests for cluster split and merge mechanics.
//!
//! These test the storage operations that the maintenance loop executes,
//! without needing a running HTTP server or real embedding model.

use alexandria_engine::clusters::maintenance::{
    check_cohesion, check_merge, MaintenanceAction, MergeCheck,
};
use alexandria_mcp::server::record_id_to_string;
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{schema, Database};

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
    let embeddings = vec![
        vec![1.0, 0.0, 0.0],
        vec![0.95, 0.05, 0.0],
        vec![0.98, 0.02, 0.0],
        vec![0.0, 1.0, 0.0],
        vec![0.05, 0.95, 0.0],
        vec![0.02, 0.98, 0.0],
    ];
    for (i, emb) in embeddings.iter().enumerate() {
        let fid = memory_repo
            .create_fact(&format!("fact{i}"), 0.5, emb, &[])
            .await
            .unwrap();
        cluster_repo.add_member(&cid, &fid).await.unwrap();
    }

    // Verify cohesion check says split
    let members = cluster_repo.get_members(&cid).await.unwrap();
    let member_embeddings: Vec<Vec<f32>> = members.iter().map(|f| f.embedding.clone()).collect();
    let action = check_cohesion(&cid, &[0.5, 0.5, 0.0], &member_embeddings, 0.85);
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

    // Execute split: same steps as the maintenance loop
    let cid_a = cluster_repo.create(None, &centroid_a).await.unwrap();
    let cid_b = cluster_repo.create(None, &centroid_b).await.unwrap();
    for &idx in &group_a {
        let fid = members[idx]
            .id
            .as_ref()
            .map(record_id_to_string)
            .unwrap_or_default();
        cluster_repo.remove_member(&cid, &fid).await.unwrap();
        cluster_repo.add_member(&cid_a, &fid).await.unwrap();
    }
    for &idx in &group_b {
        let fid = members[idx]
            .id
            .as_ref()
            .map(record_id_to_string)
            .unwrap_or_default();
        cluster_repo.remove_member(&cid, &fid).await.unwrap();
        cluster_repo.add_member(&cid_b, &fid).await.unwrap();
    }
    cluster_repo.delete(&cid).await.unwrap();

    // Verify: old cluster gone, two new ones exist, all facts accounted for
    let all = cluster_repo.list_with_counts().await.unwrap();
    assert_eq!(all.len(), 2);
    let total_members: usize = all.iter().map(|(_, c)| c).sum();
    assert_eq!(total_members, 6);
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

    let f1 = memory_repo
        .create_fact("in c1", 0.5, &[1.0, 0.0, 0.0], &[])
        .await
        .unwrap();
    let f2 = memory_repo
        .create_fact("in c2", 0.5, &[0.98, 0.02, 0.0], &[])
        .await
        .unwrap();
    cluster_repo.add_member(&c1, &f1).await.unwrap();
    cluster_repo.add_member(&c2, &f2).await.unwrap();

    // check_merge says merge (equal counts, c1 kept by convention)
    let merge = check_merge(&c1, &[1.0, 0.0, 0.0], 1, &c2, &[0.98, 0.02, 0.0], 1, 0.9);
    let (keep_id, remove_id, merged_centroid) = match merge {
        MergeCheck::Merge {
            keep_id,
            remove_id,
            merged_centroid,
        } => (keep_id, remove_id, merged_centroid),
        MergeCheck::Distinct => panic!("Expected merge"),
    };

    // Execute merge: same steps as the maintenance loop
    let removed_members = cluster_repo.get_members(&remove_id).await.unwrap();
    for fact in &removed_members {
        let fid = fact
            .id
            .as_ref()
            .map(record_id_to_string)
            .unwrap_or_default();
        cluster_repo
            .remove_member(&remove_id, &fid)
            .await
            .unwrap();
        cluster_repo.add_member(&keep_id, &fid).await.unwrap();
    }
    cluster_repo
        .update_centroid(&keep_id, &merged_centroid)
        .await
        .unwrap();
    cluster_repo.delete(&remove_id).await.unwrap();

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
