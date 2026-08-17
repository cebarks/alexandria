use alexandria_engine::clusters::ClusterInfo;
use alexandria_engine::recall::{
    broad_recall, focused_recall, ClusterWithMembers, FactSummary, ScopeHandle,
};

#[test]
fn test_scope_handle_roundtrip() {
    let handle = ScopeHandle {
        cluster_id: "cluster:abc123".to_string(),
        depth: 0,
        query_embedding: vec![0.1, 0.2, 0.3],
        issued_at: 1234567890,
    };

    let encoded = handle.encode().unwrap();
    let decoded = ScopeHandle::decode(&encoded).unwrap();

    assert_eq!(decoded.cluster_id, handle.cluster_id);
    assert_eq!(decoded.depth, handle.depth);
    assert_eq!(decoded.query_embedding, handle.query_embedding);
}

#[test]
fn test_broad_recall_returns_cluster_matches() {
    let clusters = vec![ClusterWithMembers {
        info: ClusterInfo {
            id: "c1".into(),
            centroid: vec![1.0, 0.0, 0.0],
            member_count: 2,
        },
        members: vec![
            FactSummary {
                id: "f1".into(),
                content: "OAuth tokens".into(),
                embedding: vec![0.9, 0.1, 0.0],
                heat: 5.0,
            },
            FactSummary {
                id: "f2".into(),
                content: "JWT signing".into(),
                embedding: vec![0.8, 0.2, 0.0],
                heat: 3.0,
            },
        ],
    }];

    let query = vec![0.95, 0.05, 0.0]; // auth-related
    let result = broad_recall(&query, &clusters, 5);

    assert!(!result.clusters.is_empty());
    assert!(result.clusters[0].scope_handle.is_some());
    assert!(result.clusters[0].representative_memories.len() <= 3);
}

#[test]
fn test_focused_recall_narrows_within_cluster() {
    let cluster_data = ClusterWithMembers {
        info: ClusterInfo {
            id: "c1".into(),
            centroid: vec![1.0, 0.0],
            member_count: 3,
        },
        members: vec![
            FactSummary {
                id: "f1".into(),
                content: "close match".into(),
                embedding: vec![0.9, 0.1],
                heat: 2.0,
            },
            FactSummary {
                id: "f2".into(),
                content: "medium match".into(),
                embedding: vec![0.5, 0.5],
                heat: 1.0,
            },
            FactSummary {
                id: "f3".into(),
                content: "far match".into(),
                embedding: vec![0.1, 0.9],
                heat: 0.5,
            },
        ],
    };

    let scope = ScopeHandle {
        cluster_id: "c1".into(),
        depth: 0,
        query_embedding: vec![1.0, 0.0],
        issued_at: 0,
    };
    let query = vec![1.0, 0.0];
    let result = focused_recall(&query, &scope, &cluster_data);

    assert_eq!(result.memories.len(), 3);
    // First result should be the closest match
    assert_eq!(result.memories[0].id, "f1");
}
