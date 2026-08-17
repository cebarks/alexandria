pub mod maintenance;

use crate::search::cosine_similarity;

#[derive(Debug, Clone)]
pub struct ClusterInfo {
    pub id: String,
    pub centroid: Vec<f32>,
    pub member_count: usize,
}

#[derive(Debug)]
pub enum ClusterAssignment {
    Existing(String),
    NewCluster,
}

/// Find the best matching cluster for an embedding.
/// Returns `Existing(id)` if best cosine similarity ≥ join_threshold,
/// otherwise `NewCluster`.
pub fn assign_to_cluster(
    embedding: &[f32],
    clusters: &[ClusterInfo],
    join_threshold: f32,
) -> ClusterAssignment {
    let mut best_sim = -1.0_f32;
    let mut best_id = None;

    for cluster in clusters {
        let sim = cosine_similarity(embedding, &cluster.centroid);
        if sim > best_sim {
            best_sim = sim;
            best_id = Some(&cluster.id);
        }
    }

    match best_id {
        Some(id) if best_sim >= join_threshold => ClusterAssignment::Existing(id.clone()),
        _ => ClusterAssignment::NewCluster,
    }
}

/// Update a cluster centroid with a new member using O(1) running average.
pub fn update_centroid(
    old_centroid: &[f32],
    new_embedding: &[f32],
    old_member_count: usize,
) -> Vec<f32> {
    let n = old_member_count as f32;
    old_centroid
        .iter()
        .zip(new_embedding)
        .map(|(c, e)| (n * c + e) / (n + 1.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assign_to_existing_cluster() {
        let clusters = vec![ClusterInfo {
            id: "c1".into(),
            centroid: vec![1.0, 0.0, 0.0],
            member_count: 5,
        }];
        let embedding = vec![0.9, 0.1, 0.0]; // close to c1

        let result = assign_to_cluster(&embedding, &clusters, 0.75);
        assert!(matches!(result, ClusterAssignment::Existing(id) if id == "c1"));
    }

    #[test]
    fn test_create_new_cluster_when_no_match() {
        let clusters = vec![ClusterInfo {
            id: "c1".into(),
            centroid: vec![1.0, 0.0, 0.0],
            member_count: 5,
        }];
        let embedding = vec![0.0, 0.0, 1.0]; // far from c1

        let result = assign_to_cluster(&embedding, &clusters, 0.75);
        assert!(matches!(result, ClusterAssignment::NewCluster));
    }

    #[test]
    fn test_centroid_update() {
        let old_centroid = vec![1.0, 0.0];
        let new_embedding = vec![0.0, 1.0];
        let member_count = 4;

        let new_centroid = update_centroid(&old_centroid, &new_embedding, member_count);
        assert!((new_centroid[0] - 0.8).abs() < 0.001);
        assert!((new_centroid[1] - 0.2).abs() < 0.001);
    }

    #[test]
    fn test_assign_empty_clusters() {
        let result = assign_to_cluster(&[1.0, 0.0], &[], 0.5);
        assert!(matches!(result, ClusterAssignment::NewCluster));
    }
}
