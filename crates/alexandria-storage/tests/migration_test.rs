use alexandria_storage::{Database, schema};

#[tokio::test]
async fn test_fresh_db_runs_all_migrations() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // Verify system_config has schema_version
    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    let version = rows[0]["value"].as_str().unwrap();
    assert_eq!(version, schema::LATEST_VERSION.to_string());
}

#[tokio::test]
async fn test_migrate_idempotent() {
    let db = Database::connect_embedded().await.unwrap();

    // Run twice — should be a no-op the second time
    schema::migrate(db.inner()).await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(
        rows[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string()
    );
}

#[tokio::test]
async fn test_memory_edge_table_exists_after_migration() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // memory_edge table should exist — test by inserting a relation
    // First create two facts
    db.inner()
        .query("CREATE fact:a SET content = 'fact A', embedding = [1.0], tags = []")
        .await
        .unwrap()
        .check()
        .unwrap();
    db.inner()
        .query("CREATE fact:b SET content = 'fact B', embedding = [1.0], tags = []")
        .await
        .unwrap()
        .check()
        .unwrap();

    // Create an edge between them
    db.inner()
        .query("RELATE fact:a->memory_edge->fact:b SET edge_type = 'relates_to', strength = 0.8")
        .await
        .unwrap()
        .check()
        .unwrap();

    // Query the edge
    let mut result = db.inner().query("SELECT * FROM memory_edge").await.unwrap();
    let edges: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["edge_type"].as_str().unwrap(), "relates_to");
}

// CREATE alone doesn't prove the migration ran — SurrealDB implicitly creates
// undefined tables as SCHEMALESS; the field assertions below do.
#[tokio::test]
async fn test_reminder_table_exists_after_migration() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let result = db
        .inner()
        .query("CREATE reminder SET message = 'probe', schedule_kind = 'once', next_due_at = time::now()")
        .await
        .unwrap();
    result.check().unwrap();

    // SCHEMAFULL defaults from v007 must be present on the created row.
    let mut result = db.inner().query("SELECT * FROM reminder").await.unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"].as_str().unwrap(), "pending");
    assert_eq!(rows[0]["delivered_count"].as_i64().unwrap(), 0);
}

/// The failure mode this pins: `migrate()` applies a file statement-by-statement
/// (each top-level statement commits in its own transaction) and stamps
/// `system_config.schema_version` only once *all* pending migrations succeed. A
/// crash or timeout part-way through the head migration therefore leaves
/// definitions in place with an old stamp, and the next boot re-runs that file
/// from the top. Plain `DEFINE`/`REMOVE` made that retry fatal —
/// "The table 'reminder' already exists" — so the server refused to start until
/// an operator manually removed the partial definitions, contradicting
/// `migrate()`'s "Safe to call on every startup". Every definition statement is
/// `OVERWRITE` (and removals `IF EXISTS`) so a retry finishes instead of jamming.
#[tokio::test]
async fn test_migrations_retry_after_a_lost_version_stamp() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    // Rewind the stamp while every definition from v001..v007 is still present:
    // exactly what the next boot sees after a mid-migration failure.
    db.inner()
        .query("UPDATE system_config SET value = '5' WHERE key = 'schema_version'")
        .await
        .unwrap()
        .check()
        .unwrap();

    schema::migrate(db.inner()).await.unwrap();

    let mut result = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = result.take(0).unwrap();
    assert_eq!(
        rows[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string(),
        "the retry must re-stamp the head, not just stop erroring"
    );

    // And the schema still behaves: the head migration's defaults survived the
    // second application of its own statements.
    let mut result = db
        .inner()
        .query("SELECT * FROM reminder LIMIT 1")
        .await
        .unwrap();
    assert!(
        result
            .take::<Vec<serde_json::Value>>(0)
            .is_ok_and(|v| v.is_empty()),
        "reminder must remain selectable after the retry"
    );
}

/// The stricter form of the same contract: re-applying the *earliest* migration
/// over a fully built schema must not error either, so a stamp lost at any
/// version self-heals rather than bricking at v001.
#[tokio::test]
async fn test_replaying_a_completed_migration_is_not_an_error() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    for (version, name, sql) in schema::MIGRATIONS {
        let response = db.inner().query(*sql).await.unwrap_or_else(|e| {
            panic!("re-running v{version:03} ({name}) failed at query time: {e}")
        });
        response
            .check()
            .unwrap_or_else(|e| panic!("re-running v{version:03} ({name}) reported an error: {e}"));
    }
}

/// A v008-era store upgrades through v009 without its existing rows gaining ownership, and the head
/// it reports is the registry's head. The point of asserting the *columns* rather than only the row
/// counts: an ownership schema that defaulted a legacy row to somebody's workspace would leave every
/// count here unchanged and turn the first scoped read into a cross-account leak.
#[tokio::test]
async fn test_upgrade_from_v008_adds_ownership_columns_without_touching_rows() {
    let db = Database::connect_embedded().await.unwrap();
    let previous = ACCOUNTS_SCOPES_VERSION - 1;
    for (_, _, sql) in schema::MIGRATIONS
        .iter()
        .filter(|(version, _, _)| *version <= previous)
    {
        db.inner().query(*sql).await.unwrap().check().unwrap();
    }
    db.inner()
        .query(
            "CREATE system_config SET key = 'schema_version', value = $version, updated_at = time::now();",
        )
        .bind(("version", previous.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();

    db.inner()
        .query(
            "CREATE fact SET content = 'before v009', embedding = [1.0, 0.0], tags = [];
             CREATE `session` SET external_id = 'before-v009', tags = [];
             CREATE reminder SET message = 'before v009', schedule_kind = 'once', \
                 next_due_at = d'2030-01-01T00:00:00Z', target_project = 'alexandria';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

    schema::migrate(db.inner()).await.unwrap();

    let stamped: Vec<serde_json::Value> = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(
        stamped[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string(),
        "the head must advance by registering v009, not by a literal in this test"
    );

    for table in [
        "fact",
        "account",
        "workspace",
        "project",
        "credential",
        "ownership_migration",
        "classification_proposal",
        "semantic_operation",
        "semantic_snapshot",
        "maintenance_progress",
    ] {
        let probe: Vec<serde_json::Value> = db
            .inner()
            .query(format!("SELECT * FROM {table} LIMIT 1"))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(
            probe.iter().all(|row| !row.is_null()),
            "{table} is unreadable"
        );
    }

    // The three pre-existing rows survived, unowned, and each still updates.
    let fact: Vec<serde_json::Value> = db
        .inner()
        .query("SELECT * FROM fact")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(fact.len(), 1);
    assert_eq!(fact[0]["content"].as_str().unwrap(), "before v009");
    for column in ["account", "workspace", "scope", "project"] {
        assert!(
            fact[0].get(column).is_none() || fact[0][column].is_null(),
            "v009 gave a legacy fact a {column}: {:?}",
            fact[0]
        );
    }
    db.inner()
        .query("UPDATE fact SET deleted = true")
        .await
        .unwrap()
        .check()
        .unwrap();
    db.inner()
        .query("UPDATE reminder SET status = 'cancelled'")
        .await
        .unwrap()
        .check()
        .unwrap();
    db.inner()
        .query("UPDATE `session` SET summary = 'touched after v009'")
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// Every closed string set v009 asserts is built here from the constants the storage writers use, so
/// renaming a value on either side fails this gate rather than leaving a writer producing rows the
/// engine refuses. The expected text is never typed out twice: each string is assembled from the
/// exported constants, in the order the schema lists them, and `credential`'s authenticating subset
/// is checked as a subset of the kind set the table accepts.
#[tokio::test]
async fn test_v009_closed_sets_match_the_storage_constants() {
    use alexandria_storage::models::{
        account_role, account_status, budget_job, credential_kind, migration_state, operation_kind,
        proposal_state, scope_kind, snapshot_kind,
    };

    let set = |values: &[&str]| -> String {
        format!(
            "[{}]",
            values
                .iter()
                .map(|value| format!("'{value}'"))
                .collect::<Vec<String>>()
                .join(", ")
        )
    };
    let credential_kinds = [
        credential_kind::BROWSER,
        credential_kind::AGENT,
        credential_kind::INVITE,
        credential_kind::RECOVERY,
    ];
    let operation_kinds = [
        operation_kind::STORE,
        operation_kind::IMPORT,
        operation_kind::UPDATE,
        operation_kind::DELETE,
        operation_kind::MOVE,
        operation_kind::CLUSTER_CHANGE,
        operation_kind::COLLAPSE,
        operation_kind::APPRAISE,
        operation_kind::REMINDER_MAP,
        operation_kind::QUALITY,
        operation_kind::REVERT,
        operation_kind::BOOTSTRAP_OWNER,
    ];

    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    // Located by version, not by "the head": v010 is the head now, and a test whose subject is v009's
    // closed sets must keep reading v009 whatever the registry grows. Reading `LATEST_VERSION` here
    // was only ever correct while v009 happened to be last.
    let sql = schema::MIGRATIONS
        .iter()
        .find(|(version, _, _)| *version == ACCOUNTS_SCOPES_VERSION)
        .expect("v009 accounts_scopes is a registered migration")
        .2;

    for (label, expected) in [
        (
            "account status",
            set(&[account_status::ACTIVE, account_status::SUSPENDED]),
        ),
        (
            "account role",
            set(&[account_role::OWNER, account_role::ADMIN]),
        ),
        ("credential kind", set(&credential_kinds)),
        (
            "scope kind",
            set(&[
                scope_kind::PERSONAL,
                scope_kind::PROJECT,
                scope_kind::UNASSIGNED,
            ]),
        ),
        (
            "snapshot variant",
            set(&[
                snapshot_kind::FACT,
                snapshot_kind::EDGE,
                snapshot_kind::CLUSTER,
                snapshot_kind::REMINDER,
            ]),
        ),
        (
            "budget job",
            set(&[
                budget_job::SWEEP,
                budget_job::COLLAPSE,
                budget_job::APPRAISE,
            ]),
        ),
        (
            "proposal state",
            set(&[
                proposal_state::PENDING,
                proposal_state::APPROVED,
                proposal_state::DISMISSED,
                proposal_state::STALE,
            ]),
        ),
        (
            "migration state",
            set(&[
                migration_state::PENDING,
                migration_state::RUNNING,
                migration_state::PAUSED,
                migration_state::VERIFYING,
                migration_state::COMPLETE,
                migration_state::FAILED,
            ]),
        ),
        ("operation kind", set(&operation_kinds)),
    ] {
        assert!(
            sql.contains(expected.as_str()),
            "v009 no longer asserts the {label} set the constants publish: {expected}"
        );
    }

    // The kinds that may authorize a read are a subset of the kinds the table stores, and the two
    // lists are not the same list: an invitation is a real credential row that must not open the
    // corpus.
    for kind in credential_kind::AUTHENTICATING {
        assert!(
            credential_kinds.contains(&kind),
            "`{kind}` authorizes reads but is not a stored credential kind"
        );
    }
    assert!(
        !credential_kinds
            .iter()
            .any(|kind| kind == &credential_kind::INVITE
                && credential_kind::AUTHENTICATING.contains(kind)),
        "an invitation link is in the authorizing set"
    );
}

/// The version the accounts-and-scopes schema is registered at. Named so the v009 gates above locate
/// their own subject instead of assuming it is the head, which stopped being true when v010 landed.
const ACCOUNTS_SCOPES_VERSION: u32 = 9;

/// v010 swaps the session handle index, and the swap must be **complete**: the global
/// `session_external_id_idx` is gone and `session_account_external_id_idx` is the only index over
/// `external_id`. Asserted through `INFO FOR TABLE`, because no row content can tell the two apart —
/// a store that kept the global index *and* gained the qualified one still looks healthy until two
/// accounts try to share a handle.
#[tokio::test]
async fn test_v010_swaps_the_global_session_handle_index_for_an_account_local_one() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let indexes = session_index_definitions(&db).await;
    assert!(
        !indexes
            .iter()
            .any(|definition| definition.contains("session_external_id_idx")),
        "the global handle index survived v010, so a second account sharing an external_id is still \
         refused (and answered with the first account's row): {indexes:?}"
    );
    let qualified = indexes
        .iter()
        .find(|definition| definition.contains("session_account_external_id_idx"))
        .unwrap_or_else(|| panic!("v010 did not define the account-local index: {indexes:?}"));
    assert!(
        qualified.contains("UNIQUE"),
        "the account-local handle index must be UNIQUE or two rows per account are legal: {qualified}"
    );
    assert!(
        qualified.contains("account") && qualified.contains("external_id"),
        "the key must be `(account, external_id)` in some order: {qualified}"
    );
}

/// v010 remains a registered migration after the derived head advances.
#[tokio::test]
async fn test_v010_is_registered_and_the_derived_head_is_stamped() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let named = schema::MIGRATIONS
        .iter()
        .find(|(version, _, _)| *version == ACCOUNTS_SCOPES_VERSION + 1)
        .expect("v010 is a registered entry");
    assert_eq!(named.1, "scoped_sessions");
    assert!(schema::LATEST_VERSION >= named.0);

    let stamped: Vec<serde_json::Value> = db
        .inner()
        .query("SELECT * FROM system_config WHERE key = 'schema_version'")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(
        stamped[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string()
    );
}

/// v010 over a store whose sessions predate ownership: the rows survive, and none of them gains an
/// owner. This is the gate against "helpfully" defaulting a legacy session to the first account,
/// which would hand one owner another owner's history through the scoped readers.
#[tokio::test]
async fn test_v010_leaves_migration_pending_sessions_unowned_and_unreachable() {
    let db = Database::connect_embedded().await.unwrap();
    for (_, _, sql) in schema::MIGRATIONS
        .iter()
        .filter(|(version, _, _)| *version <= ACCOUNTS_SCOPES_VERSION)
    {
        db.inner().query(*sql).await.unwrap().check().unwrap();
    }
    // A v009-era store: two legacy sessions, no owner on either, sharing nothing.
    db.inner()
        .query(
            "CREATE `session` SET external_id = 'legacy-a', tags = [];
             CREATE `session` SET external_id = 'legacy-b', tags = [];",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

    schema::migrate(db.inner()).await.unwrap();

    let rows: Vec<serde_json::Value> = db
        .inner()
        .query("SELECT * FROM `session` ORDER BY external_id")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(rows.len(), 2, "v010 must not delete migration-pending rows");
    for row in &rows {
        for column in ["account", "workspace"] {
            assert!(
                row.get(column).is_none_or(serde_json::Value::is_null),
                "v010 stamped a legacy session with a {column}: {row:?}"
            );
        }
    }

    // Unowned means unreachable to a scoped reader, and the way to show that is the reader's own
    // predicate: `account = $account` matches neither row, whatever account is named.
    let mut probe = db
        .inner()
        .query("SELECT count() FROM `session` WHERE account = $account GROUP ALL;")
        .bind((
            "account",
            surrealdb::types::RecordId::parse_simple("account:anyone").unwrap(),
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    let counted: Vec<serde_json::Value> = probe.take(0).unwrap();
    let reachable = counted
        .first()
        .and_then(|row| row.get("count"))
        .and_then(|count| count.as_i64())
        .unwrap_or(0);
    assert_eq!(
        reachable, 0,
        "a legacy unowned session must not be reachable by naming some account"
    );
}

/// The whole reason the index and its readers cannot ship apart: on a fully migrated store, one
/// handle under two accounts is now legal, and one handle twice under the *same* account is still
/// refused by the index rather than by an application check.
#[tokio::test]
async fn test_v010_permits_one_handle_per_account_and_still_refuses_two_of_the_same() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    db.inner()
        .query(
            "CREATE account:v10a SET login = 'v10a-login', password_hash = 'x';
             CREATE account:v10b SET login = 'v10b-login', password_hash = 'x';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

    db.inner()
        .query(
            "CREATE `session` SET external_id = 'shared', account = account:v10a, tags = [];
             CREATE `session` SET external_id = 'shared', account = account:v10b, tags = [];",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = db
        .inner()
        .query("SELECT count() FROM `session` WHERE external_id = 'shared' GROUP ALL;")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(rows[0]["count"].as_i64().unwrap(), 2);

    // The same account twice under one handle is the duplicate the index exists to stop.
    let duplicate = db
        .inner()
        .query("CREATE `session` SET external_id = 'shared', account = account:v10a, tags = [];")
        .await;
    let refused = match duplicate {
        Err(_) => true,
        Ok(response) => response.check().is_err(),
    };
    assert!(
        refused,
        "the account-local UNIQUE index must refuse a duplicate within one account, not just across \
         accounts"
    );
}

async fn session_index_definitions(db: &Database) -> Vec<String> {
    let mut response = db
        .inner()
        .query("INFO FOR TABLE `session`")
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<serde_json::Value> = response.take(0).unwrap();
    let indexes = rows
        .first()
        .and_then(|row| row.get("indexes"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match indexes {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(name, definition)| format!("{name} {definition}"))
            .collect(),
        other => vec![other.to_string()],
    }
}
