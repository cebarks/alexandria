use alexandria_storage::access::{AccessRepo, OwnerContext};
use alexandria_storage::models::{OperationId, Project};
use alexandria_storage::repos::{IdentityRepo, NewReminder, ScopedReminderRepo};
use alexandria_storage::{Database, schema};
use chrono::{Duration, Utc};
use serde_json::Value;

async fn fixture() -> (Database, OwnerContext, Project, Project) {
    let mut db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let alice = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            "reminder-alice",
            "fixture-digest",
            &OperationId::new("reminder-alice"),
        )
        .await
        .unwrap();
    let bobby = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            "reminder-bobby",
            "fixture-digest",
            &OperationId::new("reminder-bobby"),
        )
        .await
        .unwrap();
    let mut projects = Vec::new();
    for owner in [&alice, &bobby] {
        let mut response = db.inner()
            .query("CREATE project SET workspace = $workspace, normalized_alias = 'same-alias' RETURN AFTER;")
            .bind(("workspace", owner.workspace.as_record_id().clone()))
            .await.unwrap().check().unwrap();
        let created: Vec<Project> = response.take(0).unwrap();
        projects.push(created.into_iter().next().unwrap());
    }
    let system = db.claim_system_authority().unwrap();
    let principal = AccessRepo::new(&db)
        .local_principal(&system, &alice.account)
        .await
        .unwrap();
    let owner = AccessRepo::new(&db).authorize(&principal).await.unwrap();
    (db, owner, projects.remove(0), projects.remove(0))
}

fn once(message: &str, target: Option<String>, hours_overdue: i64) -> NewReminder {
    let due = Utc::now() - Duration::hours(hours_overdue);
    NewReminder {
        message: message.into(),
        target_project: target,
        prov_project: None,
        prov_session_id: None,
        note: None,
        schedule_kind: "once".into(),
        due_at: Some(due),
        freq: None,
        time_of_day: None,
        weekdays: vec![],
        day_of_month: None,
        cron_expr: None,
        next_due_at: Some(due),
    }
}

#[tokio::test]
async fn new_scoped_targets_require_an_owned_project_id_and_store_a_typed_link() {
    let (db, owner, own, foreign) = fixture().await;
    let repo = ScopedReminderRepo::new(db.inner(), owner);
    for target in [
        alexandria_storage::record_id_to_string(foreign.id.as_ref().unwrap()),
        "same-alias".to_string(),
        "project:never-existed".to_string(),
        "workspace:not-a-project".to_string(),
    ] {
        assert!(
            repo.create(
                &once("must not be scheduled", Some(target.clone()), 1),
                Utc::now()
            )
            .await
            .is_err(),
            "new authenticated target {target:?} must refuse, never store text or widen to global"
        );
    }
    assert!(repo.list(None, None, 100, 0).await.unwrap().0.is_empty());
    let created = repo
        .create(
            &once(
                "owned project positive control",
                Some(alexandria_storage::record_id_to_string(
                    own.id.as_ref().unwrap(),
                )),
                1,
            ),
            Utc::now(),
        )
        .await
        .unwrap();
    let mut response = db.inner()
        .query("SELECT project = $project AS typed_match, target_project = NONE AS no_legacy_target FROM $id;")
        .bind(("id", created.id.unwrap()))
        .bind(("project", own.id.unwrap()))
        .await.unwrap().check().unwrap();
    let rows: Vec<Value> = response.take(0).unwrap();
    assert_eq!(
        rows[0]["typed_match"], true,
        "new routing is v009 reminder.project"
    );
    assert_eq!(
        rows[0]["no_legacy_target"], true,
        "legacy text is migration input, not new routing"
    );
}

#[tokio::test]
async fn unresolved_legacy_target_cannot_displace_or_deliver_before_a_global_reminder() {
    let (db, owner, _, _) = fixture().await;
    let repo = ScopedReminderRepo::new(db.inner(), owner);
    let unresolved = repo
        .create(&once("legacy unresolved", None, 72), Utc::now())
        .await
        .unwrap();
    let global = repo
        .create(&once("global positive control", None, 1), Utc::now())
        .await
        .unwrap();
    db.inner()
        .query("UPDATE $id SET target_project = 'old-project-text', legacy_target_project = 'old-project-text';")
        .bind(("id", unresolved.id.clone().unwrap()))
        .await.unwrap().check().unwrap();
    let sample = repo.due_sample(Utc::now(), 1).await.unwrap();
    assert_eq!(sample.len(), 1);
    assert_eq!(
        sample[0].id, global.id,
        "exclude unresolved routing in SQL before LIMIT"
    );
    let due = repo.list_due(Utc::now()).await.unwrap();
    assert_eq!(
        due.len(),
        1,
        "unresolved legacy targets remain pending even after escalation window"
    );
    assert_eq!(due[0].id, global.id);
    let unresolved_id = alexandria_storage::record_id_to_string(unresolved.id.as_ref().unwrap());
    assert!(
        !repo
            .record_delivery(&unresolved_id, None, unresolved.next_due_at, Utc::now())
            .await
            .unwrap(),
        "the actual claim must refuse unresolved routing too"
    );
    assert_eq!(
        repo.get(&unresolved_id).await.unwrap().unwrap().status,
        "pending"
    );
}
