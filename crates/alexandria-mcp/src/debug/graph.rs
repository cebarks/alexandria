use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Json, Response};

use crate::AlexandriaServer;
use crate::server::record_id_to_string;

/// Node labels are a content snippet this long; record ids move to the tooltip.
const LABEL_MAX_CHARS: usize = 40;
/// Hop radius used when `?hops` is absent or unparseable — the radius the page shipped before
/// the parameter existed, so the default view does not change.
pub const DEFAULT_HOPS: u32 = 2;
/// Ceiling on the hop radius. Memory graphs are dense, so `?hops=99` must not turn into an
/// unbounded traversal of the whole database.
const MAX_HOPS: u32 = 3;
/// Drop everything past `cap` nodes from an already-sorted node list, reporting whether anything
/// was dropped. Split out because the alternative — testing through `api_graph` — would need a
/// 201-node fixture, and making the cap injectable in production just for a test is worse.
fn apply_node_cap(ordered: &mut Vec<(u32, String)>, cap: usize) -> bool {
    let truncated = ordered.len() > cap;
    ordered.truncate(cap);
    truncated
}

/// Cap on how many nodes one graph view will draw. Beyond it the ego-graph stops expanding and
/// the page says so: a few hundred nodes on a canvas is a hairball, not a picture, and the
/// client would pay for it in layout time.
const GRAPH_NODE_CAP: usize = 200;

/// Effective hop radius for a raw `?hops` value. Absent, empty, non-numeric and out-of-range all
/// resolve inside `1..=MAX_HOPS`, so no input can ask for a wider traversal than the cap allows.
fn resolve_hops(raw: Option<&str>) -> u32 {
    match raw.and_then(|value| value.trim().parse::<u32>().ok()) {
        Some(hops) => hops.clamp(1, MAX_HOPS),
        None => DEFAULT_HOPS,
    }
}

/// Node type, taken from the table part of the record id (`<table>:<key>`). Returns `""` for an
/// id with no separator rather than panicking — the graph still has to render.
fn node_table_of(id: &str) -> &str {
    id.split_once(':').map(|(table, _)| table).unwrap_or("")
}

/// Content snippet for a node label, following the `memories.rs` preview convention: whitespace
/// collapsed, cut at [`LABEL_MAX_CHARS`] in a single pass, `…` only when something was dropped.
fn label_snippet(content: &str) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let taken: String = chars.by_ref().take(LABEL_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// Width/opacity band for an edge, derived from its strength.
///
/// Strengths are not comparable across edge types — `relates_to` stores a 0..=1 similarity while
/// `contains_session_memory` stores an unbounded counter — so they are banded into three steps
/// rather than scaled linearly. `""` means "no signal": unknown band, missing or zero strength,
/// and the client then falls back to the type's default styling.
fn strength_band(strength: f64) -> &'static str {
    if !strength.is_finite() || strength <= 0.0 {
        ""
    } else if strength < 0.34 {
        "faint"
    } else if strength < 0.67 {
        "medium"
    } else {
        "strong"
    }
}

/// Tooltip: everything a 40-character label gives up — the full record id and the hop distance.
fn node_tooltip(id: &str, hop: u32) -> String {
    let table = node_table_of(id);
    let kind = if table.is_empty() { "node" } else { table };
    format!("{id} — {kind}, hop {hop}")
}

/// JSON node/edge data backing the graph page, shaped for vis-network's DataSet format.
///
/// `?hops` is read out of a free-form map rather than a typed field so a junk value falls through
/// to [`DEFAULT_HOPS`] instead of failing the request with a 400.
pub async fn api_graph(
    State(server): State<AlexandriaServer>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    api_graph_with_radius(
        &server,
        &id,
        resolve_hops(query.get("hops").map(String::as_str)),
    )
    .await
}

/// Shared traversal, parameterised by radius. [`api_graph`] and the page's control resolve
/// `?hops` through the same [`resolve_hops`], so they cannot disagree about what is drawn.
async fn api_graph_with_radius(
    server: &AlexandriaServer,
    id: &str,
    hops: u32,
) -> Json<serde_json::Value> {
    let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());

    // Collect the node set: the center plus everything within `hops` hops.
    let mut node_ids: HashSet<String> = HashSet::new();
    let mut hop_of: HashMap<String, u32> = HashMap::new();
    node_ids.insert(id.to_string());
    hop_of.insert(id.to_string(), 0);
    let neighbors = edge_repo.get_neighbors(id, hops).await.unwrap_or_default();
    for n in &neighbors {
        let neighbor_id = record_id_to_string(&n.id);
        // A node reached by several paths keeps the shortest distance: `get_neighbors` reports a
        // node once per path, so the first `hop` seen is not necessarily the closest.
        let entry = hop_of.entry(neighbor_id.clone()).or_insert(n.hop);
        *entry = (*entry).min(n.hop);
        node_ids.insert(neighbor_id);
    }

    // Cap the node set before any per-node work happens, so an oversized ego-graph costs one
    // traversal rather than hundreds of queries. Ordered by hop then id so the cut is the same
    // set every time — a HashSet's iteration order would make the dropped half arbitrary.
    let mut ordered: Vec<(u32, String)> = node_ids
        .iter()
        .map(|nid| (hop_of.get(nid).copied().unwrap_or(hops), nid.clone()))
        .collect();
    ordered.sort();
    let truncated = apply_node_cap(&mut ordered, GRAPH_NODE_CAP);
    let node_ids: Vec<String> = ordered.into_iter().map(|(_, nid)| nid).collect();

    // TODO(debt): this handler costs one `get_edges_for` plus one `get_fact` per node, so the
    // round trips are O(nodes) — bounded now by GRAPH_NODE_CAP but still the first thing to
    // revisit if graph pages feel slow. A batch-by-ids read in storage would collapse both.
    //
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
                "edge_type": e.edge_type,
                "strength": e.strength,
                "strength_band": strength_band(e.strength),
                "edge_color": edge_style_of(&e.edge_type).0,
                "edge_dashes": dashes_json(edge_style_of(&e.edge_type).1),
            }));
        }
    }

    let content = load_fact_content(server, &node_ids).await;
    let nodes_json: Vec<serde_json::Value> = node_ids
        .iter()
        .map(|nid| {
            let table = node_table_of(nid);
            let (shape, color) = node_style_of(table);
            let hop = hop_of.get(nid).copied().unwrap_or(hops);
            serde_json::json!({
                "id": nid,
                // Facts get a content snippet; clusters, raw documents and any table the graph
                // has not been taught about keep their record id, so no node goes unlabeled.
                "label": content
                    .get(nid)
                    .map(|text| label_snippet(text))
                    .unwrap_or_else(|| nid.clone()),
                "table": table,
                // Shape *and* colour are keyed on the table, so a node's kind survives a
                // colour-blind viewer or a grey-scale print.
                "shape": shape,
                "color": color,
                "hop": hop,
                "title": node_tooltip(nid, hop),
            })
        })
        .collect();

    Json(serde_json::json!({
        "nodes": nodes_json,
        "edges": edges_json,
        "centre": id,
        "hops": hops,
        "node_count": node_ids.len(),
        "node_cap": GRAPH_NODE_CAP,
        "truncated": truncated,
        // Legend data, derived from the same consts that style the canvas.
        "node_types": node_type_legend(),
        "edge_types": edge_type_legend(),
    }))
}

/// How each known node table is drawn: `(table, shape, colour)`.
///
/// This list is the only place those choices exist — it is emitted as `node_types` alongside the
/// nodes so the page's legend is generated from it rather than restated by hand.
const NODE_STYLES: [(&str, &str, &str); 3] = [
    ("fact", "dot", "#58a6ff"),
    ("cluster", "diamond", "#d29922"),
    ("raw", "square", "#3fb950"),
];

/// Everything unrecognised — an unknown table, or an id with no table at all — draws like this.
/// Deliberately not folded into `fact`, so a new edge endpoint type shows up as odd rather than
/// masquerading as a memory.
const UNKNOWN_NODE_STYLE: (&str, &str) = ("triangle", "#8b949e");

/// How each known edge type is drawn: `(edge_type, colour, dash)`. `None` dash means solid.
///
/// Open-ended on purpose: `edge_type` is a plain string column, so a type nobody has taught the
/// page about still renders, via [`UNKNOWN_EDGE_STYLE`].
const EDGE_STYLES: [(&str, &str, Option<[f64; 2]>); 4] = [
    ("relates_to", "#58a6ff", None),
    ("derived_from", "#f85149", Some([6.0, 4.0])),
    ("extracted_from", "#3fb950", Some([2.0, 4.0])),
    ("contains_session_memory", "#d2a8ff", Some([10.0, 3.0])),
];

const UNKNOWN_EDGE_STYLE: (&str, Option<[f64; 2]>) = ("#8b949e", Some([1.0, 3.0]));

/// Node shape and fill for a table name (see [`NODE_STYLES`]).
fn node_style_of(table: &str) -> (&'static str, &'static str) {
    NODE_STYLES
        .iter()
        .find(|(name, _, _)| *name == table)
        .map(|(_, shape, color)| (*shape, *color))
        .unwrap_or(UNKNOWN_NODE_STYLE)
}

/// Edge colour and dash pattern for an edge type (see [`EDGE_STYLES`]).
fn edge_style_of(edge_type: &str) -> (&'static str, Option<[f64; 2]>) {
    EDGE_STYLES
        .iter()
        .find(|(name, _, _)| *name == edge_type)
        .map(|(_, color, dash)| (*color, *dash))
        .unwrap_or(UNKNOWN_EDGE_STYLE)
}

/// vis-network's `dashes` value for a dash pattern: an array, or `false` for a solid line.
fn dashes_json(dash: Option<[f64; 2]>) -> serde_json::Value {
    match dash {
        Some([on, off]) => serde_json::json!([on, off]),
        None => serde_json::json!(false),
    }
}

/// Legend entries for node types, straight out of [`NODE_STYLES`] so they cannot drift.
fn node_type_legend() -> Vec<serde_json::Value> {
    NODE_STYLES
        .iter()
        .map(|(table, shape, color)| {
            serde_json::json!({"table": table, "shape": shape, "color": color})
        })
        .collect()
}

/// Legend entries for edge types, straight out of [`EDGE_STYLES`].
fn edge_type_legend() -> Vec<serde_json::Value> {
    EDGE_STYLES
        .iter()
        .map(|(edge_type, color, dash)| {
            serde_json::json!({
                "edge_type": edge_type,
                "color": color,
                "dashes": dashes_json(*dash),
            })
        })
        .collect()
}

/// Content for node labels, one [`MemoryRepo::get_fact`] per fact node.
///
/// `get_fact` is the only by-id read storage exposes, and `all_ids_and_content()` would load the
/// entire table to label a few dozen nodes. A failed lookup degrades that one node to its record
/// id rather than failing the graph: a missing label is cosmetic, a 500 is not.
async fn load_fact_content(
    server: &AlexandriaServer,
    node_ids: &[String],
) -> HashMap<String, String> {
    let fact_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
    let mut content = HashMap::new();
    for node_id in node_ids.iter().filter(|nid| node_table_of(nid) == "fact") {
        match fact_repo.get_fact(node_id).await {
            Ok(Some(fact)) => {
                content.insert(node_id.clone(), fact.content);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(node = %node_id, error = %e, "graph label lookup failed");
            }
        }
    }
    content
}

/// The graph page itself.
///
/// `id` is the centre record id only: the nodes and edges are fetched client-side from
/// [`api_graph`], so this handler does no database work and has no failure path to render. It
/// resolves `?hops` with the same [`resolve_hops`] the API uses, so the radius the control marks
/// active is the radius the client-side fetch will traverse.
#[derive(Template)]
#[template(path = "graph.html")]
struct GraphTemplate {
    nav: &'static str,
    id: String,
    hops: u32,
}

/// The renderer is called by full path because this handler is itself named `page` — a module
/// level `use super::html::page` would collide with it.
pub async fn page(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    super::html::page(GraphTemplate {
        // There is no "Graph" nav entry (the page is only reached from a memory detail page),
        // and inventing one is out of scope for a migration. "memories" is the section the
        // graph belongs to, which keeps the highlight honest without claiming a link exists.
        nav: "memories",
        id,
        hops: resolve_hops(query.get("hops").map(String::as_str)),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        DEFAULT_HOPS, EDGE_STYLES, GRAPH_NODE_CAP, GraphTemplate, MAX_HOPS, NODE_STYLES,
        UNKNOWN_EDGE_STYLE, UNKNOWN_NODE_STYLE, apply_node_cap, edge_style_of, label_snippet,
        node_style_of, node_table_of, resolve_hops, strength_band,
    };
    use crate::AlexandriaServer;
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
            hops: super::DEFAULT_HOPS,
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
            html.contains(r#"fetch("/debug/api/graph/fact%3Aabc?hops=2")"#),
            "id must be urlencoded inside the JS string literal; got: {html}"
        );
        // The radius reaches the request as a query parameter, so a page rendered for a clamped
        // radius cannot fetch a different one.
        assert!(
            html.contains(r#"href="/debug/graph/fact%3Aabc?hops=3""#),
            "hop control links must urlencode the id in the href too; got: {html}"
        );
        // Click-through must escape in JS, against the id that arrived as parsed JSON.
        assert!(
            html.contains(r#""/debug/memories/" + encodeURIComponent(node.id)"#),
            "fact nodes must navigate to the memory detail page; got: {html}"
        );
        assert!(
            !html.contains("unpkg.com") && !html.contains("|safe"),
            "no CDN reference and no |safe in the rendered page"
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

    async fn graph_json(server: &AlexandriaServer, centre: &str, query: &str) -> serde_json::Value {
        let app = crate::debug::router(server.clone());
        let uri = format!("/debug/api/graph/{}{query}", centre.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn node_by_id<'a>(json: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
        json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == serde_json::json!(id))
            .unwrap_or_else(|| panic!("no node {id} in {}", json["nodes"]))
    }

    /// The whole point of the change: a node is labelled by what it says, not by its primary key.
    #[tokio::test]
    async fn test_api_graph_labels_facts_with_content_not_record_ids() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());

        let long = "SurrealDB rejects a RELATE whose record id is not pre-parsed,                     so every edge helper binds a RecordId before building the query."
            .to_string();
        let centre = memory_repo
            .create_fact(&long, 0.9, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let neighbour = memory_repo
            .create_fact("short fact about kelp forests", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        edge_repo
            .create_edge(&centre, &neighbour, "relates_to", 0.75)
            .await
            .unwrap();

        let json = graph_json(&server, &centre, "").await;

        let centre_node = node_by_id(&json, &centre);
        assert_ne!(
            centre_node["label"],
            serde_json::json!(centre),
            "a fact node must not be labelled with its own record id"
        );
        assert_eq!(
            centre_node["label"],
            serde_json::json!(label_snippet(&long)),
            "label must be the content snippet"
        );
        assert!(
            !centre_node["label"].as_str().unwrap().contains("RecordId"),
            "snippet must be bounded: got {:?}",
            centre_node["label"]
        );
        assert!(
            centre_node["label"].as_str().unwrap().ends_with('…'),
            "a truncated snippet must say so"
        );
        assert_eq!(centre_node["table"], serde_json::json!("fact"));
        assert_eq!(centre_node["hop"], serde_json::json!(0));
        assert_eq!(
            centre_node["title"],
            serde_json::json!(format!("{centre} — fact, hop 0")),
            "the full id belongs in the tooltip, where the short label gives it up"
        );

        let neighbour_node = node_by_id(&json, &neighbour);
        assert_eq!(
            neighbour_node["label"],
            serde_json::json!("short fact about kelp forests")
        );
        assert_eq!(neighbour_node["hop"], serde_json::json!(1));
        assert!(
            neighbour_node["title"]
                .as_str()
                .unwrap()
                .contains(&neighbour),
            "tooltip must carry the full record id: got {:?}",
            neighbour_node["title"]
        );
    }

    /// Clusters and raw documents reach the graph too, and `memory_edge` is a
    /// `FROM fact TO fact` relation, so the reliable way to draw a non-fact node is to centre the
    /// ego-graph on one. A node whose table is unknown — or which has no table separator at all —
    /// must still render, labelled with its own id rather than panicking or going blank.
    #[tokio::test]
    async fn test_api_graph_falls_back_to_id_for_non_facts_and_unknown_tables() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
        let fact = memory_repo
            .create_fact("the fact content is not its id", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let other = memory_repo
            .create_fact("a second fact", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        edge_repo
            .create_edge(&fact, &other, "relates_to", 1.0)
            .await
            .unwrap();

        // Table extraction is a pure function over the id, so the id shapes that cannot be
        // stored here (unknown table, no separator) are exercised directly.
        assert_eq!(node_table_of("fact:abc"), "fact");
        assert_eq!(node_table_of("cluster:9"), "cluster");
        assert_eq!(node_table_of("raw:doc1"), "raw");
        assert_eq!(node_table_of("widget:7"), "widget");
        assert_eq!(node_table_of("no-separator-here"), "");
        assert_eq!(node_table_of(""), "", "an empty id must not panic");

        for (node_id, table) in [
            ("cluster:legend", "cluster"),
            ("raw:doc1", "raw"),
            ("widget:mystery", "widget"),
            ("malformed-id", ""),
        ] {
            let json = graph_json(&server, node_id, "").await;
            let node = node_by_id(&json, node_id);
            assert_eq!(
                node["label"],
                serde_json::json!(node_id),
                "a non-fact node must fall back to its record id"
            );
            assert_eq!(node["table"], serde_json::json!(table));
            assert_eq!(node["hop"], serde_json::json!(0));
            assert!(
                node["title"].as_str().unwrap().contains(node_id),
                "the tooltip must still carry the id: got {:?}",
                node["title"]
            );
        }

        // Content labelling and id fallback coexist in one response.
        let json = graph_json(&server, &fact, "").await;
        assert_eq!(
            node_by_id(&json, &fact)["label"],
            serde_json::json!("the fact content is not its id")
        );
        assert_eq!(node_by_id(&json, &fact)["table"], serde_json::json!("fact"));
        assert_eq!(
            node_by_id(&json, &other)["label"],
            serde_json::json!("a second fact"),
            "a hop-1 fact is labelled from content too"
        );
        assert_eq!(node_by_id(&json, &other)["hop"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn test_api_graph_edges_carry_type_and_strength() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
        let a = memory_repo
            .create_fact("edge styling endpoint a", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let b = memory_repo
            .create_fact("edge styling endpoint b", 0.5, &[0.7, 0.8], &[])
            .await
            .unwrap();
        edge_repo
            .create_edge(&a, &b, "derived_from", 0.42)
            .await
            .unwrap();

        let json = graph_json(&server, &a, "").await;
        let edges = json["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["edge_type"], serde_json::json!("derived_from"));
        assert_eq!(edges[0]["strength"], serde_json::json!(0.42));
        assert_eq!(edges[0]["label"], serde_json::json!("derived_from"));
    }

    /// `?hops=99` is the case that matters: the parameter must never widen the traversal past the
    /// ceiling, and junk must never break the page.
    #[test]
    fn test_resolve_hops_clamps_and_defaults() {
        assert_eq!(resolve_hops(None), 2);
        assert_eq!(resolve_hops(Some("1")), 1);
        assert_eq!(resolve_hops(Some("2")), 2);
        assert_eq!(resolve_hops(Some("3")), 3);
        assert_eq!(
            resolve_hops(Some("99")),
            3,
            "99 must be clamped, not honoured"
        );
        // A number too large for `u32` never parses, so it is junk like any other and takes the
        // default — the point is that it cannot overflow into a wide traversal.
        assert_eq!(resolve_hops(Some("4294967296")), DEFAULT_HOPS);
        assert!(resolve_hops(Some("99999999999999999999999")) <= MAX_HOPS);
        assert_eq!(
            resolve_hops(Some("0")),
            1,
            "0 must clamp up, not mean 'no traversal'"
        );
        assert_eq!(resolve_hops(Some("-1")), 2);
        assert_eq!(resolve_hops(Some("abc")), 2);
        assert_eq!(resolve_hops(Some("")), 2);
        assert_eq!(resolve_hops(Some("   ")), 2);
        assert_eq!(resolve_hops(Some("2.5")), 2);
    }

    #[tokio::test]
    async fn test_api_graph_hops_parameter_is_clamped_end_to_end() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());

        // A chain of six facts: only a radius >= 5 could reach the far end, so seeing node 6
        // proves the clamp was ignored.
        let mut ids = Vec::new();
        for index in 0..6 {
            ids.push(
                memory_repo
                    .create_fact(&format!("chain node {index}"), 0.5, &[0.1, 0.2], &[])
                    .await
                    .unwrap(),
            );
        }
        for pair in ids.windows(2) {
            edge_repo
                .create_edge(&pair[0], &pair[1], "relates_to", 1.0)
                .await
                .unwrap();
        }

        for (query, expected) in [
            ("", 2u32),
            ("?hops=1", 1),
            ("?hops=3", 3),
            ("?hops=99", 3),
            ("?hops=0", 1),
            ("?hops=abc", 2),
        ] {
            let json = graph_json(&server, &ids[0], query).await;
            assert_eq!(
                json["hops"],
                serde_json::json!(expected),
                "effective radius wrong for {query:?}"
            );
            let nodes = json["nodes"].as_array().unwrap();
            let reached: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
            assert!(
                !reached.contains(&ids[5].as_str()),
                "{query:?} must not reach 5 hops away (got {} nodes)",
                nodes.len()
            );
        }

        // The radius actually narrows the view: 1 hop draws strictly fewer nodes than 3.
        let one = graph_json(&server, &ids[0], "?hops=1").await;
        let three = graph_json(&server, &ids[0], "?hops=3").await;
        assert!(
            one["nodes"].as_array().unwrap().len() < three["nodes"].as_array().unwrap().len(),
            "hops=1 should draw a smaller ego-graph than hops=3"
        );
    }

    #[test]
    fn test_label_snippet_bounds_and_marks() {
        assert_eq!(label_snippet("  short   one  "), "short one");
        assert_eq!(label_snippet(""), "");
        let exact = "a".repeat(40);
        assert_eq!(
            label_snippet(&exact),
            exact,
            "exactly 40 chars is not truncated"
        );
        let over = format!("{}b", "c".repeat(40));
        assert_eq!(label_snippet(&over), format!("{}…", "c".repeat(40)));
        // Multi-byte content must be sliced by characters, not bytes.
        assert_eq!(
            label_snippet(&"é".repeat(45)),
            format!("{}…", "é".repeat(40)),
            "must not split a multi-byte char"
        );
        let tabbed = String::from("newlines\tand   tabs   collapse into a single label line");
        let snippet = label_snippet(&tabbed);
        assert!(
            !snippet.contains('\t') && !snippet.contains("  "),
            "whitespace must collapse so a label fits one line: {snippet:?}"
        );
    }
    /// The encoding has to be readable without colour, and an unknown type must degrade to a
    /// default rather than vanish or panic.
    #[test]
    fn test_node_and_edge_styles_cover_known_types_and_default_the_rest() {
        assert_eq!(node_style_of("fact"), ("dot", "#58a6ff"));
        assert_eq!(node_style_of("cluster"), ("diamond", "#d29922"));
        assert_eq!(node_style_of("raw"), ("square", "#3fb950"));
        assert_eq!(
            node_style_of("widget"),
            UNKNOWN_NODE_STYLE,
            "an unknown table must fall back to one shared style, not to `fact`"
        );
        assert_eq!(node_style_of(""), UNKNOWN_NODE_STYLE);

        // Three distinguishable shapes plus one for the unknowns: the kinds are tellable apart
        // without relying on colour.
        let shapes: HashSet<&str> = ["fact", "cluster", "raw", "widget"]
            .map(|table| node_style_of(table).0)
            .into_iter()
            .collect();
        assert_eq!(shapes.len(), 4, "shapes must distinguish every kind");
        let colors: HashSet<&str> = ["fact", "cluster", "raw"]
            .map(|table| node_style_of(table).1)
            .into_iter()
            .collect();
        assert_eq!(colors.len(), 3);

        assert_eq!(edge_style_of("relates_to"), ("#58a6ff", None));
        assert_eq!(edge_style_of("derived_from").1, Some([6.0, 4.0]));
        assert_eq!(edge_style_of("extracted_from").1, Some([2.0, 4.0]));
        assert_eq!(
            edge_style_of("contains_session_memory").1,
            Some([10.0, 3.0])
        );
        assert_eq!(
            edge_style_of("something_new"),
            UNKNOWN_EDGE_STYLE,
            "a type nobody taught the page about must still be drawable"
        );
        assert_ne!(
            edge_style_of("relates_to").0,
            edge_style_of("derived_from").0,
            "the two most common types must not look identical"
        );
    }

    #[test]
    fn test_strength_band_maps_strengths_to_widths() {
        assert_eq!(strength_band(0.9), "strong");
        assert_eq!(strength_band(0.67), "strong");
        assert_eq!(strength_band(0.5), "medium");
        assert_eq!(strength_band(0.34), "medium");
        assert_eq!(strength_band(0.01), "faint");
        // "" is the "no signal" band: the client then uses the type's default width.
        assert_eq!(strength_band(0.0), "");
        assert_eq!(strength_band(-3.0), "");
        assert_eq!(strength_band(f64::NAN), "");
        assert_eq!(strength_band(f64::INFINITY), "");
        // An unbounded strength (session counters) saturates at the top band instead of asking
        // for an infinite line width.
        assert_eq!(strength_band(19_000.0), "strong");
    }

    #[tokio::test]
    async fn test_api_graph_emits_style_data_and_legend_entries() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
        let a = memory_repo
            .create_fact("styled node alpha", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let b = memory_repo
            .create_fact("styled node beta", 0.5, &[0.3, 0.4], &[])
            .await
            .unwrap();
        edge_repo
            .create_edge(&a, &b, "extracted_from", 0.9)
            .await
            .unwrap();

        let json = graph_json(&server, &a, "").await;
        let node = node_by_id(&json, &a);
        assert_eq!(node["shape"], serde_json::json!("dot"));
        assert_eq!(node["color"], serde_json::json!("#58a6ff"));
        let edge = &json["edges"][0];
        assert_eq!(edge["edge_type"], serde_json::json!("extracted_from"));
        assert_eq!(edge["edge_color"], serde_json::json!("#3fb950"));
        assert_eq!(edge["edge_dashes"], serde_json::json!([2.0, 4.0]));
        assert_eq!(edge["strength_band"], serde_json::json!("strong"));

        // The legend is generated from the server's own style tables, so it cannot drift, and the
        // colour it advertises is the colour that was drawn.
        let node_types = json["node_types"].as_array().unwrap();
        assert_eq!(node_types.len(), NODE_STYLES.len());
        assert_eq!(node_types[0]["table"], serde_json::json!("fact"));
        assert_eq!(node_types[0]["color"], node["color"]);
        let edge_types = json["edge_types"].as_array().unwrap();
        assert_eq!(edge_types.len(), EDGE_STYLES.len());
        assert!(
            edge_types
                .iter()
                .any(|t| t["edge_type"] == serde_json::json!("contains_session_memory"))
        );
    }

    #[test]
    fn test_apply_node_cap_keeps_the_nearest_nodes() {
        // Sorted by (hop, id) upstream, so truncating keeps the closest nodes and drops the far
        // ones — the same set every time, not whatever a HashSet happened to yield first.
        let mut ordered: Vec<(u32, String)> = [
            (0, "fact:a".to_string()),
            (1, "fact:b".to_string()),
            (1, "fact:c".to_string()),
            (2, "fact:d".to_string()),
            (2, "fact:e".to_string()),
        ]
        .into();
        assert!(
            !apply_node_cap(&mut ordered, 5),
            "at the cap nothing is dropped"
        );
        assert_eq!(ordered.len(), 5);
        assert!(apply_node_cap(&mut ordered, 3), "past the cap it is");
        assert_eq!(
            ordered
                .iter()
                .map(|(_, id)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["fact:a", "fact:b", "fact:c"],
            "the far nodes must be the ones dropped"
        );
        assert!(
            !apply_node_cap(&mut ordered, 3),
            "a set already at the cap is not truncated"
        );
        assert!(
            !apply_node_cap(&mut Vec::new(), 0),
            "an empty set is never over the cap"
        );
    }

    /// The live path reports the cap it used and that it did not bite. The truncation flag itself
    /// is asserted through [`apply_node_cap`] instead: exceeding 200 nodes end-to-end would mean
    /// a 201-memory fixture, and the cap is deliberately not injectable in production.
    #[tokio::test]
    async fn test_api_graph_reports_cap_state_when_not_truncated() {
        let server = super::super::test_support::test_server().await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let fact = memory_repo
            .create_fact("uncapped small graph", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let json = graph_json(&server, &fact, "").await;
        assert_eq!(json["truncated"], serde_json::json!(false));
        assert_eq!(json["node_count"], serde_json::json!(1));
        assert_eq!(json["node_cap"], serde_json::json!(GRAPH_NODE_CAP));
    }
}
