//! Opt-in browser host over synthetic in-memory data and deterministic embedding stubs.
//! Never reads ambient server configuration, model artifacts, or the real corpus.

use std::sync::Arc;

use alexandria_storage::models::{FactId, MemoryScope, Version};
use alexandria_storage::repos::ClassificationRepo;
use axum::routing::get;
use serde_json::{Value, json};
use surrealdb::types::RecordId;

use super::test_support::{AuthenticatedTestApp, PASSWORD, test_server};
use super::{WebBoundary, WebPolicy};
use crate::auth::admission::Admission;
use crate::auth::http::AuthMiddleware;
use crate::auth::mcp_transport::McpSessionRegistry;

#[tokio::test]
#[ignore = "explicit disposable browser journey; serves until its owned process is stopped"]
async fn owner_browser_fixture() {
    let server = test_server().await;
    let alice = AuthenticatedTestApp::on_server(server.clone(), "fixture-alice").await;
    let bob = alice.second_owner("fixture-bob").await;

    // -- what the journey needs to be able to *do*, not only to look at -------------------------------
    //
    // The single-item journey was already covered by the request-level suites; what the parent's native
    // browser pass has to prove is the shapes those suites could not reach — two boxes checked at once, a
    // real project destination, a tagged row edited without losing its tags, and a control whose expected
    // revision has gone stale. Each state below exists because one of those journeys needs it.

    // A tagged memory, so a text-only save can be shown to preserve the tag set (H2) and the explicit
    // clear control shown to remove it.
    let tagged = alice.seed_personal("Fixture tagged lesson <before>").await;
    alice
        .set_tags(&tagged, &["fixture-tag".to_string(), "keep-me".to_string()])
        .await;
    let editable = alice
        .seed_personal("Fixture original owner lesson <before>")
        .await;
    let removable = alice
        .seed_personal("Fixture memory selected for soft deletion")
        .await;
    // The second row the multi-select journey needs: two proposals, checked together, approved in one
    // post that a real browser sends as two repeated `selected` pairs.
    let multi_second = alice
        .seed_personal("Fixture second classification candidate")
        .await;
    let unassigned = alice
        .seed_quarantined_unassigned("Fixture quarantined classification candidate")
        .await;
    let foreign = bob.seed_personal("Fixture Bob private control").await;

    // Two projects of Alice's own, so a project move and a destination change are both selectable by a
    // human rather than typed as a guess. `own_project` establishes through the scoped repo, so the rows
    // carry Alice's account and workspace and the resolver can find them by handle.
    let project_a = alice.own_project("fixture-project-a").await;
    let project_b = alice.own_project("fixture-project-b").await;
    // A row already inside a project, so a project→personal and project→project move are both reachable
    // from the listing without first performing one.
    let in_project = alice
        .seed_personal("Fixture project memory already filed")
        .await;
    let in_project_revision = alice.semantic_revision(&in_project).await;
    let moved = alice
        .move_row(
            &in_project,
            MemoryScope::Project(
                alexandria_storage::models::ProjectId::try_from(
                    RecordId::parse_simple(&project_a).expect("a fixture project handle"),
                )
                .expect("a project id"),
            ),
            &format!("fixture-move-{in_project_revision}"),
        )
        .await;
    assert!(moved, "the fixture could not file a row into a project");
    // A lawful surviving member makes the native A→B journey exercise both nonempty clusters.
    let source_a = alice
        .seed_cluster(Some("Fixture project A cluster"), &[0.1, 0.2])
        .await;
    alice.add_cluster_member(&source_a, &in_project).await;

    let unassigned_id = FactId::try_from(RecordId::parse_simple(&unassigned).unwrap()).unwrap();
    let classify = ClassificationRepo::new(alice.db.inner(), &alice.owner);
    let proposal = classify
        .propose(
            &unassigned_id,
            Version::new(0),
            MemoryScope::Personal,
            Some("Synthetic owner-approved browser fixture"),
        )
        .await
        .unwrap();
    let second_id = FactId::try_from(RecordId::parse_simple(&multi_second).unwrap()).unwrap();
    let second_proposal = classify
        .propose(
            &second_id,
            Version::new(alice.semantic_revision(&multi_second).await),
            MemoryScope::Project(
                alexandria_storage::models::ProjectId::try_from(
                    RecordId::parse_simple(&project_b).expect("a fixture project handle"),
                )
                .expect("a project id"),
            ),
            Some("Synthetic second candidate for the multi-select journey"),
        )
        .await
        .unwrap();

    // The stale-control row: a memory whose rendered revision is about to be moved forward behind the
    // page's back. The fixture prints the *rendered* revision it had before the bump, so the parent's
    // journey can post exactly what a stale page would post and observe the refusal.
    let stale = alice
        .seed_personal("Fixture memory with a stale control")
        .await;
    let stale_rendered = alice.semantic_revision(&stale).await;
    alice.bump_semantic_revision(&stale).await;
    let stale_current = alice.semantic_revision(&stale).await;

    // A session with memories in two partitions, so the archive's mixed-scope label is a fact the journey
    // can observe rather than a claim in a comment: one row is Personal, one was filed into a project, and
    // the session link stayed where it was written — which is exactly why its prose is archive-only.
    alice
        .seed_session(
            "fixture-mixed-session",
            Some("Fixture whole-session summary <b>markup</b>"),
        )
        .await;
    alice.link_session("fixture-mixed-session", &editable).await;
    alice
        .link_session("fixture-mixed-session", &in_project)
        .await;
    let mixed = alice
        .session_scopes("fixture-mixed-session")
        .await
        .join(",");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let policy = WebPolicy::hosted(
        &origin,
        &["127.0.0.1".to_string()],
        std::slice::from_ref(&origin),
        true,
    )
    .unwrap();
    let gate = AuthMiddleware::admitting(
        Arc::clone(&alice.service),
        Admission::bound_to(alice.service.events()),
        McpSessionRegistry::over(&alice.service.events()),
    );
    let boundary = WebBoundary::new(gate, Arc::clone(&alice.db), policy);
    // Build the real router, NOT the fixture's request-header/adoption middleware.
    let router = super::router_secured(server, None, boundary);
    let db = Arc::clone(&alice.db);
    // A read-only observation endpoint on this ignored fixture only. Every row is synthetic.
    let router = router.route(
        "/__fixture_state",
        get(move || {
            let db = Arc::clone(&db);
            async move {
                let mut response = db
                    .inner()
                    .query(
                        "SELECT id, content, tags, deleted, quarantined_at, scope, project, \
                         semantic_revision FROM fact; SELECT id, kind, operation_key, \
                         affected_ids FROM semantic_operation; SELECT id, state, revision, \
                         destination_kind, destination_project FROM classification_proposal; \
                         SELECT id, normalized_alias, display_name FROM project; \
                         SELECT id, workspace, centroid, membership_revision, centroid_revision, \
                         member_embedding_revision FROM cluster ORDER BY id; \
                         SELECT in, out FROM contains_memory ORDER BY in, out;",
                    )
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                let facts: Vec<Value> = response.take(0).unwrap();
                let operations: Vec<Value> = response.take(1).unwrap();
                let proposals: Vec<Value> = response.take(2).unwrap();
                let projects: Vec<Value> = response.take(3).unwrap();
                let clusters: Vec<Value> = response.take(4).unwrap();
                let memberships: Vec<Value> = response.take(5).unwrap();
                axum::Json(json!({
                    "facts": facts,
                    "operations": operations,
                    "proposals": proposals,
                    "projects": projects,
                    "clusters": clusters,
                    "memberships": memberships,
                }))
            }
        }),
    );
    println!(
        "BROWSER_FIXTURE {}",
        json!({
            "origin": origin,
            "login": "fixture-alice",
            "other_login": "fixture-bob",
            "password": PASSWORD,
            "journeys": {
                "edit_text_preserves_tags": {
                    "id": tagged,
                    "expect_tags_after_text_only_save": ["fixture-tag", "keep-me"],
                    "clear_control": "check Remove every tag, then expect tags = []"
                },
                "multi_select_decide": {
                    "proposal_one": format!("{}:{}", proposal.id, proposal.revision.get()),
                    "proposal_two": format!("{}:{}", second_proposal.id, second_proposal.revision.get()),
                    "expect": "both boxes checked post two `selected` pairs; approve writes two move rows, dismiss writes none"
                },
                "project_move": {
                    "id": editable,
                    "revision_note": "read the revision off the rendered row; the listing shows a Rev column",
                    "project_a": project_a,
                    "project_b": project_b,
                    "already_in_project": in_project,
                    "source_project_a_cluster": source_a
                },
                "stale_conflict": {
                    "id": stale,
                    "rendered_revision": stale_rendered,
                    "current_revision": stale_current,
                    "expect": "posting the rendered revision refuses with the reload message and journals nothing"
                },
                "owner_soft_delete": {
                    "id": removable,
                    "expect": "Hide this memory journals one delete and the history page shows the exact before-state"
                },
                "human_review_delete": {
                    "id": unassigned,
                    "expect": "the owner page can hide a quarantined review row; delete_memory cannot reach it"
                },
                "whole_session_archive": {
                    "path": "/debug/collections/archive",
                    "session": "fixture-mixed-session",
                    "scopes": mixed,
                    "expect": "session prose appears only there, and this session is labelled mixed"
                },
                "foreign_visibility": {
                    "bob_row": foreign,
                    "expect": "Alice's pages never show Bob's content"
                }
            },
            "synthetic_only": true,
            "embedding": "constant deterministic stub; no artifacts or network"
        })
    );
    axum::serve(listener, router).await.unwrap();
}
