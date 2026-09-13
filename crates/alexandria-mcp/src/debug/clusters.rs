use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::html::{error_page, page};
use crate::AlexandriaServer;
use crate::server::record_id_to_string;

/// One row of the cluster list, flattened out of `Cluster` so the template never has to
/// deal with `Option<RecordId>` or with how a record id is formatted.
struct ClusterRow {
    id: String,
    label: String,
    members: usize,
    depth: i64,
}

#[derive(Template)]
#[template(path = "clusters.html")]
struct ClustersTemplate {
    nav: &'static str,
    clusters: Vec<ClusterRow>,
    total: usize,
}

/// One member row of the detail view. `preview` is the 120-char content truncation the
/// legacy page showed.
struct MemberRow {
    id: String,
    preview: String,
}

#[derive(Template)]
#[template(path = "cluster_detail.html")]
struct ClusterDetailTemplate {
    nav: &'static str,
    id: String,
    /// Already-resolved health text: the cohesion math stays in the handler (and in the
    /// engine), the template only displays the verdict.
    cohesion: String,
    members: Vec<MemberRow>,
    member_count: usize,
}

pub async fn list(State(server): State<AlexandriaServer>) -> Response {
    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let clusters = match cluster_repo.list_with_counts().await {
        Ok(c) => c,
        Err(e) => return error_page("clusters", &e.to_string()),
    };

    let total = clusters.len();
    let rows = clusters
        .into_iter()
        .map(|(cluster, count)| ClusterRow {
            id: cluster
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default(),
            label: cluster
                .label
                .as_deref()
                .unwrap_or("(unlabeled)")
                .to_string(),
            members: count,
            depth: cluster.depth,
        })
        .collect();

    page(ClustersTemplate {
        nav: "clusters",
        clusters: rows,
        total,
    })
}

pub async fn detail(State(server): State<AlexandriaServer>, Path(id): Path<String>) -> Response {
    let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
    let members = match cluster_repo.get_members(&id).await {
        Ok(m) => m,
        Err(e) => {
            // The detail handler has always answered a data-layer failure with 500 (the list
            // handlers return 200), so keep the status rather than taking `error_page`'s.
            let mut response = error_page("clusters", &e.to_string());
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return response;
        }
    };

    if members.is_empty() {
        // Distinguish "cluster with no members" from "cluster doesn't exist" is not possible
        // via get_members alone (it returns an empty Vec either way); render what we have.
    }

    let cohesion = if members.len() >= 4 {
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
            server.cohesion_floor,
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

    let member_count = members.len();
    let rows = members
        .iter()
        .map(|fact| MemberRow {
            id: fact
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default(),
            preview: fact.content.chars().take(120).collect(),
        })
        .collect();

    page(ClusterDetailTemplate {
        nav: "clusters",
        id,
        cohesion,
        members: rows,
        member_count,
    })
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

    /// Renders the cluster detail page for a 4-member cluster whose members all sit at
    /// cosine 0.56 from their own averaged centroid — above a 0.4 floor, below the 0.6
    /// default. `None` leaves the server on the value `new()` derives from the engine.
    /// Before the floor was threaded, both calls rendered the same verdict.
    async fn cohesion_verdict(cohesion_floor: Option<f32>) -> String {
        let server = super::super::test_support::test_server().await;
        let server = match cohesion_floor {
            Some(floor) => server.with_cohesion_floor(floor),
            None => server,
        };
        let cluster_repo = alexandria_storage::repos::ClusterRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());

        // Two mirrored pairs about the x-axis: their mean is [0.56, 0.0], and the cosine
        // of each unit member against it is 0.56.
        let members = [
            [0.56_f32, 0.8285],
            [0.56, 0.8285],
            [0.56, -0.8285],
            [0.56, -0.8285],
        ];
        let c1 = cluster_repo
            .create(Some("diffuse cluster"), &[0.56, 0.0])
            .await
            .unwrap();
        for (i, embedding) in members.iter().enumerate() {
            let fact = memory_repo
                .create_fact(&format!("diffuse member {i}"), 0.5, embedding, &[])
                .await
                .unwrap();
            cluster_repo.add_member(&c1, &fact).await.unwrap();
        }

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
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn test_cluster_detail_cohesion_reads_the_servers_floor() {
        let at_default = cohesion_verdict(None).await;
        assert!(
            at_default.contains("Needs split"),
            "a cluster at cosine 0.56 must split under the default floor ({}), got: {at_default}",
            alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR,
        );

        let at_relaxed_floor = cohesion_verdict(Some(0.4)).await;
        assert!(
            at_relaxed_floor.contains("Healthy"),
            "a 0.4 cohesion_floor must report the same cluster as healthy, got: {at_relaxed_floor}"
        );
    }
}
