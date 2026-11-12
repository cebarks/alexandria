use super::*;
use surrealdb::types::RecordId;

type Row = serde_json::Value;

async fn row(app: &AuthenticatedTestApp, id: &RecordId) -> Row {
    let mut response = app
        .db
        .inner()
        .query("SELECT * FROM $record;")
        .bind(("record", id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let mut rows: Vec<Row> = response.take(0).unwrap();
    assert_eq!(rows.len(), 1, "the ownership fixture row really exists");
    rows.remove(0)
}

fn assert_adopted(row: &Row, account: &RecordId, workspace: &RecordId) {
    assert_eq!(
        row["account"].as_str(),
        Some(record_id_to_string(account).as_str())
    );
    assert_eq!(
        row["workspace"].as_str(),
        Some(record_id_to_string(workspace).as_str())
    );
}

#[tokio::test]
async fn fixture_adoption_preserves_foreign_and_partial_ownership_markers() {
    let app = authenticated_on_server(test_server().await).await;
    let account = app.owner.account().as_record_id().clone();
    let workspace = app.owner.workspace().as_record_id().clone();
    let foreign = IdentityRepo::new(app.db.inner())
        .bootstrap_owner(
            "adoption-foreign-owner",
            "$argon2id$fixture-prepared-hash-classification-padding",
            &OperationId::new("adoption-foreign-bootstrap".to_string()),
        )
        .await
        .unwrap();
    let foreign_account = foreign.account.as_record_id().clone();
    let mut response = app
        .db
        .inner()
        .query("SELECT VALUE id FROM workspace WHERE owner = $account;")
        .bind(("account", foreign_account.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let foreign_workspaces: Vec<RecordId> = response.take(0).unwrap();
    assert_eq!(foreign_workspaces.len(), 1);
    let foreign_workspace = foreign_workspaces[0].clone();
    let project = ScopedProjectRepo::new(app.db.inner(), &app.owner)
        .establish("adoption-project", Some("Adoption project"))
        .await
        .unwrap()
        .project
        .id;
    let cases = vec![
        ("unowned", None, None, None, None, true),
        (
            "own-scope-less",
            Some(account.clone()),
            Some(workspace.clone()),
            None,
            None,
            true,
        ),
        (
            "foreign-assigned",
            Some(foreign_account.clone()),
            Some(foreign_workspace.clone()),
            Some("personal"),
            None,
            false,
        ),
        (
            "foreign-scope-less",
            Some(foreign_account.clone()),
            Some(foreign_workspace.clone()),
            None,
            None,
            false,
        ),
        (
            "account-only",
            Some(foreign_account.clone()),
            None,
            None,
            None,
            false,
        ),
        (
            "workspace-only",
            None,
            Some(foreign_workspace.clone()),
            None,
            None,
            false,
        ),
        ("scope-only", None, None, Some("personal"), None, false),
        (
            "project-only",
            None,
            None,
            None,
            Some(project.as_record_id().clone()),
            false,
        ),
        (
            "mismatched-workspace",
            Some(account.clone()),
            Some(foreign_workspace.clone()),
            None,
            None,
            false,
        ),
    ];
    let mut protected = Vec::new();
    let mut protected_fact_contents = Vec::new();
    let mut adopted_facts = Vec::new();
    for (key, a, w, scope, project, adopt) in cases {
        let id = RecordId::new("fact", key);
        app.db.inner().query("CREATE $fact SET content = $content, confidence = 0.5, embedding = [1.0, 0.0], tags = [], account = $a, workspace = $w, scope = $scope, project = $project;")
            .bind(("fact", id.clone())).bind(("content", format!("adoption control {key}")))
            .bind(("a", a)).bind(("w", w)).bind(("scope", scope.map(str::to_string)))
            .bind(("project", project)).await.unwrap().check().unwrap();
        if adopt {
            adopted_facts.push(id);
        } else {
            protected_fact_contents.push(format!("adoption control {key}"));
            protected.push((id.clone(), row(&app, &id).await));
        }
    }
    let mut adopted_others = Vec::new();
    for (table, fields) in [
        (
            "session",
            "external_id = $external, summary = 'adoption control', started_at = time::now(), ended_at = time::now()",
        ),
        (
            "cluster",
            "label = 'adoption control', centroid = [1.0, 0.0], depth = 0",
        ),
        ("raw", "content = 'adoption control'"),
    ] {
        for partial in [false, true] {
            let id = RecordId::new(
                table,
                if partial {
                    "partial-workspace"
                } else {
                    "unowned-control"
                },
            );
            app.db
                .inner()
                .query(format!(
                    "CREATE $record SET {fields}, account = NONE, workspace = $workspace;"
                ))
                .bind(("record", id.clone()))
                .bind(("workspace", partial.then(|| foreign_workspace.clone())))
                .bind(("external", format!("adoption-{table}-{partial}")))
                .await
                .unwrap()
                .check()
                .unwrap();
            if partial {
                protected.push((id.clone(), row(&app, &id).await));
            } else {
                adopted_others.push(id);
            }
        }
    }
    // Use the real test router: the adoption middleware must not rewrite a foreign row to hide a leak.
    let response = app
        .session_router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/debug/memories")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = body_of(response).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the real compatibility router must admit its own session and injected Host: {body}"
    );
    for id in adopted_facts {
        let after = row(&app, &id).await;
        assert_adopted(&after, &account, &workspace);
        assert_eq!(after["scope"], "personal");
        assert!(
            body.contains(after["content"].as_str().unwrap()),
            "adoption must keep a useful own-page control"
        );
    }
    for id in adopted_others {
        assert_adopted(&row(&app, &id).await, &account, &workspace);
    }
    for (id, before) in protected {
        assert_eq!(
            row(&app, &id).await,
            before,
            "fixture adoption must preserve original ownership/scope for {id:?}"
        );
    }
    for content in protected_fact_contents {
        assert!(
            !body.contains(&content),
            "foreign or partially-owned fixture content escaped into the caller's page: {content}"
        );
    }
}
