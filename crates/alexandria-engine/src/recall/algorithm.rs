use super::scope_handle::ScopeHandle;
use crate::clusters::ClusterInfo;
use crate::search::{cosine_similarity, rank_by_similarity};

use std::time::{SystemTime, UNIX_EPOCH};

/// A memory's summary for recall results.
#[derive(Debug, Clone)]
pub struct FactSummary {
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub heat: f64,
}

/// A cluster with its member facts loaded.
#[derive(Debug, Clone)]
pub struct ClusterWithMembers {
    pub info: ClusterInfo,
    pub members: Vec<FactSummary>,
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
#[derive(Debug)]
pub struct FocusedRecallResult {
    pub memories: Vec<MemoryResult>,
}

/// Broad recall: find top matching clusters for a query.
///
/// For each cluster, check centroid similarity, verify against actual members,
/// then rank by best_member_sim × cluster_heat. Return top `limit` clusters
/// with scope handles for narrowing.
pub fn broad_recall(
    query_embedding: &[f32],
    clusters: &[ClusterWithMembers],
    limit: usize,
) -> BroadRecallResult {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut matches: Vec<ClusterMatch> = clusters
        .iter()
        .filter_map(|cwm| {
            let centroid_sim = cosine_similarity(query_embedding, &cwm.info.centroid);
            if centroid_sim < 0.3 {
                return None; // skip clusters with very low centroid match
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

            // Build scope handle
            let handle = ScopeHandle {
                cluster_id: cwm.info.id.clone(),
                depth: 0,
                query_embedding: query_embedding.to_vec(),
                issued_at: now,
            };

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

/// Focused recall: narrow within a specific cluster using a scope handle.
pub fn focused_recall(
    query_embedding: &[f32],
    _scope: &ScopeHandle,
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

    FocusedRecallResult { memories }
}
