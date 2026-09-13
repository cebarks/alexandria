//! The Query Tester: a form page plus the htmx fragment that answers it.
//!
//! Both live in templates: `query.html` is a normal page, `query_results.html` is a body
//! fragment (it does not extend `layout.html`). Because `run` only ever answers with a fragment,
//! its error paths are `Fragment::Error` — `html::error_page` would wrap the message in a whole
//! layout and swap a second `<nav>` into `#query-results`, which is not what this endpoint has
//! ever returned.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::Response;
use serde::Deserialize;

use super::html::page;
use crate::AlexandriaServer;
use crate::tools::{RecallParams, RetrieveMemoriesParams};

/// The form. `nav` is the only field: no values are pre-filled, the inputs are static markup
/// whose `name` attributes are what [`QueryForm`] deserializes.
#[derive(Template)]
#[template(path = "query.html")]
struct QueryTemplate {
    nav: &'static str,
}

pub async fn form(State(_server): State<AlexandriaServer>) -> Response {
    page(QueryTemplate { nav: "query" })
}

#[derive(Debug, Deserialize)]
pub struct QueryForm {
    pub mode: String,
    pub query: String,
    pub limit: Option<usize>,
}

/// One row of the ID / Content / Similarity table. `retrieve_memories` results and focused
/// recall share this shape — the legacy code rendered both with the same `format!` line.
struct ResultRow {
    id: String,
    content: String,
    similarity: String,
}

/// One cluster of a broad recall. Only the representative memory *contents* are listed: the
/// legacy renderer never showed the per-memory ids or similarities inside the `<ul>`, so this
/// drops nothing.
struct RecallCluster {
    id: String,
    similarity: String,
    contents: Vec<String>,
}

/// What `/debug/query/run` can answer — one variant per return branch of the two `format!`
/// renderers this replaces. An enum rather than a struct of optional fields so the fragment
/// cannot render two branches at once, and so the template's `{% match %}` is checked for
/// exhaustiveness at compile time.
enum Fragment {
    /// A tool error, an unknown `mode` value, or a recall payload that failed to parse.
    Error { message: String },
    /// A bare-`<p>` empty state ("No results." and friends), which legacy had no class for.
    Notice { message: String },
    /// The results table. `heading` is `Some("Focused recall")` for recall's focused mode;
    /// retrieve results had no heading at all.
    Table {
        heading: Option<&'static str>,
        rows: Vec<ResultRow>,
    },
    /// Broad recall: one section per cluster. The `<h3>Broad recall</h3>` lives in the template.
    Clusters { sections: Vec<RecallCluster> },
}

impl Fragment {
    fn error(message: String) -> Self {
        Self::Error { message }
    }

    fn notice(message: &str) -> Self {
        Self::Notice {
            message: message.to_string(),
        }
    }

    /// A `retrieve_memories` result: `{ "results": [ { id, content, similarity }, … ] }`.
    fn retrieve(value: &serde_json::Value) -> Self {
        let rows = result_rows(value.get("results"));
        if rows.is_empty() {
            Self::notice("No results.")
        } else {
            Self::Table {
                heading: None,
                rows,
            }
        }
    }

    /// A `recall` result. The tool hands back a JSON *string*, so it can still fail to parse
    /// here after a successful call — that is this function's own `Error` branch.
    fn recall(json_str: &str) -> Self {
        let value: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => return Self::error(format!("Failed to parse recall response: {e}")),
        };

        // The form never sends a scope handle, so in practice only "broad" arrives here, but
        // both branches are kept: a missing or unexpected `mode` falls through to broad, which
        // is what legacy did (`unwrap_or("")`), and broad with no clusters prints
        // "No clusters found." rather than an empty table.
        if value.get("mode").and_then(|v| v.as_str()).unwrap_or("") == "focused" {
            let rows = result_rows(value.get("memories"));
            if rows.is_empty() {
                Self::notice("No memories in this scope.")
            } else {
                Self::Table {
                    heading: Some("Focused recall"),
                    rows,
                }
            }
        } else {
            let empty = Vec::new();
            let clusters = value
                .get("clusters")
                .and_then(|c| c.as_array())
                .unwrap_or(&empty);
            let sections: Vec<RecallCluster> = clusters
                .iter()
                .map(|c| RecallCluster {
                    id: string_field(c, "cluster_id"),
                    similarity: similarity_field(c),
                    contents: c
                        .get("representative_memories")
                        .and_then(|m| m.as_array())
                        .unwrap_or(&empty)
                        .iter()
                        .map(|m| string_field(m, "content"))
                        .collect(),
                })
                .collect();
            if sections.is_empty() {
                Self::notice("No clusters found.")
            } else {
                Self::Clusters { sections }
            }
        }
    }
}

/// Rows for a JSON array of `{ id, content, similarity }` entries. A missing or non-array value
/// yields no rows, which the callers turn into their empty state — as legacy did.
fn result_rows(value: Option<&serde_json::Value>) -> Vec<ResultRow> {
    let empty = Vec::new();
    let entries = value.and_then(|v| v.as_array()).unwrap_or(&empty);
    entries
        .iter()
        .map(|entry| ResultRow {
            id: string_field(entry, "id"),
            content: string_field(entry, "content"),
            similarity: similarity_field(entry),
        })
        .collect()
}

/// A string field, empty when absent or not a string — the legacy renderers used
/// `unwrap_or("")` rather than skipping a malformed entry, so a bad row shows blank cells.
fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A `similarity` as the legacy `{:.4}` printed it. Formatting stays in Rust: askama renders an
/// `f64` with plain `Display`, which would print `0.5` where the table printed `0.5000`.
fn similarity_field(value: &serde_json::Value) -> String {
    let similarity = value
        .get("similarity")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    format!("{similarity:.4}")
}

/// The fragment template. No `nav` field, because `query_results.html` is swapped into a div
/// rather than extending `layout.html`.
#[derive(Template)]
#[template(path = "query_results.html")]
struct QueryResultsTemplate {
    fragment: Fragment,
}

pub async fn run(State(server): State<AlexandriaServer>, Form(form): Form<QueryForm>) -> Response {
    let fragment = match form.mode.as_str() {
        "retrieve" => {
            let params = RetrieveMemoriesParams {
                query: form.query.clone(),
                limit: form.limit,
                session_id: None,
            };
            match server.do_retrieve_memories(params).await {
                Ok(value) => Fragment::retrieve(&value),
                Err(e) => Fragment::error(e.to_string()),
            }
        }
        // recall mode intentionally ignores `limit` — RecallParams has no limit field.
        "recall" => {
            let params = RecallParams {
                query: form.query.clone(),
                scope_handle: None,
            };
            match server.do_recall(params).await {
                Ok(json_str) => Fragment::recall(&json_str),
                Err(e) => Fragment::error(e.to_string()),
            }
        }
        other => Fragment::error(format!("Unknown mode: {other}")),
    };
    page(QueryResultsTemplate { fragment })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_query_run_retrieve_mode_empty_db() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=retrieve&query=test&limit=5"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("No results"));
    }

    #[tokio::test]
    async fn test_query_run_retrieve_mode_finds_stored_fact() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("findable memory", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=retrieve&query=test&limit=5"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("findable memory"));
    }

    #[tokio::test]
    async fn test_query_run_recall_mode_without_limit() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=recall&query=test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("No clusters found") || text.contains("Broad recall"));
    }
}
