use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;

use super::html::{error_page, page, unavailable};
use super::web::{Boundary, BrowserSession};
use crate::AlexandriaServer;
use crate::server::record_id_to_string;

// No timestamps here, deliberately: neither cluster page prints a time, so neither participates
// in the shared `html::format_dt` / `html::ABSENT` contract that the memories, sessions and
// maintenance pages are pinned to (`test_sessions_pages_use_the_shared_timestamp_format_and_absent_marker`
// and its siblings). The absent-value marker this page does use is `(unlabeled)` for a cluster
// with no label — that is a *label* placeholder, not a timestamp one, and is left alone.

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

pub(crate) async fn list(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
) -> Response {
    let Ok((owner, read)) = boundary.corpus(&session).await else {
        return forbidden("this page requires an authenticated owner session");
    };
    let cluster_repo =
        alexandria_storage::repos::ScopedClusterRepo::new(server.db.inner(), &owner, &read);
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

/// The three verdicts a cluster can carry, plus the case where neither is reachable.
///
/// Shared by the cluster detail page and the dashboard rollup so the two cannot disagree, and
/// worded so a reader can act on it without knowing what `check_cohesion` returns.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Cohesion {
    Healthy,
    NeedsSplit,
    /// Too few members for the maintenance task to consider a split at all.
    TooSmall,
}

impl Cohesion {
    /// Display form. Kept here rather than in the template so the dashboard and the detail
    /// page render the same words for the same state.
    pub(super) fn label(self) -> &'static str {
        match self {
            Cohesion::Healthy => "Healthy",
            Cohesion::NeedsSplit => "Needs split (below cohesion floor)",
            Cohesion::TooSmall => "N/A (fewer than 4 members)",
        }
    }
}

/// Cohesion of one cluster, judged from its **stored** centroid.
///
/// The stored centroid is the value `alexandria`'s background maintenance task calls
/// `check_cohesion` with, so it is the only one a diagnostic surface may report on. This used
/// to average the member embeddings inline, which is a different vector whenever member
/// magnitudes differ — and a UI that says "Healthy" about a cluster the splitter is about to
/// divide is worse than a UI that says nothing.
///
/// `cluster_id` is carried through to the engine's action only; the verdict discards it.
pub(super) fn cohesion_of(
    cluster_id: &str,
    stored_centroid: &[f32],
    member_embeddings: &[Vec<f32>],
    cohesion_floor: f32,
) -> Cohesion {
    if member_embeddings.len() < 4 {
        return Cohesion::TooSmall;
    }
    match alexandria_engine::clusters::maintenance::check_cohesion(
        cluster_id,
        stored_centroid,
        member_embeddings,
        cohesion_floor,
    ) {
        alexandria_engine::clusters::maintenance::MaintenanceAction::Healthy => Cohesion::Healthy,
        alexandria_engine::clusters::maintenance::MaintenanceAction::Split { .. } => {
            Cohesion::NeedsSplit
        }
    }
}

/// Member embeddings, in the shape [`cohesion_of`] wants.
pub(super) fn embeddings_of(members: &[alexandria_storage::models::Fact]) -> Vec<Vec<f32>> {
    members.iter().map(|f| f.embedding.clone()).collect()
}

pub(crate) async fn detail(
    State(server): State<AlexandriaServer>,
    boundary: Boundary,
    session: BrowserSession,
    Path(id): Path<String>,
) -> Response {
    let Ok((owner, read)) = boundary.corpus(&session).await else {
        return forbidden("this page requires an authenticated owner session");
    };
    let cluster_repo =
        alexandria_storage::repos::ScopedClusterRepo::new(server.db.inner(), &owner, &read);
    let cluster = match cluster_repo.get(&id).await {
        Ok(cluster) => cluster,
        Err(e) => return unavailable("clusters", "cluster", e),
    };
    // A cluster the caller's population does not contain is a 404, and that includes one that is
    // genuinely absent: the scoped read cannot tell "deleted" from "someone else's", so neither can the
    // page — which is the point, because a distinguishable answer would be a probe for foreign ids.
    //
    // This replaces the pre-Task 9 rendering, which showed "N/A (no cluster record)" plus whatever
    // member edges still existed. Those edges carry **fact content**, and `get_members` on a foreign
    // cluster would have quoted another owner's memories on a page their own account never touched.
    let Some(cluster) = cluster else {
        let mut response = error_page("clusters", "Cluster not found.");
        *response.status_mut() = axum::http::StatusCode::NOT_FOUND;
        return response;
    };
    let members = match cluster_repo.get_members(&id).await {
        Ok(m) => m,
        Err(e) => return unavailable("clusters", "cluster members", e),
    };

    let cohesion = cohesion_of(
        &id,
        &cluster.centroid,
        &embeddings_of(&members),
        server.cohesion_floor,
    )
    .label()
    .to_string();

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

/// A refusal through the shared layout, for a handler that passed the guard and could not resolve its
/// caller's population.
fn forbidden(message: &str) -> Response {
    let mut response = error_page("clusters", message);
    *response.status_mut() = axum::http::StatusCode::FORBIDDEN;
    response
}

#[cfg(test)]
mod tests {
    use super::super::test_support::HOST_HEADER;
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::ClusterDetailTemplate;
    use crate::AlexandriaServer;

    /// Renders the cluster detail page for `cid` through the real router.
    async fn detail_html(server: &AlexandriaServer, cid: &str) -> String {
        let app = {
            let fixture = super::super::test_support::authenticated_on_server(server.clone()).await;
            fixture.session_router()
        };
        let uri = format!("/debug/clusters/{}", cid.replace(':', "%3A"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(HOST_HEADER, super::super::test_support::HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

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

        let app = {
            let fixture = super::super::test_support::authenticated_on_server(server).await;
            fixture.session_router()
        };
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/clusters")
                    .header(HOST_HEADER, super::super::test_support::HOST)
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

    /// Pins the cohesion fix: the verdict must come from the cluster's **stored** centroid,
    /// not from an average of its members. `disagreeing_cluster` is built so the two reach
    /// opposite answers, and the assertions below re-check that opposition rather than
    /// trusting it — otherwise a later edit to the fixture could quietly make this test vacuous.
    #[tokio::test]
    async fn test_cluster_detail_cohesion_uses_the_stored_centroid() {
        use alexandria_storage::repos::ClusterRepo;

        let server = super::super::test_support::test_server().await;
        let cid = super::super::test_support::disagreeing_cluster(&server).await;

        let stored = ClusterRepo::new(server.db.inner())
            .get(&cid)
            .await
            .unwrap()
            .expect("fixture cluster must exist")
            .centroid;
        let members = super::embeddings_of(
            &ClusterRepo::new(server.db.inner())
                .get_members(&cid)
                .await
                .unwrap(),
        );
        let average = super::super::test_support::average_centroid(&members);

        // The fixture guard, stated as the engine's own verdicts.
        assert_eq!(
            super::cohesion_of(&cid, &stored, &members, server.cohesion_floor),
            super::Cohesion::Healthy,
            "fixture's stored centroid must read as healthy"
        );
        assert_eq!(
            super::cohesion_of(&cid, &average, &members, server.cohesion_floor),
            super::Cohesion::NeedsSplit,
            "fixture's member average must read as needing a split, or the test proves nothing"
        );

        let html = detail_html(&server, &cid).await;
        assert!(
            html.contains("Healthy"),
            "the page must report the stored centroid's verdict"
        );
        assert!(
            !html.contains("Needs split"),
            "the page rendered the member-average verdict: {html}"
        );
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

        let app = {
            let fixture = super::super::test_support::authenticated_on_server(server).await;
            fixture.session_router()
        };
        let uri = format!("/debug/clusters/{}", c1.replace(':', "%3A"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(HOST_HEADER, super::super::test_support::HOST)
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

        let app = {
            let fixture = super::super::test_support::authenticated_on_server(server).await;
            fixture.session_router()
        };
        let uri = format!("/debug/clusters/{}", c1.replace(':', "%3A"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(HOST_HEADER, super::super::test_support::HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
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

    /// Storage failure ⇒ `html::UNAVAILABLE_STATUS`, asserted on bytes the handler produced.
    ///
    /// Forced by removing the table the read needs, on a store that *is* migrated: the first repo call
    /// then fails for real, with no mock and no change to `alexandria-storage`. The body assertion matters
    /// as much as the status one — it is what proves this page goes through `html::unavailable`
    /// rather than re-inlining `error_page` plus a `status_mut` (the old `memories.rs` shape), which
    /// would keep the status identical and only change the wording.
    #[tokio::test]
    async fn test_cluster_storage_failure_answers_with_the_one_status() {
        // A migrated store with the one table the read needs removed: the handler's repo call then
        // fails for real. (The pre-Task 9 injection was an unmigrated database, which the boundary now
        // refuses before any handler runs — a page cannot be shown to fail on a store that cannot
        // sign anybody in.)
        let fixture = super::super::test_support::authenticated_app(true).await;
        fixture.drop_table("cluster").await;
        let app = fixture.session_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/clusters/cluster:absent")
                    .header(HOST_HEADER, super::super::test_support::HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The literal, not `html::UNAVAILABLE_STATUS`: asserting against the constant would pass no
        // matter what status it names, which is the opposite of pinning one.
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "a storage failure must answer with the one detail-page status"
        );
        assert_eq!(
            response.status(),
            super::super::html::UNAVAILABLE_STATUS,
            "…and the shared constant must still name it"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("storage error while loading cluster"),
            "the page must name what failed, via the shared helper; got: {text}"
        );
    }

    #[test]
    fn test_cluster_detail_template_empty_state_keeps_the_header_and_marks_the_gap() {
        let html = ClusterDetailTemplate {
            nav: "clusters",
            id: "cluster:abc".into(),
            cohesion: "too few members to judge".into(),
            members: vec![],
            member_count: 0,
        }
        .render()
        .unwrap();
        assert!(
            html.contains("<tr><th>ID</th><th>Content</th></tr>"),
            "the header row must survive a memberless cluster; got: {html}"
        );
        assert!(
            html.contains("<td colspan=\"2\" class=\"empty\">No members in this cluster.</td>"),
            "the shared empty-state row must stand in for the missing rows; got: {html}"
        );
    }
}

/// Clusters as two owners see them, including the member content the detail page quotes.
#[cfg(test)]
mod boundary {
    use crate::debug::test_support as support;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn each_owner_sees_their_own_clusters_and_nobody_elses_members() {
        let server = crate::debug::test_support::test_server().await;
        let alice = support::authenticated_on_server(server.clone()).await;
        let bob = alice.second_owner("debug-fixture-second").await;

        let alice_cluster = alice.seed_cluster(Some("alice cluster"), &[0.1, 0.2]).await;
        let alice_fact = alice.seed_personal("alice member text").await;
        alice.add_cluster_member(&alice_cluster, &alice_fact).await;
        let bob_cluster = bob.seed_cluster(Some("bob cluster"), &[0.3, 0.4]).await;
        let bob_fact = bob.seed_personal("bob member text").await;
        bob.add_cluster_member(&bob_cluster, &bob_fact).await;

        let (status, page) = alice.get("/debug/clusters").await;
        assert_eq!(status, StatusCode::OK, "{}", support::clip(&page));
        assert!(
            page.contains("alice cluster"),
            "the owner's cluster is missing"
        );
        assert!(
            !page.contains("bob cluster"),
            "a second owner's cluster was listed: {}",
            support::clip(&page)
        );

        let (own, own_body) = alice
            .get(&format!(
                "/debug/clusters/{}",
                alice_cluster.replace(':', "%3A")
            ))
            .await;
        assert_eq!(own, StatusCode::OK, "{}", support::clip(&own_body));
        assert!(
            own_body.contains("alice member text"),
            "the owner lost their own member"
        );
        assert!(
            !own_body.contains("bob member text"),
            "a foreign member's content was quoted: {}",
            support::clip(&own_body)
        );

        let (foreign, _) = alice
            .get(&format!(
                "/debug/clusters/{}",
                bob_cluster.replace(':', "%3A")
            ))
            .await;
        let (absent, _) = alice.get("/debug/clusters/cluster%3Ano-such").await;
        assert_eq!(foreign, StatusCode::NOT_FOUND);
        assert_eq!(
            absent, foreign,
            "a foreign cluster is distinguishable from an absent one"
        );
    }
}
