use alexandria_mcp::migrate::{ReembedOutcome, reembed};
use alexandria_pipeline::embedding::{EmbeddingProvider, MAX_TOKENS};
use alexandria_storage::repos::{ClusterRepo, MemoryRepo};
use alexandria_storage::{Database, system_config};

/// Fake model "b": 3-dim unit vectors chosen per text so a mean centroid is
/// distinguishable from any single member.
struct ModelB;

fn embed_b(text: &str) -> Vec<f32> {
    match text {
        "one" => vec![1.0, 0.0, 0.0],
        "two" => vec![0.0, 1.0, 0.0],
        _ => vec![0.0, 0.0, 1.0],
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for ModelB {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| embed_b(t)).collect())
    }
    fn dimensions(&self) -> usize {
        3
    }
    fn model_id(&self) -> &str {
        "b"
    }
}

/// Seed: two live facts and one deleted fact, all 2-dim under lock model "a";
/// one cluster holding both live facts with a 2-dim centroid.
async fn seed() -> (Database, String, String, String, String) {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    let memories = MemoryRepo::new(db.inner());
    let live1 = memories
        .create_fact("one", 0.5, &[0.6, 0.8], &[])
        .await
        .unwrap();
    let live2 = memories
        .create_fact("two", 0.5, &[0.8, 0.6], &[])
        .await
        .unwrap();
    let gone = memories
        .create_fact("three", 0.5, &[0.0, 1.0], &[])
        .await
        .unwrap();
    memories.soft_delete_fact(&gone).await.unwrap();

    let clusters = ClusterRepo::new(db.inner());
    let cid = clusters.create(None, &[0.7, 0.7]).await.unwrap();
    clusters.add_member(&cid, &live1).await.unwrap();
    clusters.add_member(&cid, &live2).await.unwrap();

    system_config::set_config(db.inner(), "embedding_model", "a")
        .await
        .unwrap();
    system_config::set_config(db.inner(), "embedding_dimensions", "2")
        .await
        .unwrap();
    (db, live1, live2, gone, cid)
}

#[tokio::test]
async fn reembed_rewrites_facts_centroids_and_lock() {
    let (db, live1, live2, gone, cid) = seed().await;

    let outcome = reembed(&db, &ModelB, 2).await.unwrap();
    match outcome {
        ReembedOutcome::Done { facts, clusters } => {
            assert_eq!(facts, 3, "deleted facts are re-embedded too");
            assert_eq!(clusters, 1);
        }
        ReembedOutcome::Skipped(why) => panic!("unexpected skip: {why}"),
    }

    let memories = MemoryRepo::new(db.inner());
    for id in [&live1, &live2, &gone] {
        let fact = memories.get_fact(id).await.unwrap().unwrap();
        assert_eq!(fact.embedding, embed_b(&fact.content), "fact {id}");
    }
    assert!(memories.get_fact(&gone).await.unwrap().unwrap().deleted);

    let clusters = ClusterRepo::new(db.inner());
    let (cluster, _) = clusters
        .list_with_counts()
        .await
        .unwrap()
        .into_iter()
        .find(|(c, _)| {
            c.id.as_ref().map(alexandria_storage::record_id_to_string) == Some(cid.clone())
        })
        .expect("cluster still exists");
    assert_eq!(
        cluster.centroid,
        vec![0.5, 0.5, 0.0],
        "mean of both members"
    );

    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_dimensions")
            .await
            .unwrap()
            .as_deref(),
        Some("3")
    );
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_max_tokens")
            .await
            .unwrap()
            .as_deref(),
        Some(MAX_TOKENS.to_string().as_str())
    );
}

/// Same model but no token lock: the corpus was embedded at the old 128-token limit,
/// so this is a real re-embed, not a no-op.
#[tokio::test]
async fn reembed_runs_when_only_token_lock_differs() {
    let (db, live1, _, _, _) = seed().await;
    system_config::set_config(db.inner(), "embedding_model", "b")
        .await
        .unwrap();

    let outcome = reembed(&db, &ModelB, 2).await.unwrap();
    assert!(matches!(outcome, ReembedOutcome::Done { facts: 3, .. }));

    let fact = MemoryRepo::new(db.inner())
        .get_fact(&live1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fact.embedding, embed_b("one"));
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_max_tokens")
            .await
            .unwrap()
            .as_deref(),
        Some(MAX_TOKENS.to_string().as_str())
    );
}

/// The HNSW index rejects vectors of any other dimension, so reembed must drop
/// it before writing 3-dim vectors over a 2-dim index.
#[tokio::test]
async fn reembed_drops_vector_index_before_changing_dimension() {
    let (db, live1, _, _, _) = seed().await;
    alexandria_storage::schema::ensure_vector_index(db.inner(), 2)
        .await
        .unwrap();

    let outcome = reembed(&db, &ModelB, 2).await.unwrap();
    assert!(matches!(outcome, ReembedOutcome::Done { facts: 3, .. }));

    let fact = MemoryRepo::new(db.inner())
        .get_fact(&live1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fact.embedding, embed_b("one"));
}

#[tokio::test]
async fn reembed_is_noop_when_lock_matches() {
    let (db, live1, _, _, _) = seed().await;
    system_config::set_config(db.inner(), "embedding_model", "b")
        .await
        .unwrap();
    system_config::set_config(db.inner(), "embedding_max_tokens", &MAX_TOKENS.to_string())
        .await
        .unwrap();

    let outcome = reembed(&db, &ModelB, 2).await.unwrap();
    assert!(matches!(outcome, ReembedOutcome::Skipped(_)));

    let fact = MemoryRepo::new(db.inner())
        .get_fact(&live1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fact.embedding, vec![0.6, 0.8], "untouched");
}

#[tokio::test]
async fn reembed_is_noop_on_fresh_database() {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();

    let outcome = reembed(&db, &ModelB, 2).await.unwrap();
    assert!(matches!(outcome, ReembedOutcome::Skipped(_)));
    assert!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reembed_refuses_unlocked_database_with_facts() {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    let memories = MemoryRepo::new(db.inner());
    let id = memories
        .create_fact("pre-lock", 0.5, &[0.6, 0.8], &[])
        .await
        .unwrap();

    let err = reembed(&db, &ModelB, 2).await.unwrap_err();
    assert!(err.to_string().contains("1 fact"), "{err}");

    let fact = memories.get_fact(&id).await.unwrap().unwrap();
    assert_eq!(fact.embedding, vec![0.6, 0.8], "untouched");
    assert!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reembed_drops_empty_clusters() {
    let (db, _, _, _, cid) = seed().await;
    let clusters = ClusterRepo::new(db.inner());
    let empty = clusters.create(None, &[0.1, 0.9]).await.unwrap();

    reembed(&db, &ModelB, 2).await.unwrap();

    let ids: Vec<String> = clusters
        .list()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|c| c.id.as_ref().map(alexandria_storage::record_id_to_string))
        .collect();
    assert_eq!(ids, vec![cid], "empty cluster {empty} should be gone");
}

#[tokio::test]
async fn reembed_moves_lock_over_empty_corpus() {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner())
        .await
        .unwrap();
    system_config::set_config(db.inner(), "embedding_model", "a")
        .await
        .unwrap();

    let outcome = reembed(&db, &ModelB, 2).await.unwrap();
    assert!(matches!(
        outcome,
        ReembedOutcome::Done {
            facts: 0,
            clusters: 0
        }
    ));
    assert_eq!(
        system_config::get_config(db.inner(), "embedding_model")
            .await
            .unwrap()
            .as_deref(),
        Some("b")
    );
}
