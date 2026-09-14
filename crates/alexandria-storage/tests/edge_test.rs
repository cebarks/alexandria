use alexandria_storage::repos::edge_repo::Neighbor;
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

    let id_a = mem
        .create_fact("Rust is fast", 0.9, &[0.1, 0.2, 0.3], &[])
        .await
        .unwrap();
    let id_b = mem
        .create_fact("Rust is safe", 0.9, &[0.15, 0.25, 0.35], &[])
        .await
        .unwrap();

    edges
        .create_edge(&id_a, &id_b, "relates_to", 0.85)
        .await
        .unwrap();

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

    edges
        .create_edge(&id_a, &id_b, "supports", 0.9)
        .await
        .unwrap();
    edges
        .create_edge(&id_a, &id_c, "contradicts", 0.7)
        .await
        .unwrap();

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

    edges
        .create_edge(&id_a, &id_b, "relates_to", 0.8)
        .await
        .unwrap();
    edges
        .create_edge(&id_b, &id_c, "relates_to", 0.7)
        .await
        .unwrap();

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

    edges
        .create_edge(&id_a, &id_b, "relates_to", 0.8)
        .await
        .unwrap();
    edges
        .create_edge(&id_b, &_id_c, "relates_to", 0.7)
        .await
        .unwrap();

    let neighbors = edges.get_neighbors(&id_a, 1).await.unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].hop, 1);
}

// --- the bounded walk behind the debug graph -------------------------------------------------
//
// `get_neighbors` is now a delegate over `get_neighbors_capped` with `usize::MAX`. These tests are
// what prove both callers still get what they always got: spreading activation must see the whole
// neighbourhood, while the debug UI needs a bound it can actually rely on.

/// Neighbors as sorted `id@hop:edge_type` strings. `Neighbor` has no `PartialEq`, and the walk
/// reports a node once per edge it is reached by, so two walks are compared on membership.
fn neighbor_keys(neighbors: &[Neighbor]) -> Vec<String> {
    let mut keys: Vec<String> = neighbors
        .iter()
        .map(|n| format!("{:?}@hop{}:{}", n.id, n.hop, n.edge_type))
        .collect();
    keys.sort();
    keys
}

/// A -> B -> C -> A, plus B -> D: a cycle *and* a branch, so a traversal that only worked on a
/// tree would fail here. Returns the four ids in A, B, C, D order.
async fn cyclic_fixture(
    mem: &MemoryRepo<'_>,
    edges: &EdgeRepo<'_>,
) -> (String, String, String, String) {
    let mut ids = Vec::new();
    for name in ["Node A", "Node B", "Node C", "Node D"] {
        ids.push(mem.create_fact(name, 0.9, &[0.1], &[]).await.unwrap());
    }
    edges
        .create_edge(&ids[0], &ids[1], "relates_to", 0.8)
        .await
        .unwrap();
    edges
        .create_edge(&ids[1], &ids[2], "relates_to", 0.7)
        .await
        .unwrap();
    edges
        .create_edge(&ids[2], &ids[0], "supports", 0.5)
        .await
        .unwrap();
    edges
        .create_edge(&ids[1], &ids[3], "contradicts", 0.6)
        .await
        .unwrap();
    (
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    )
}

#[tokio::test]
async fn test_get_neighbors_matches_capped_variant_at_max_bound() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());
    let (a, _b, _c, _d) = cyclic_fixture(&mem, &edges).await;

    for hops in 1..=4 {
        let uncapped = edges.get_neighbors(&a, hops).await.unwrap();
        let (capped, truncated) = edges
            .get_neighbors_capped(&a, hops, usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            neighbor_keys(&uncapped),
            neighbor_keys(&capped),
            "`usize::MAX` must be exactly the walk `get_neighbors` always performed, at {hops} hops"
        );
        assert!(
            !truncated,
            "a walk with no bound cannot report having hit one (hops={hops})"
        );
    }

    // And an absolute floor on what the full walk finds, so the equality above cannot be
    // satisfied by two implementations that are truncated in the same way. The fixture has three
    // reachable records other than the centre; the whole radius is two hops wide.
    let uncapped = edges.get_neighbors(&a, 2).await.unwrap();
    assert_eq!(
        uncapped.len(),
        3,
        "the unbounded walk must reach B, C and D from A; got {:?}",
        neighbor_keys(&uncapped)
    );
}

#[tokio::test]
async fn test_get_neighbors_capped_stops_at_its_node_bound() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());
    let (a, _b, _c, _d) = cyclic_fixture(&mem, &edges).await;

    let (whole, truncated) = edges.get_neighbors_capped(&a, 3, usize::MAX).await.unwrap();
    assert!(!truncated);
    let full = neighbor_keys(&whole);
    assert_eq!(full.len(), 3, "fixture sanity; got {full:?}");

    // The bound counts visited records *including the centre*, so a walk bounded to `n` nodes
    // returns at most `n - 1` neighbors — and below the fixture's four records it must always
    // report that it stopped early.
    for max_nodes in 2..4 {
        let (some, stopped) = edges.get_neighbors_capped(&a, 3, max_nodes).await.unwrap();
        assert!(
            some.len() < max_nodes,
            "a {max_nodes}-node bound must not return {} neighbors",
            some.len()
        );
        assert!(
            stopped,
            "stopping short of a 4-record graph at a {max_nodes}-node bound is truncation and must \
             be reported"
        );
        for key in neighbor_keys(&some) {
            assert!(
                full.contains(&key),
                "capped result {key} is not in the full walk {full:?}"
            );
        }
    }

    // Exactly big enough is not truncated: the flag means "there was more to find", not "the
    // bound was reachable".
    let (same, stopped) = edges.get_neighbors_capped(&a, 3, 4).await.unwrap();
    assert_eq!(neighbor_keys(&same), full);
    assert!(
        !stopped,
        "a bound that fits the whole ego-graph must not report truncation"
    );
}

#[tokio::test]
async fn test_get_neighbors_capped_terminates_on_a_cycle() {
    let db = setup().await;
    let mem = MemoryRepo::new(db.inner());
    let edges = EdgeRepo::new(db.inner());
    let (a, _b, _c, _d) = cyclic_fixture(&mem, &edges).await;

    // A -> B -> C -> A with a radius far past the graph's own diameter: only the `visited` set
    // keeps this from walking the cycle forever. Pinned explicitly because the debug UI now
    // accepts a user-selected radius, so a large one is a request away, not a typo in code.
    let (neighbors, truncated) = edges
        .get_neighbors_capped(&a, 50, usize::MAX)
        .await
        .unwrap();
    let wide = neighbor_keys(&neighbors);
    let (near, _) = edges.get_neighbors_capped(&a, 3, usize::MAX).await.unwrap();
    assert_eq!(
        wide,
        neighbor_keys(&near),
        "a radius past the graph's diameter must not change the result"
    );
    assert_eq!(
        wide.len(),
        3,
        "and the walk must still be the whole 3-node set"
    );
    assert!(!truncated);

    // Termination under a bound, too: the walk stops by refusal rather than by exhausting the
    // cycle.
    let (some, stopped) = edges.get_neighbors_capped(&a, 50, 2).await.unwrap();
    assert!(stopped);
    assert_eq!(some.len(), 1);
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
    edges
        .create_edge(&id_a, &id_b, "relates_to", 0.9)
        .await
        .unwrap();

    let found = edges.get_edges_for(&id_a).await.unwrap();
    assert_eq!(found.len(), 1);
    assert!(
        found[0].in_node.is_some(),
        "in_node should deserialize, not be None"
    );
    assert!(
        found[0].out_node.is_some(),
        "out_node should deserialize, not be None"
    );
}
