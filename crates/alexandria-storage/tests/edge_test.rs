use alexandria_storage::repos::{EdgeRepo, MemoryRepo};
use alexandria_storage::{Database, schema};

async fn setup() -> Database {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    db
}

#[tokio::test]
async fn test_create_and_query_edge() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());

    let id_a = mem.create_fact("Rust is fast", 0.9, &[0.1, 0.2, 0.3], &[]).await.unwrap();
    let id_b = mem.create_fact("Rust is safe", 0.9, &[0.15, 0.25, 0.35], &[]).await.unwrap();

    edges.create_edge(&id_a, &id_b, "relates_to", 0.85).await.unwrap();

    let found = edges.get_edges_for(&id_a).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].edge_type, "relates_to");
}

#[tokio::test]
async fn test_direct_neighbors() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());

    let id_a = mem.create_fact("Node A", 0.9, &[0.1], &[]).await.unwrap();
    let id_b = mem.create_fact("Node B", 0.9, &[0.2], &[]).await.unwrap();
    let id_c = mem.create_fact("Node C", 0.9, &[0.3], &[]).await.unwrap();

    edges.create_edge(&id_a, &id_b, "supports", 0.9).await.unwrap();
    edges.create_edge(&id_a, &id_c, "contradicts", 0.7).await.unwrap();

    let neighbors = edges.get_direct_neighbors(&id_a).await.unwrap();
    assert_eq!(neighbors.len(), 2);
    assert!(neighbors.iter().all(|n| n.hop == 1));
}

#[tokio::test]
async fn test_multi_hop_neighbors() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());

    // A -> B -> C (chain)
    let id_a = mem.create_fact("Node A", 0.9, &[0.1], &[]).await.unwrap();
    let id_b = mem.create_fact("Node B", 0.9, &[0.2], &[]).await.unwrap();
    let id_c = mem.create_fact("Node C", 0.9, &[0.3], &[]).await.unwrap();

    edges.create_edge(&id_a, &id_b, "relates_to", 0.8).await.unwrap();
    edges.create_edge(&id_b, &id_c, "relates_to", 0.7).await.unwrap();

    // From A, max_hops=2: should find B (hop 1) and C (hop 2)
    let neighbors = edges.get_neighbors(&id_a, 2).await.unwrap();
    assert_eq!(neighbors.len(), 2);

    let hop1: Vec<_> = neighbors.iter().filter(|n| n.hop == 1).collect();
    let hop2: Vec<_> = neighbors.iter().filter(|n| n.hop == 2).collect();
    assert_eq!(hop1.len(), 1);
    assert_eq!(hop2.len(), 1);
}

#[tokio::test]
async fn test_multi_hop_respects_limit() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());

    // A -> B -> C chain, but max_hops=1: should only find B
    let id_a = mem.create_fact("Node A", 0.9, &[0.1], &[]).await.unwrap();
    let id_b = mem.create_fact("Node B", 0.9, &[0.2], &[]).await.unwrap();
    let _id_c = mem.create_fact("Node C", 0.9, &[0.3], &[]).await.unwrap();

    edges.create_edge(&id_a, &id_b, "relates_to", 0.8).await.unwrap();
    edges.create_edge(&id_b, &_id_c, "relates_to", 0.7).await.unwrap();

    let neighbors = edges.get_neighbors(&id_a, 1).await.unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].hop, 1);
}

#[tokio::test]
async fn test_get_edges_for_deserializes_in_out_record_ids() {
    // Regression test: MemoryEdge::in_node/out_node were silently deserializing as None
    // because the SurrealValue derive macro needs its own #[surreal(rename = "...")] attribute
    // (not just #[serde(rename = "...")]) to map the SurrealDB "in"/"out" edge fields.
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());

    let id_a = mem.create_fact("Node A", 0.9, &[0.1], &[]).await.unwrap();
    let id_b = mem.create_fact("Node B", 0.9, &[0.2], &[]).await.unwrap();
    edges.create_edge(&id_a, &id_b, "relates_to", 0.9).await.unwrap();

    let found = edges.get_edges_for(&id_a).await.unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].in_node.is_some(), "in_node should deserialize, not be None");
    assert!(found[0].out_node.is_some(), "out_node should deserialize, not be None");
}
