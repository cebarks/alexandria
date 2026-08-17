use crate::search::cosine_similarity;

/// Result of a cluster health check.
#[derive(Debug)]
pub enum MaintenanceAction {
    /// Cluster is healthy, no action needed.
    Healthy,
    /// Cluster should split — members are too diffuse.
    Split {
        cluster_id: String,
        group_a: Vec<usize>,
        group_b: Vec<usize>,
        centroid_a: Vec<f32>,
        centroid_b: Vec<f32>,
    },
}

/// Result of checking two clusters for merge eligibility.
#[derive(Debug)]
pub enum MergeCheck {
    /// Clusters should be merged.
    Merge {
        keep_id: String,
        remove_id: String,
        merged_centroid: Vec<f32>,
    },
    /// Clusters are distinct enough, don't merge.
    Distinct,
}

/// Check if a cluster needs splitting by computing avg member-to-centroid similarity.
///
/// If the average drops below `cohesion_floor`, returns a `Split` action with
/// two groups computed via simple k-means(k=2).
pub fn check_cohesion(
    cluster_id: &str,
    centroid: &[f32],
    member_embeddings: &[Vec<f32>],
    cohesion_floor: f32,
) -> MaintenanceAction {
    if member_embeddings.len() < 4 {
        // Too few members to split meaningfully
        return MaintenanceAction::Healthy;
    }

    // Compute average similarity to centroid
    let avg_sim: f32 = member_embeddings
        .iter()
        .map(|emb| cosine_similarity(emb, centroid))
        .sum::<f32>()
        / member_embeddings.len() as f32;

    if avg_sim >= cohesion_floor {
        return MaintenanceAction::Healthy;
    }

    // Split via k-means(k=2)
    let (group_a, group_b, centroid_a, centroid_b) = kmeans_split(member_embeddings);

    MaintenanceAction::Split {
        cluster_id: cluster_id.to_string(),
        group_a,
        group_b,
        centroid_a,
        centroid_b,
    }
}

/// Check if two clusters should be merged.
pub fn check_merge(
    id_a: &str,
    centroid_a: &[f32],
    count_a: usize,
    id_b: &str,
    centroid_b: &[f32],
    count_b: usize,
    merge_threshold: f32,
) -> MergeCheck {
    let sim = cosine_similarity(centroid_a, centroid_b);
    if sim < merge_threshold {
        return MergeCheck::Distinct;
    }

    // Merge centroids weighted by member count
    let total = (count_a + count_b) as f32;
    let merged_centroid: Vec<f32> = centroid_a
        .iter()
        .zip(centroid_b)
        .map(|(a, b)| (count_a as f32 * a + count_b as f32 * b) / total)
        .collect();

    // Keep the cluster with more members
    let (keep_id, remove_id) = if count_a >= count_b {
        (id_a.to_string(), id_b.to_string())
    } else {
        (id_b.to_string(), id_a.to_string())
    };

    MergeCheck::Merge {
        keep_id,
        remove_id,
        merged_centroid,
    }
}

/// Simple k-means with k=2 for splitting.
/// Returns (group_a_indices, group_b_indices, centroid_a, centroid_b).
fn kmeans_split(embeddings: &[Vec<f32>]) -> (Vec<usize>, Vec<usize>, Vec<f32>, Vec<f32>) {
    let dim = embeddings[0].len();

    // Initialize centroids: first and most distant point
    let c_a = embeddings[0].clone();
    let mut max_dist = -1.0_f32;
    let mut far_idx = 1;
    for (i, emb) in embeddings.iter().enumerate().skip(1) {
        let sim = cosine_similarity(emb, &c_a);
        let dist = 1.0 - sim;
        if dist > max_dist {
            max_dist = dist;
            far_idx = i;
        }
    }
    let c_b = embeddings[far_idx].clone();

    let mut centroid_a = c_a;
    let mut centroid_b = c_b;

    // Run 10 iterations of k-means
    let mut assignments = vec![0u8; embeddings.len()];
    for _ in 0..10 {
        // Assign
        for (i, emb) in embeddings.iter().enumerate() {
            let sim_a = cosine_similarity(emb, &centroid_a);
            let sim_b = cosine_similarity(emb, &centroid_b);
            assignments[i] = if sim_a >= sim_b { 0 } else { 1 };
        }

        // Recompute centroids
        let mut new_a = vec![0.0_f32; dim];
        let mut new_b = vec![0.0_f32; dim];
        let mut count_a = 0usize;
        let mut count_b = 0usize;

        for (i, emb) in embeddings.iter().enumerate() {
            if assignments[i] == 0 {
                for (j, v) in emb.iter().enumerate() {
                    new_a[j] += v;
                }
                count_a += 1;
            } else {
                for (j, v) in emb.iter().enumerate() {
                    new_b[j] += v;
                }
                count_b += 1;
            }
        }

        if count_a > 0 {
            centroid_a = new_a.iter().map(|v| v / count_a as f32).collect();
        }
        if count_b > 0 {
            centroid_b = new_b.iter().map(|v| v / count_b as f32).collect();
        }
    }

    let group_a: Vec<usize> = assignments.iter().enumerate()
        .filter(|(_, &a)| a == 0).map(|(i, _)| i).collect();
    let group_b: Vec<usize> = assignments.iter().enumerate()
        .filter(|(_, &a)| a == 1).map(|(i, _)| i).collect();

    (group_a, group_b, centroid_a, centroid_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_healthy_cluster_no_split() {
        let centroid = vec![1.0, 0.0, 0.0];
        let members = vec![
            vec![0.95, 0.05, 0.0],
            vec![0.9, 0.1, 0.0],
            vec![0.92, 0.08, 0.0],
            vec![0.88, 0.12, 0.0],
        ];
        let result = check_cohesion("c1", &centroid, &members, 0.6);
        assert!(matches!(result, MaintenanceAction::Healthy));
    }

    #[test]
    fn test_diffuse_cluster_splits() {
        // Two distinct groups forced into one cluster
        let centroid = vec![0.5, 0.5, 0.0]; // midpoint
        let members = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.95, 0.05, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.05, 0.95, 0.0],
            vec![0.98, 0.02, 0.0],
            vec![0.02, 0.98, 0.0],
        ];
        let result = check_cohesion("c1", &centroid, &members, 0.85);
        match result {
            MaintenanceAction::Split { group_a, group_b, .. } => {
                // Should split into two groups
                assert!(!group_a.is_empty());
                assert!(!group_b.is_empty());
                assert_eq!(group_a.len() + group_b.len(), 6);
            }
            MaintenanceAction::Healthy => panic!("Expected split, got healthy"),
        }
    }

    #[test]
    fn test_too_few_members_no_split() {
        let centroid = vec![0.5, 0.5];
        let members = vec![
            vec![1.0, 0.0],
            vec![0.0, 1.0],
        ];
        // Even if diffuse, <4 members = no split
        let result = check_cohesion("c1", &centroid, &members, 0.9);
        assert!(matches!(result, MaintenanceAction::Healthy));
    }

    #[test]
    fn test_merge_similar_clusters() {
        let c_a = vec![1.0, 0.0, 0.0];
        let c_b = vec![0.98, 0.02, 0.0]; // very similar
        let result = check_merge("c1", &c_a, 5, "c2", &c_b, 3, 0.9);
        match result {
            MergeCheck::Merge { keep_id, remove_id, .. } => {
                assert_eq!(keep_id, "c1"); // more members
                assert_eq!(remove_id, "c2");
            }
            MergeCheck::Distinct => panic!("Expected merge"),
        }
    }

    #[test]
    fn test_no_merge_distinct_clusters() {
        let c_a = vec![1.0, 0.0, 0.0];
        let c_b = vec![0.0, 1.0, 0.0]; // very different
        let result = check_merge("c1", &c_a, 5, "c2", &c_b, 3, 0.9);
        assert!(matches!(result, MergeCheck::Distinct));
    }
}
