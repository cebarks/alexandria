use axum::extract::State;
use axum::response::Html;
use axum::Form;
use serde::Deserialize;

use super::html::{esc, layout};
use crate::tools::{RecallParams, RetrieveMemoriesParams};
use crate::AlexandriaServer;

pub async fn form(State(_server): State<AlexandriaServer>) -> Html<String> {
    let body = r##"<h1>Query Tester</h1>
<form hx-post="/debug/query/run" hx-target="#query-results">
<label>Mode:
<select name="mode">
<option value="retrieve">retrieve_memories</option>
<option value="recall">recall</option>
</select>
</label>
<input type="text" name="query" placeholder="query text" required>
<label>Limit (retrieve only): <input type="number" name="limit" value="10" min="1"></label>
<button type="submit">Run</button>
</form>
<div id="query-results"></div>"##;
    Html(layout("Query Tester", body))
}

#[derive(Debug, Deserialize)]
pub struct QueryForm {
    pub mode: String,
    pub query: String,
    pub limit: Option<usize>,
}

pub async fn run(
    State(server): State<AlexandriaServer>,
    Form(form): Form<QueryForm>,
) -> Html<String> {
    let body = match form.mode.as_str() {
        "retrieve" => {
            let params = RetrieveMemoriesParams {
                query: form.query.clone(),
                limit: form.limit,
                session_id: None,
            };
            match server.do_retrieve_memories(params).await {
                Ok(value) => render_retrieve_results(&value),
                Err(e) => format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
            }
        }
        // recall mode intentionally ignores `limit` — RecallParams has no limit field.
        "recall" => {
            let params = RecallParams {
                query: form.query.clone(),
                scope_handle: None,
            };
            match server.do_recall(params).await {
                Ok(json_str) => render_recall_results(&json_str),
                Err(e) => format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
            }
        }
        other => format!(r#"<p class="error">Unknown mode: {}</p>"#, esc(other)),
    };
    Html(body)
}

fn render_retrieve_results(value: &serde_json::Value) -> String {
    let empty = Vec::new();
    let results = value
        .get("results")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty);

    if results.is_empty() {
        return "<p>No results.</p>".to_string();
    }

    let mut rows = String::new();
    for r in results {
        let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let similarity = r.get("similarity").and_then(|v| v.as_f64()).unwrap_or(0.0);
        rows.push_str(&format!(
            r#"<tr><td><a class="link" href="/debug/memories/{}">{}</a></td><td>{}</td><td>{:.4}</td></tr>"#,
            esc(id),
            esc(id),
            esc(content),
            similarity,
        ));
    }

    format!(r#"<table><tr><th>ID</th><th>Content</th><th>Similarity</th></tr>{rows}</table>"#)
}

fn render_recall_results(json_str: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            return format!(
                r#"<p class="error">Failed to parse recall response: {}</p>"#,
                esc(&e.to_string())
            )
        }
    };

    let mode = value.get("mode").and_then(|v| v.as_str()).unwrap_or("");

    if mode == "focused" {
        let empty = Vec::new();
        let memories = value
            .get("memories")
            .and_then(|m| m.as_array())
            .unwrap_or(&empty);
        if memories.is_empty() {
            return "<p>No memories in this scope.</p>".to_string();
        }
        let mut rows = String::new();
        for m in memories {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let similarity = m.get("similarity").and_then(|v| v.as_f64()).unwrap_or(0.0);
            rows.push_str(&format!(
                r#"<tr><td><a class="link" href="/debug/memories/{}">{}</a></td><td>{}</td><td>{:.4}</td></tr>"#,
                esc(id),
                esc(id),
                esc(content),
                similarity,
            ));
        }
        format!(
            r#"<h3>Focused recall</h3><table><tr><th>ID</th><th>Content</th><th>Similarity</th></tr>{rows}</table>"#
        )
    } else {
        let empty = Vec::new();
        let clusters = value
            .get("clusters")
            .and_then(|c| c.as_array())
            .unwrap_or(&empty);
        if clusters.is_empty() {
            return "<p>No clusters found.</p>".to_string();
        }
        let mut sections = String::new();
        for c in clusters {
            let cluster_id = c.get("cluster_id").and_then(|v| v.as_str()).unwrap_or("");
            let similarity = c.get("similarity").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let mems_empty = Vec::new();
            let mems = c
                .get("representative_memories")
                .and_then(|m| m.as_array())
                .unwrap_or(&mems_empty);
            let mut mem_items = String::new();
            for m in mems {
                let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                mem_items.push_str(&format!("<li>{}</li>", esc(content)));
            }
            sections.push_str(&format!(
                r#"<div><h3>Cluster {} (similarity {:.4})</h3><ul>{mem_items}</ul></div>"#,
                esc(cluster_id),
                similarity,
            ));
        }
        format!("<h3>Broad recall</h3>{sections}")
    }
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
