use alexandria_storage::{Database, schema};
use alexandria_storage::repos::{MemoryRepo, HeatRepo, ClusterRepo};

#[tokio::test]
async fn test_create_and_get_fact() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();

    let repo = MemoryRepo::new(db.inner());
    let id = repo
        .create_fact(
            "OAuth tokens expire after 7 days",
            0.8,
            &vec![0.1_f32; 384],
            &["auth".to_string()],
        )
        .await
        .unwrap();

    let fact = repo.get_fact(&id).await.unwrap().unwrap();
    assert_eq!(fact.content, "OAuth tokens expire after 7 days");
    assert_eq!(fact.confidence, 0.8);
    assert!(!fact.deleted);
}

#[tokio::test]
async fn test_soft_delete_fact() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();

    let repo = MemoryRepo::new(db.inner());
    let id = repo
        .create_fact("temp fact", 0.5, &vec![0.0_f32; 384], &[])
        .await
        .unwrap();

    repo.soft_delete_fact(&id).await.unwrap();
    let fact = repo.get_fact(&id).await.unwrap().unwrap();
    assert!(fact.deleted);
}

#[tokio::test]
async fn test_create_and_get_heat_state() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();

    let memory_repo = MemoryRepo::new(db.inner());
    let fact_id = memory_repo
        .create_fact("test", 0.5, &vec![0.0_f32; 384], &[])
        .await
        .unwrap();

    let heat_repo = HeatRepo::new(db.inner());
    heat_repo.create_for_memory(&fact_id, 1.0).await.unwrap();

    let state = heat_repo.get(&fact_id).await.unwrap().unwrap();
    assert_eq!(state.heat, 1.0);
    assert_eq!(state.stability, 1.0);
    assert_eq!(state.access_count, 0);
}

#[tokio::test]
async fn test_create_cluster_and_add_member() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();

    let memory_repo = MemoryRepo::new(db.inner());
    let fact_id = memory_repo
        .create_fact("auth fact", 0.9, &vec![0.1_f32; 384], &["auth".to_string()])
        .await
        .unwrap();

    let cluster_repo = ClusterRepo::new(db.inner());
    let cluster_id = cluster_repo
        .create(Some("Authentication"), &vec![0.1_f32; 384])
        .await
        .unwrap();

    cluster_repo
        .add_member(&cluster_id, &fact_id)
        .await
        .unwrap();

    let members = cluster_repo.get_members(&cluster_id).await.unwrap();
    assert_eq!(members.len(), 1);
}
