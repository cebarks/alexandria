use alexandria_storage::access::AccessRepo;
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{OperationId, ProjectId, Version};
use alexandria_storage::repos::{IdentityRepo, NewReminder, ReminderRepo, ScopedReminderRepo};
use alexandria_storage::{Database, parse_record_id, record_id_to_string, schema};
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use surrealdb::types::{RecordId, SurrealValue};

struct Fixture {
    db: Database,
    owner: alexandria_storage::access::OwnerContext,
    workspace: alexandria_storage::models::identity::WorkspaceId,
}

async fn fixture(login: &str) -> Fixture {
    let mut db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let system = db.claim_system_authority().unwrap();
    let ids = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            login,
            "$argon2id$fixture-prepared-hash-reminder-hold-padding",
            &OperationId::new(format!("reminder-hold-bootstrap-{login}")),
        )
        .await
        .unwrap();
    let principal = AccessRepo::new(&db)
        .local_principal(&system, &ids.account)
        .await
        .unwrap();
    let owner = AccessRepo::new(&db).authorize(&principal).await.unwrap();
    Fixture {
        db,
        owner,
        workspace: ids.workspace,
    }
}

async fn create_project(fx: &Fixture, alias: &str) -> ProjectId {
    #[derive(SurrealValue)]
    struct ProjectRow {
        id: RecordId,
    }
    let mut response = fx
        .db
        .inner()
        .query("CREATE project SET workspace = $workspace, normalized_alias = $alias RETURN AFTER;")
        .bind(("workspace", fx.workspace.as_record_id().clone()))
        .bind(("alias", alias.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<ProjectRow> = response.take(0).unwrap();
    ProjectId::try_from(rows.into_iter().next().unwrap().id).unwrap()
}

async fn held_reminder(fx: &Fixture, legacy_target: &str, next_due_at: DateTime<Utc>) -> String {
    let id = ReminderRepo::new(fx.db.inner())
        .create(&NewReminder {
            message: "follow up on the legacy project".into(),
            target_project: Some(legacy_target.to_string()),
            prov_project: None,
            prov_session_id: None,
            note: Some("preserve this schedule while mapping".into()),
            schedule_kind: "cron".into(),
            due_at: None,
            freq: None,
            time_of_day: None,
            weekdays: Vec::new(),
            day_of_month: None,
            cron_expr: Some("0 9 * * MON-FRI".into()),
            next_due_at: Some(next_due_at),
        })
        .await
        .unwrap();
    let last_delivered_at = next_due_at - chrono::Duration::days(1);
    fx.db
        .inner()
        .query(
            "UPDATE type::record($id) SET account = $account, migration_hold = true, \
             legacy_target_project = $legacy, semantic_revision = 0, delivered_count = 4, \
             last_delivered_at = $last_delivered_at RETURN AFTER;",
        )
        .bind(("id", parse_record_id(&id).unwrap()))
        .bind(("account", fx.owner.account().as_record_id().clone()))
        .bind(("legacy", legacy_target.to_string()))
        .bind(("last_delivered_at", last_delivered_at))
        .await
        .unwrap()
        .check()
        .unwrap();
    id
}

async fn reminder_row(fx: &Fixture, id: &str) -> Json {
    let mut response = fx
        .db
        .inner()
        .query("SELECT * FROM type::record($id);")
        .bind(("id", parse_record_id(id).unwrap()))
        .await
        .unwrap()
        .check()
        .unwrap();
    response
        .take::<Vec<Json>>(0)
        .unwrap()
        .into_iter()
        .next()
        .expect("seeded reminder exists")
}

#[tokio::test]
async fn release_preserves_schedule_delivery_history_and_legacy_text_and_journals_the_owner_action()
{
    let fx = fixture("hold-release-owner").await;
    let target = create_project(&fx, "mapped-target").await;
    let overdue = DateTime::parse_from_rfc3339("2026-06-01T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let legacy_text = "ssh://git.example/team/repo.git";
    let id = held_reminder(&fx, legacy_text, overdue).await;
    let target_handle =
        alexandria_storage::project_handle::format_project_handle(target.as_record_id());
    let before = reminder_row(&fx, &id).await;

    let outcome = ReminderRepo::new(fx.db.inner())
        .release_migration_hold(
            &fx.owner,
            &id,
            Version::ZERO,
            Some(target.clone()),
            &OperationId::new("owner-maps-reminder-1"),
        )
        .await;
    assert!(
        outcome.is_ok(),
        "the owner should release a current held reminder: {outcome:?}"
    );

    let after = reminder_row(&fx, &id).await;
    assert_eq!(
        after["migration_hold"], false,
        "release clears only the hold"
    );
    assert_ne!(after["project"], Json::Null);
    let typed = ReminderRepo::new(fx.db.inner())
        .get(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(typed.project.as_ref(), Some(target.as_record_id()));
    assert_eq!(
        alexandria_storage::project_handle::format_project_handle(typed.project.as_ref().unwrap()),
        target_handle
    );
    for field in [
        "target_project",
        "legacy_target_project",
        "schedule_kind",
        "cron_expr",
        "next_due_at",
        "delivered_count",
        "last_delivered_at",
        "status",
    ] {
        assert_eq!(after[field], before[field], "release changed {field}");
    }
    assert_eq!(after["legacy_target_project"], legacy_text);
    assert_eq!(
        after["semantic_revision"], 1,
        "release bumps the reminder revision"
    );

    let mut operations = fx
        .db
        .inner()
        .query("SELECT * FROM semantic_operation WHERE workspace = $workspace AND operation_key = $key;")
        .bind(("workspace", fx.workspace.as_record_id().clone()))
        .bind(("key", "owner-maps-reminder-1".to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let operations: Vec<Json> = operations.take(0).unwrap();
    assert_eq!(
        operations.len(),
        1,
        "the owner action is journaled atomically"
    );
    assert_eq!(operations[0]["kind"], "reminder_map");
    assert!(!operations[0]["preimages"].as_array().unwrap().is_empty());
    assert!(!operations[0]["postimages"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn stale_revision_cannot_release_a_migration_hold() {
    let fx = fixture("hold-stale-owner").await;
    let id = held_reminder(
        &fx,
        "legacy-name-that-is-not-a-project-handle",
        Utc::now() - chrono::Duration::days(4),
    )
    .await;
    let before = reminder_row(&fx, &id).await;

    let outcome = ReminderRepo::new(fx.db.inner())
        .release_migration_hold(
            &fx.owner,
            &id,
            Version::new(9),
            None,
            &OperationId::new("stale-reminder-map"),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::Stale)),
        "stale reminder revisions must refuse with Stale, got {outcome:?}"
    );
    assert_eq!(
        reminder_row(&fx, &id).await,
        before,
        "a stale map writes nothing"
    );
}

#[tokio::test]
async fn malformed_reminder_ids_are_refused_before_release() {
    let fx = fixture("hold-malformed-id-owner").await;
    let outcome = ReminderRepo::new(fx.db.inner())
        .release_migration_hold(
            &fx.owner,
            "reminder:valid-looking; trailing-garbage",
            Version::ZERO,
            None,
            &OperationId::new("malformed-reminder-map"),
        )
        .await;
    assert!(
        matches!(outcome, Err(StorageError::InvalidInput(_))),
        "the full reminder handle must parse strictly, got {outcome:?}"
    );
}

#[tokio::test]
async fn held_reminder_is_not_delivered_escalated_or_piggybacked_and_ambiguous_text_stays_unresolved()
 {
    let fx = fixture("hold-unresolved-owner").await;
    let legacy_text = "team/repo (two projects share this label)";
    let id = held_reminder(&fx, legacy_text, Utc::now() - chrono::Duration::days(8)).await;
    let now = Utc::now();
    let repo = ScopedReminderRepo::new(fx.db.inner(), fx.owner.clone());

    assert!(
        repo.list_due(now).await.unwrap().is_empty(),
        "held rows are not claim candidates for delivery or escalation"
    );
    assert!(
        repo.due_sample(now, 20).await.unwrap().is_empty(),
        "held rows never enter the prompt piggyback"
    );

    let (rows, _) = repo.list(None, None, 20, 0).await.unwrap();
    let row = rows
        .iter()
        .find(|row| {
            row.id
                .as_ref()
                .is_some_and(|rid| record_id_to_string(rid) == id)
        })
        .expect("owner history still lists the held reminder");
    assert_eq!(
        row.status, "pending",
        "ambiguous mapping remains pending until an owner acts"
    );
    assert_eq!(
        row.project, None,
        "ambiguous legacy text is not resolved by guessing"
    );
    assert_eq!(row.legacy_target_project.as_deref(), Some(legacy_text));
    assert_eq!(row.migration_hold, Some(true));
}
