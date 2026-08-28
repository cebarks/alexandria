use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};
use surrealdb::Surreal;

use crate::models::MemoryEdge;

/// Neighbor with hop distance for multi-hop traversal.
#[derive(Debug, Clone)]
pub struct Neighbor {
    pub id: RecordId,
    pub hop: u32,
    pub edge_type: String,
    pub strength: f64,
}

pub struct EdgeRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> EdgeRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    /// Create a directed edge between two facts.
    pub async fn create_edge(
        &self,
        from: &str,
        to: &str,
        edge_type: &str,
        strength: f64,
    ) -> Result<()> {
        let from_id = RecordId::parse_simple(from)?;
        let to_id = RecordId::parse_simple(to)?;
        self.db
            .query(
                "RELATE $from->memory_edge->$to \
                 SET edge_type = $edge_type, strength = $strength",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("edge_type", edge_type.to_string()))
            .bind(("strength", strength))
            .await?
            .check()?;
        Ok(())
    }

    /// Get all edges originating from or pointing to a memory.
    pub async fn get_edges_for(&self, id: &str) -> Result<Vec<MemoryEdge>> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM memory_edge WHERE in = type::record($id) OR out = type::record($id)",
            )
            .bind(("id", id.to_string()))
            .await?;
        let edges: Vec<MemoryEdge> = response.take(0)?;
        Ok(edges)
    }

    /// Get direct neighbors (1 hop) of a memory.
    pub async fn get_direct_neighbors(&self, id: &str) -> Result<Vec<Neighbor>> {
        // Outgoing edges: id -> neighbor
        let mut out_response = self
            .db
            .query("SELECT out, edge_type, strength FROM memory_edge WHERE in = type::record($id)")
            .bind(("id", id.to_string()))
            .await?;
        let outgoing: Vec<NeighborRow> = out_response.take(0)?;

        // Incoming edges: neighbor -> id
        let mut in_response = self
            .db
            .query(
                "SELECT in AS node, edge_type, strength FROM memory_edge WHERE out = type::record($id)",
            )
            .bind(("id", id.to_string()))
            .await?;
        let incoming: Vec<NeighborRow> = in_response.take(0)?;

        let mut neighbors = Vec::new();
        for row in outgoing {
            if let Some(node) = row.out {
                neighbors.push(Neighbor {
                    id: node,
                    hop: 1,
                    edge_type: row.edge_type,
                    strength: row.strength,
                });
            }
        }
        for row in incoming {
            if let Some(node) = row.node {
                neighbors.push(Neighbor {
                    id: node,
                    hop: 1,
                    edge_type: row.edge_type,
                    strength: row.strength,
                });
            }
        }

        Ok(neighbors)
    }

    /// Get neighbors up to max_hops away via BFS.
    pub async fn get_neighbors(&self, id: &str, max_hops: u32) -> Result<Vec<Neighbor>> {
        let mut all_neighbors = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let start_id = surrealdb::types::RecordId::parse_simple(id)?;
        visited.insert(format!("{:?}", start_id));

        let mut frontier = vec![(id.to_string(), 0u32)];

        while let Some((current_id, current_hop)) = frontier.pop() {
            if current_hop >= max_hops {
                continue;
            }

            let direct = self.get_direct_neighbors(&current_id).await?;
            for neighbor in direct {
                let neighbor_key = format!("{:?}", neighbor.id);
                if visited.contains(&neighbor_key) {
                    continue;
                }
                visited.insert(neighbor_key);

                let neighbor_id_str = format!(
                    "{}:{}",
                    neighbor.id.table,
                    match &neighbor.id.key {
                        surrealdb::types::RecordIdKey::String(s) => s.clone(),
                        surrealdb::types::RecordIdKey::Number(n) => n.to_string(),
                        other => format!("{other:?}"),
                    }
                );

                all_neighbors.push(Neighbor {
                    id: neighbor.id,
                    hop: current_hop + 1,
                    edge_type: neighbor.edge_type,
                    strength: neighbor.strength,
                });

                frontier.push((neighbor_id_str, current_hop + 1));
            }
        }

        Ok(all_neighbors)
    }
}

#[derive(Debug, serde::Deserialize, surrealdb::types::SurrealValue)]
struct NeighborRow {
    #[serde(default)]
    out: Option<RecordId>,
    #[serde(default)]
    node: Option<RecordId>,
    edge_type: String,
    strength: f64,
}
