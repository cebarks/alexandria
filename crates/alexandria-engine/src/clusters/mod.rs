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

/// The centroid a set of member embeddings implies: their arithmetic mean.
///
/// This is the *derived* value a cluster's membership authorises, and it is kept apart from
/// [`update_centroid`] because the two answer different questions. The running average asks "what does the
/// centroid I already have become when one member is added?" and is exact only while the centroid it is
/// handed is itself the mean of the members it counts — so it silently inherits any drift in that stored
/// value, and it cannot answer a write that has to prove its centroid from the members rather than from
/// the number it is replacing. The mean asks "what is the centre of these members?" and is the only
/// answer that survives a concurrent writer having moved the membership since this caller read it, which
/// is exactly the state a derived-bookkeeping conflict leaves behind.
///
/// Dimension handling is deliberate, not defensive. Every embedding in a cluster was produced by one
/// locked model, so a member of a different length is not a smaller cluster but a corrupt population, and
/// averaging a short member against a long one would silently truncate the tail of the centroid. The
/// caller sees the mismatch instead of a plausible number: `None` names the first member whose length
/// differs from the first member's, and an empty population has no centre (`None`) rather than a zero
/// vector that a caller could mistake for a real centroid.
pub fn centroid(embeddings: &[&[f32]]) -> Option<Vec<f32>> {
    alexandria_storage::models::cluster::mean_centroid(embeddings)
}

/// The centroid implied by a population of members together with one member being added to it.
///
/// One function because the store path is the only caller that needs the combination, and writing it as
/// `centroid(members ∪ [added])` in prose keeps the arithmetic in one place: the added vector is a member
/// of the result, so it is counted in the divisor as well as the numerator.
///
/// There is deliberately no removal or "move" sibling here, and a scope move does not need one: storage's
/// `MutationRepo::move_scope` re-derives the centre of the survivors from the population it reads inside
/// its own transaction, calling the same [`mean_centroid`] this function is a call to. A helper with no
/// producer is the shape this repo has already deleted once (the `worker` failure variant in
/// `docs/prompt-path-stall-attribution.md`), so none is added here on spec — but the arithmetic is shared,
/// which is the point of `centroid` being a call rather than a copy.
pub fn centroid_with(members: &[&[f32]], added: &[f32]) -> Option<Vec<f32>> {
    let mut population: Vec<&[f32]> = Vec::with_capacity(members.len() + 1);
    population.extend_from_slice(members);
    population.push(added);
    centroid(&population)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's mean and the storage writer's mean are one function, pinned.
    ///
    /// Asserted on the shapes where a divergence would be silent in production and fatal in the journal:
    /// the empty population, the zero-dimension population, and the mismatched-length member that must be
    /// a refusal rather than a truncated centroid.
    #[test]
    fn centroid_is_storage_mean_on_every_shape_that_refuses() {
        let empty: Vec<&[f32]> = vec![];
        assert_eq!(centroid(&empty), None);
        assert_eq!(centroid(&[&[]]), None);
        assert_eq!(centroid(&[&[1.0, 1.0], &[1.0]]), None);
        assert_eq!(centroid(&[&[0.0, 2.0], &[2.0, 0.0]]), Some(vec![1.0, 1.0]));
        assert_eq!(centroid(&[&[0.1, 0.2]]), Some(vec![0.1, 0.2]));
    }

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
