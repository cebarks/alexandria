use std::collections::HashSet;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{Json, Response};

use crate::AlexandriaServer;
use crate::server::record_id_to_string;

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

/// The graph page itself.
///
/// `id` is the centre record id only: the nodes and edges are fetched client-side from
/// [`api_graph`], so this handler does no database work and has no failure path to render.
#[derive(Template)]
#[template(path = "graph.html")]
struct GraphTemplate {
    nav: &'static str,
    id: String,
}

/// The renderer is called by full path because this handler is itself named `page` — a module
/// level `use super::html::page` would collide with it.
pub async fn page(Path(id): Path<String>) -> Response {
    super::html::page(GraphTemplate {
        // There is no "Graph" nav entry (the page is only reached from a memory detail page),
        // and inventing one is out of scope for a migration. "memories" is the section the
        // graph belongs to, which keeps the highlight honest without claiming a link exists.
        nav: "memories",
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::GraphTemplate;
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// The graph page is the migration's most subtle template and had zero render coverage:
    /// its only other test exercises the JSON API, never `GraphTemplate`. Everything this pins
    /// fails silently in a browser while `just test` stays green.
    #[test]
    fn test_graph_template_embeds_vendored_vis_network_not_a_cdn() {
        let html = GraphTemplate {
            nav: "memories",
            id: "fact:abc".into(),
        }
        .render()
        .unwrap();

        // Drift seam: assets.rs exports the path, this template hard-codes it. layout.html has
        // the htmx equivalent guard; without this, a vis-network bump that misses the template
        // 404s the script and the page renders blank with `vis is not defined`.
        assert!(
            html.contains(super::super::assets::VIS_NETWORK_URL),
            "got: {html}"
        );
        assert!(!html.contains("unpkg.com"), "no CDN references may remain");

        // Escaping CONTEXT, the subtlest decision in the migration. Inside a JS string literal
        // browsers do not decode character references, so the id must be percent-encoded; the
        // <h1> is ordinary HTML text, so it must not be.
        assert!(
            html.contains(r#"fetch("/debug/api/graph/fact%3Aabc")"#),
            "id must be urlencoded inside the JS string literal; got: {html}"
        );
        assert!(
            html.contains("<h1>Graph: fact:abc</h1>"),
            "h1 is HTML text context and must stay unencoded; got: {html}"
        );

        // Load order: layout.html defers htmx in <head>, and a deferred script runs AFTER an
        // inline body script. vis-network must therefore load plain, and first.
        let lib = html
            .find(super::super::assets::VIS_NETWORK_URL)
            .expect("vis-network script tag missing");
        let inline = html.find("new vis.Network").expect("inline script missing");
        assert!(
            lib < inline,
            "vis-network must load before the inline script that uses `vis`"
        );
        assert!(
            html.contains(&format!(
                r#"<script src="{}"></script>"#,
                super::super::assets::VIS_NETWORK_URL
            )),
            "vis-network must load plain, with no defer, so it executes before the inline script; got: {html}"
        );

        // The container the inline script looks up, and it must precede the scripts block.
        let container = html
            .find(r#"<div id="graph""#)
            .expect("graph container missing");
        assert!(
            container < inline,
            "#graph div must exist before the script runs"
        );
    }

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
