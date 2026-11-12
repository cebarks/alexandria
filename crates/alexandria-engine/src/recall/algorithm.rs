use super::scope_handle::RecallHandle;
use crate::clusters::ClusterInfo;
use crate::search::{cosine_similarity, rank_by_similarity};
use alexandria_storage::models::ClusterVersion;

/// A memory's summary for recall results.
#[derive(Debug, Clone)]
pub struct FactSummary {
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub heat: f64,
}

/// A cluster with its member facts loaded.
///
/// `version` is what the caller's own read saw about this cluster — its stored membership guard and the
/// live member count — and `broad_recall` stamps it into the handle it issues. It is part of the input
/// rather than something the algorithm derives from `members`: the count the pass holds *is* a truncated
/// representative set in some paths, and the guard is not in the rows at all. A caller that has no
/// versioned read to offer (the interim unscoped arm, which resolves no owned cluster row) says so with
/// `membership_revision: None` rather than inventing a stamp.
#[derive(Debug, Clone)]
pub struct ClusterWithMembers {
    pub info: ClusterInfo,
    pub members: Vec<FactSummary>,
    pub version: ClusterVersion,
}

/// A matched cluster in broad recall results.
#[derive(Debug)]
pub struct ClusterMatch {
    pub cluster_id: String,
    pub label: Option<String>,
    pub similarity: f32,
    pub representative_memories: Vec<MemoryResult>,
    pub scope_handle: Option<String>,
}

/// An individual memory result.
#[derive(Debug)]
pub struct MemoryResult {
    pub id: String,
    pub content: String,
    pub similarity: f32,
    pub heat: f64,
}

/// Result of broad recall.
#[derive(Debug)]
pub struct BroadRecallResult {
    pub clusters: Vec<ClusterMatch>,
}

/// Result of focused recall.
///
/// `snapshot_moved` says whether the cluster has changed since the handle was issued. It is deliberately
/// *not* the public response: `recall` promises to narrow into a cluster, never into a frozen view of
/// one, so a moved snapshot is answered with current rows and this flag exists for the server's own
/// owner-safe diagnostic and for tests that need to see the comparison actually happened.
#[derive(Debug)]
pub struct FocusedRecallResult {
    pub memories: Vec<MemoryResult>,
    pub snapshot_moved: bool,
}

/// Broad recall: find top matching clusters for a query.
///
/// For each cluster, check centroid similarity, verify against actual members,
/// then rank by best_member_sim × cluster_heat. Return top `limit` clusters
/// with scope handles for narrowing.
///
/// `min_similarity` is the noise floor on centroid similarity; callers should
/// pass the same value used for `retrieve_memories` so the two can't drift.
pub fn broad_recall(
    query_embedding: &[f32],
    clusters: &[ClusterWithMembers],
    limit: usize,
    min_similarity: f32,
) -> BroadRecallResult {
    let mut matches: Vec<ClusterMatch> = clusters
        .iter()
        .filter_map(|cwm| {
            let centroid_sim = cosine_similarity(query_embedding, &cwm.info.centroid);
            // Noise floor only. With all-MiniLM-L6-v2 a question against a
            // stored statement scores ~0.2; unrelated text ~0.0.
            if centroid_sim < min_similarity {
                return None;
            }

            // Find best member similarity
            let member_embeddings: Vec<Vec<f32>> =
                cwm.members.iter().map(|m| m.embedding.clone()).collect();
            let top_members = rank_by_similarity(query_embedding, &member_embeddings, 3);

            if top_members.is_empty() {
                return None;
            }

            let best_member_sim = top_members[0].1;

            // Representative memories
            let representative_memories: Vec<MemoryResult> = top_members
                .iter()
                .map(|(idx, sim)| {
                    let m = &cwm.members[*idx];
                    MemoryResult {
                        id: m.id.clone(),
                        content: m.content.clone(),
                        similarity: *sim,
                        heat: m.heat,
                    }
                })
                .collect();

            // The handle is a locator plus the version this pass read. `issued_at` used to live here and
            // was a wall clock, which made two identical passes look different and a moved cluster look
            // the same; neither of those is a fact about the cluster.
            let handle = RecallHandle::new(cwm.info.id.clone(), cwm.version);

            Some(ClusterMatch {
                cluster_id: cwm.info.id.clone(),
                label: None,
                similarity: best_member_sim,
                representative_memories,
                scope_handle: handle.encode().ok(),
            })
        })
        .collect();

    // Sort by similarity descending
    matches.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    matches.truncate(limit);

    BroadRecallResult { clusters: matches }
}

/// Focused recall: narrow within a specific cluster using a recall handle.
///
/// The rows served are `cluster_data`'s — whatever the caller's own read found just now — never anything
/// reconstructed from the handle, which is why the handle can be an unsigned string. The handle's
/// version is compared with the cluster's current one and reported as `snapshot_moved`: the point of the
/// number is that this pass can say it is answering about a cluster that has changed since the caller
/// last looked, instead of implying the two calls saw the same thing.
pub fn focused_recall(
    query_embedding: &[f32],
    handle: &RecallHandle,
    cluster_data: &ClusterWithMembers,
) -> FocusedRecallResult {
    let member_embeddings: Vec<Vec<f32>> = cluster_data
        .members
        .iter()
        .map(|m| m.embedding.clone())
        .collect();
    let ranked = rank_by_similarity(query_embedding, &member_embeddings, 10);

    let memories = ranked
        .iter()
        .map(|(idx, sim)| {
            let m = &cluster_data.members[*idx];
            MemoryResult {
                id: m.id.clone(),
                content: m.content.clone(),
                similarity: *sim,
                heat: m.heat,
            }
        })
        .collect();

    FocusedRecallResult {
        memories,
        snapshot_moved: handle.snapshot_moved(&cluster_data.version),
    }
}
