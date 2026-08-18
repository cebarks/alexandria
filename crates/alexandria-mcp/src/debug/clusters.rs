use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};

use super::html::{esc, layout};
use crate::server::record_id_to_string;
use crate::AlexandriaServer;

/// Default cohesion floor used only for the debug UI's health display.
/// `AlexandriaServer` doesn't carry the configured value, so this mirrors
/// `ClusterConfig::default().cohesion_floor` in `crates/alexandria/src/config.rs`.
const DISPLAY_COHESION_FLOOR: f32 = 0.6;

pub async fn list(State(server): State<AlexandriaServer>) -> Html<String> {
    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let clusters = match cluster_repo.list_with_counts().await {
        Ok(c) => c,
        Err(e) => {
            return Html(layout(
                "Clusters",
                &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
            ))
        }
    };

    let mut rows_html = String::new();
    for (cluster, count) in &clusters {
        let id = cluster
            .id
            .as_ref()
            .map(record_id_to_string)
            .unwrap_or_default();
        let label = cluster.label.as_deref().unwrap_or("(unlabeled)");
        rows_html.push_str(&format!(
            r#"<tr><td><a class="link" href="/debug/clusters/{id}">{id}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
            esc(label),
            count,
            cluster.depth,
        ));
    }

    let body = format!(
        r#"<h1>Clusters</h1>
<table>
<tr><th>ID</th><th>Label</th><th>Members</th><th>Depth</th></tr>
{rows_html}
</table>
<p>{} clusters</p>"#,
        clusters.len(),
    );

    Html(layout("Clusters", &body))
}

pub async fn detail(
    State(server): State<AlexandriaServer>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let members = match cluster_repo.get_members(&id).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(layout(
                    "Error",
                    &format!(r#"<p class="error">{}</p>"#, esc(&e.to_string())),
                )),
            )
        }
    };

    if members.is_empty() {
        // Distinguish "cluster with no members" from "cluster doesn't exist" is not possible
        // via get_members alone (it returns an empty Vec either way); render what we have.
    }

    let cohesion_html = if members.len() >= 4 {
        // Recompute the centroid inline for display purposes (approximate: average of member embeddings).
        let dims = members[0].embedding.len();
        let mut centroid = vec![0.0f32; dims];
        for m in &members {
            for (i, v) in m.embedding.iter().enumerate() {
                if i < dims {
                    centroid[i] += v;
                }
            }
        }
        let n = members.len() as f32;
        for v in centroid.iter_mut() {
            *v /= n;
        }
        let embeddings: Vec<Vec<f32>> = members.iter().map(|f| f.embedding.clone()).collect();
        match alexandria_engine::clusters::maintenance::check_cohesion(
            &id,
            &centroid,
            &embeddings,
            DISPLAY_COHESION_FLOOR,
        ) {
            alexandria_engine::clusters::maintenance::MaintenanceAction::Healthy => {
                "Healthy".to_string()
            }
            alexandria_engine::clusters::maintenance::MaintenanceAction::Split { .. } => {
                "Needs split (below cohesion floor)".to_string()
            }
        }
    } else {
        "N/A (fewer than 4 members)".to_string()
    };

    let mut rows_html = String::new();
    for fact in &members {
        let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
        let content_preview: String = fact.content.chars().take(120).collect();
        rows_html.push_str(&format!(
            r#"<tr><td><a class="link" href="/debug/memories/{fid}">{fid}</a></td><td>{}</td></tr>"#,
            esc(&content_preview),
        ));
    }

    let body = format!(
        r#"<h1>Cluster {}</h1>
<p>Cohesion: {}</p>
<table>
<tr><th>ID</th><th>Content</th></tr>
{rows_html}
</table>
<p>{} members</p>"#,
        esc(&id),
        cohesion_html,
        members.len(),
    );

    (StatusCode::OK, Html(layout("Cluster Detail", &body)))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_clusters_list_shows_labels_and_counts() {
        let server = super::super::test_support::test_server().await;
        let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());

        let c1 = cluster_repo
            .create(Some("cluster one"), &[0.1, 0.1])
            .await
            .unwrap();
        let f1 = memory_repo
            .create_fact("f1", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&c1, &f1).await.unwrap();
        cluster_repo
            .create(Some("cluster two"), &[0.9, 0.9])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/clusters")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("cluster one"));
        assert!(text.contains("cluster two"));
    }

    #[tokio::test]
    async fn test_cluster_detail_shows_member_content() {
        let server = super::super::test_support::test_server().await;
        let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());

        let c1 = cluster_repo
            .create(Some("cluster with members"), &[0.1, 0.1])
            .await
            .unwrap();
        let f1 = memory_repo
            .create_fact("member fact content", 0.5, &[0.1, 0.1], &[])
            .await
            .unwrap();
        cluster_repo.add_member(&c1, &f1).await.unwrap();

        let app = crate::debug::router(server);
        let uri = format!("/debug/clusters/{}", c1.replace(':', "%3A"));
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("member fact content"));
    }
}
