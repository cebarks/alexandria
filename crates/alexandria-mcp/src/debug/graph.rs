use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::response::{Html, Json};

use super::html::{esc, layout};
use crate::server::record_id_to_string;
use crate::AlexandriaServer;

/// JSON node/edge data backing the graph page, shaped for vis-network's DataSet format.
pub async fn api_graph(
    State(server): State<AlexandriaServer>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());

    // Collect the node set: the center plus everything within 2 hops.
    let mut node_ids: HashSet<String> = HashSet::new();
    node_ids.insert(id.clone());
    let neighbors = edge_repo.get_neighbors(&id, 2).await.unwrap_or_default();
    for n in &neighbors {
        node_ids.insert(record_id_to_string(&n.id));
    }

    // Collect edges among the node set by querying each node's direct edges and
    // keeping only those whose both endpoints are in `node_ids` (dedup by in/out/type).
    let mut edges_seen: HashSet<(String, String, String)> = HashSet::new();
    let mut edges_json = Vec::new();
    for node_id in &node_ids {
        let edges = edge_repo.get_edges_for(node_id).await.unwrap_or_default();
        for e in edges {
            let from = e
                .in_node
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default();
            let to = e
                .out_node
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default();
            if !node_ids.contains(&from) || !node_ids.contains(&to) {
                continue;
            }
            let key = (from.clone(), to.clone(), e.edge_type.clone());
            if !edges_seen.insert(key) {
                continue;
            }
            edges_json.push(serde_json::json!({
                "from": from,
                "to": to,
                "label": e.edge_type,
            }));
        }
    }

    let nodes_json: Vec<serde_json::Value> = node_ids
        .iter()
        .map(|nid| {
            serde_json::json!({
                "id": nid,
                "label": nid,
            })
        })
        .collect();

    Json(serde_json::json!({
        "nodes": nodes_json,
        "edges": edges_json,
    }))
}

pub async fn page(Path(id): Path<String>) -> Html<String> {
    let id_esc = esc(&id);
    let body = format!(
        r##"<h1>Graph: {id_esc}</h1>
<div id="graph" style="height: 600px; border: 1px solid #30363d;"></div>
<script src="https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js"></script>
<script>
fetch("/debug/api/graph/{id_esc}")
  .then(r => r.json())
  .then(data => {{
    const nodes = new vis.DataSet(data.nodes);
    const edges = new vis.DataSet(data.edges);
    const container = document.getElementById("graph");
    new vis.Network(container, {{ nodes, edges }}, {{
      nodes: {{ color: "#58a6ff", font: {{ color: "#c9d1d9" }} }},
      edges: {{ color: "#30363d", font: {{ color: "#8b949e" }}, arrows: "to" }},
    }});
  }});
</script>"##
    );
    Html(layout("Graph", &body))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_api_graph_includes_nodes_and_edge() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());

        let a = memory_repo
            .create_fact("node a", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let b = memory_repo
            .create_fact("node b", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        edge_repo
            .create_edge(&a, &b, "relates_to", 1.0)
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/api/graph/{}", a.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let nodes = json["nodes"].as_array().unwrap();
        let edges = json["edges"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["label"], "relates_to");
    }
}
