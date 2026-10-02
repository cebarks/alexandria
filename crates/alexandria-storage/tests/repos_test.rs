use alexandria_storage::repos::{ClusterRepo, HeatRepo, MemoryRepo};
use alexandria_storage::{Database, schema};

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

/// Why every live query has to say `quarantined_at = NONE`, and not the spelling a reader would
/// reach for first.
///
/// An absent `option<datetime>` field reads as `NONE` on the pinned 3.2.4 engine, and **both**
/// `IS NOT NULL` and `!= NULL` are satisfied by `NONE` — so those predicates filter nothing at
/// all and would quietly keep returning quarantined rows (secrets included) from every read path.
/// The row created without the field is exactly what every pre-v008 fact looks like, which is the
/// case that matters most: it must stay live, not vanish.
#[tokio::test]
async fn quarantine_none_predicate_pins_the_trap() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();

    let repo = MemoryRepo::new(db.inner());
    let legacy = repo
        .create_fact("legacy row", 0.5, &vec![0.1_f32; 384], &[])
        .await
        .unwrap();
    let secret = repo
        .create_fact("quarantined row", 0.5, &vec![0.1_f32; 384], &[])
        .await
        .unwrap();

    db.inner()
        .query("UPDATE type::record($id) SET quarantined_at = time::now()")
        .bind(("id", secret))
        .await
        .unwrap()
        .check()
        .unwrap();

    let live = {
        let mut r = db
            .inner()
            .query("SELECT * FROM fact WHERE deleted = false AND quarantined_at = NONE")
            .await
            .unwrap();
        let rows: Vec<alexandria_storage::models::Fact> = r.take(0).unwrap();
        rows
    };
    assert_eq!(
        live.len(),
        1,
        "`= NONE` must keep the never-quarantined row and drop the quarantined one"
    );
    assert_eq!(live[0].content, "legacy row");
    assert_eq!(
        live[0]
            .id
            .as_ref()
            .map(alexandria_storage::record_id_to_string)
            .as_deref(),
        Some(legacy.as_str()),
        "the surviving row must be the one that never had the field set"
    );

    for spelling in ["quarantined_at IS NOT NULL", "quarantined_at != NULL"] {
        let mut r = db
            .inner()
            .query(format!("SELECT * FROM fact WHERE {spelling}"))
            .await
            .unwrap();
        let leaked: Vec<alexandria_storage::models::Fact> = r.take(0).unwrap();
        assert_eq!(
            leaked.len(),
            2,
            "`{spelling}` is satisfied by NONE, so it filters nothing — this is the trap that \
             makes `= NONE` the only correct live predicate"
        );
    }
}
