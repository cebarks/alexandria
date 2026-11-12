//! v009 account, ownership, journal and progress schema — the behaviour of the *pinned engine*,
//! not text matching.
//!
//! Everything here goes through `schema::migrate` on a real in-memory SurrealDB and asserts what the
//! engine accepts, rejects and preserves. Two things are deliberately tested together: the new tables
//! enforce their own constraints (one private workspace per account, a workspace-qualified project
//! alias, a workspace-scoped operation key, typed snapshot variants, cursors that cannot name the
//! wrong table), and *nothing* about v009 rewrites or reinterprets a row written in a pre-v009 shape
//! — unowned legacy data stays unowned, which is the migration-pending state, never Personal and
//! never public.
//!
//! Record ids are read back as typed `RecordId`s and re-bound, never parsed out of a display string:
//! `record_id_to_string()` is not a parser-safe SQL round trip, so a test that round-tripped through
//! it would be testing the helper instead of the schema. Every negative below binds what the
//! positive binds — an unbound `$parameter` fails for an unrelated reason and proves nothing.

use alexandria_storage::{Database, record_id_to_string, schema};
use serde_json::Value as Json;
use surrealdb::types::{RecordId, SurrealValue};

#[derive(SurrealValue)]
struct OneId {
    id: RecordId,
}

/// Runs a statement that returns rows, binding every `$parameter` it names, and panics with the
/// engine's own message if the engine refused it.
macro_rules! run {
    ($db:expr, $sql:expr $(, $name:literal, $value:expr )* $(,)?) => {{
        let db: &Database = $db;
        #[allow(unused_mut)]
        let mut query = db.inner().query($sql);
        $(
            query = query.bind(($name, $value));
        )*
        let response = query
            .await
            .unwrap_or_else(|e| panic!("`{}` failed to run: {e}", $sql));
        let mut response = response
            .check()
            .unwrap_or_else(|e| panic!("`{}` was refused: {e}", $sql));
        let rows: Vec<Json> = response.take(0).unwrap_or_default();
        rows
    }};
}

/// The same, expecting refusal: returns the engine's message so the caller can say *why* it failed.
macro_rules! refused {
    ($db:expr, $sql:expr $(, $name:literal, $value:expr )* $(,)?) => {{
        let db: &Database = $db;
        #[allow(unused_mut)]
        let mut query = db.inner().query($sql);
        $(
            query = query.bind(($name, $value));
        )*
        match query.await {
            Ok(response) => match response.check() {
                Err(e) => e.to_string(),
                Ok(_) => panic!("expected refusal from `{}`, but the engine accepted it", $sql),
            },
            Err(e) => e.to_string(),
        }
    }};
}

/// The created/updated row's id, typed, for re-binding.
macro_rules! create_id {
    ($db:expr, $sql:expr $(, $name:literal, $value:expr )* $(,)?) => {{
        let db: &Database = $db;
        #[allow(unused_mut)]
        let mut query = db.inner().query($sql);
        $(
            query = query.bind(($name, $value));
        )*
        let response = query
            .await
            .unwrap_or_else(|e| panic!("`{}` failed to run: {e}", $sql));
        let mut response = response
            .check()
            .unwrap_or_else(|e| panic!("`{}` was refused: {e}", $sql));
        let rows: Vec<OneId> = response.take(0).unwrap_or_default();
        rows.into_iter()
            .next()
            .unwrap_or_else(|| panic!("`{}` returned no row to take an id from", $sql))
            .id
    }};
}

/// A store migrated to the compiled-in head.
async fn migrated() -> Database {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    db
}

/// A store migrated only as far as `version` and stamped as such, so a fixture can be written in the
/// shape the pre-v009 code wrote it and then upgraded for real.
async fn migrated_to(version: u32) -> Database {
    let db = Database::connect_embedded().await.unwrap();
    for (_, _, sql) in schema::MIGRATIONS.iter().filter(|(v, _, _)| *v <= version) {
        db.inner().query(*sql).await.unwrap().check().unwrap();
    }
    run!(
        &db,
        "CREATE system_config SET key = 'schema_version', value = $version, updated_at = time::now();",
        "version",
        version.to_string()
    );
    db
}

/// One row by id, as JSON, with the write-only bookkeeping column dropped so rows can be compared
/// across a migration replay.
async fn row(db: &Database, id: &RecordId) -> Json {
    let rows = run!(
        db,
        "SELECT * OMIT updated_at FROM ONLY $target;",
        "target",
        id.clone()
    );
    let label = record_id_to_string(id);
    assert_eq!(rows.len(), 1, "no readable row for {label}");
    assert!(!rows[0].is_null(), "no readable row for {label}");
    rows.into_iter().next().unwrap()
}

fn count_of(rows: &[Json]) -> i64 {
    rows.first().and_then(|r| r["total"].as_i64()).unwrap_or(0)
}

async fn count_rows(db: &Database, table: &str) -> i64 {
    let sql = format!("SELECT count() AS total FROM {table} GROUP ALL");
    count_of(&run!(db, sql.as_str()))
}

async fn count_where(db: &Database, sql: &str) -> i64 {
    count_of(&run!(db, sql))
}

/// A column a legacy row never had reads as absent-or-null. `IS NOT NULL` and `!= NULL` are both
/// satisfied by `NONE` on this engine, so a test that meant "this row has no owner" has to ask the
/// document rather than the where-clause.
fn has_value(row: &Json, field: &str) -> bool {
    match row.get(field) {
        None | Some(Json::Null) => false,
        Some(_) => true,
    }
}

async fn account(db: &Database, login: &str) -> RecordId {
    create_id!(
        db,
        "CREATE account SET login = $login, password_hash = $hash;",
        "login",
        login.to_string(),
        "hash",
        "$argon2id$fixture-hash-not-a-secret"
    )
}

async fn workspace_of(db: &Database, owner: &RecordId) -> RecordId {
    create_id!(
        db,
        "CREATE workspace SET owner = $owner;",
        "owner",
        owner.clone()
    )
}

async fn project(db: &Database, in_workspace: &RecordId, alias: &str) -> RecordId {
    create_id!(
        db,
        "CREATE project SET workspace = $workspace, normalized_alias = $alias;",
        "workspace",
        in_workspace.clone(),
        "alias",
        alias.to_string()
    )
}

async fn fact(db: &Database, content: &str) -> RecordId {
    create_id!(
        db,
        "CREATE fact SET content = $content, embedding = [1.0, 0.0], tags = [];",
        "content",
        content.to_string()
    )
}

async fn operation(
    db: &Database,
    in_workspace: &RecordId,
    actor: &RecordId,
    key: &str,
    fingerprint: &str,
    effects: &[&str],
) -> RecordId {
    let effects: Vec<String> = effects.iter().map(|e| (*e).to_string()).collect();
    create_id!(
        db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = $key, request_fingerprint = $fingerprint, expected_effects = $effects;",
        "workspace",
        in_workspace.clone(),
        "actor",
        actor.clone(),
        "key",
        key.to_string(),
        "fingerprint",
        fingerprint.to_string(),
        "effects",
        effects
    )
}

/// The `id` of a row that is already known to exist, re-read as a typed record so a repository's
/// string id can be bound again without parsing a display form.
async fn typed_id(db: &Database, id_sql: &str) -> RecordId {
    let rows = run!(
        db,
        "SELECT id FROM type::record($id);",
        "id",
        id_sql.to_string()
    );
    let raw = rows
        .first()
        .and_then(|r| r["id"].as_str())
        .unwrap_or_else(|| panic!("no row readable for {id_sql}"))
        .to_string();
    // The engine hands the id back in its own canonical form; re-binding it as a record path is the
    // one place a test needs a string, and `type::record` does the parsing, not Rust.
    let (table, key) = raw.split_once(':').expect("a record id has a table");
    RecordId::new(table.to_string(), key.trim_matches('`').to_string())
}

/// The head must be the registered last entry, and the version a migrated store reports must come from
/// the registry rather than a literal — every other migration test asserts against `LATEST_VERSION`, so
/// a head that is written but not registered is a silent mismatch.
///
/// Asserted by *name* against `MIGRATIONS.last()` rather than against a hard-coded v009: this gate's job
/// is "the file and the registry agree", and pinning a specific version number would make it a second
/// place to remember the head.
#[tokio::test]
async fn the_head_migration_is_registered_and_advances_the_stamped_version() {
    let (head_version, head_name, _) = schema::MIGRATIONS.last().unwrap();
    assert_eq!(
        *head_name, "scoped_sessions",
        "the head migration must be v010_scoped_sessions.surql"
    );
    assert_eq!(schema::LATEST_VERSION, *head_version);

    let db = migrated().await;
    let stamped = run!(
        &db,
        "SELECT * FROM system_config WHERE key = 'schema_version' LIMIT 1"
    );
    assert_eq!(stamped.len(), 1);
    assert_eq!(
        stamped[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string()
    );
}

/// The failure this pins: a migration that "cleans up" while it defines. Replaying v009 over a
/// populated ownership schema must be a pure definition pass — an account keeps its status, its
/// authentication epoch, its credential and its project alias, byte-identical. A `DEFINE FIELD` with
/// an `UPDATE … SET` backfill beside it would silently reset an owner's state on exactly the
/// boot-after-crash this simulates, which is the retry path `migrate()` documents.
#[tokio::test]
async fn migration_replay_preserves_accounts() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let alias_project = project(&db, &alice_workspace, "alexandria").await;
    let credential = create_id!(
        &db,
        "CREATE credential SET account = $account, kind = 'agent', label = 'pi', auth_epoch = 3, \
         secret_digest = $digest;",
        "account",
        alice.clone(),
        "digest",
        "agent-digest-fixture-value-000000000000000000"
    );

    // A real state change, so a "defaults reset everything" replay is visible.
    run!(
        &db,
        "UPDATE ONLY $target SET status = 'suspended', auth_epoch = 9;",
        "target",
        alice.clone()
    );

    let before = row(&db, &alice).await;
    assert_eq!(before["status"].as_str().unwrap(), "suspended");
    assert_eq!(before["auth_epoch"], Json::Number(9.into()));
    let before_credential = row(&db, &credential).await;
    let before_project = row(&db, &alias_project).await;
    let before_workspace = row(&db, &alice_workspace).await;

    // Re-apply the head file wholesale: exactly what a boot after a mid-file crash does.
    let head_sql = schema::MIGRATIONS.last().unwrap().2.to_string();
    run!(&db, head_sql.as_str());

    assert_eq!(
        row(&db, &alice).await,
        before,
        "replaying v009 rewrote account state"
    );
    assert_eq!(
        row(&db, &credential).await,
        before_credential,
        "replaying v009 rewrote a credential row"
    );
    assert_eq!(
        row(&db, &alias_project).await,
        before_project,
        "replaying v009 rewrote a project row"
    );
    assert_eq!(
        row(&db, &alice_workspace).await,
        before_workspace,
        "replaying v009 rewrote a workspace row"
    );
    assert_eq!(count_rows(&db, "account").await, 1);
    assert_eq!(count_rows(&db, "workspace").await, 1);
    assert_eq!(count_rows(&db, "credential").await, 1);

    // And the retry path through `migrate()` itself: rewind the stamp, keep the data, re-run.
    run!(
        &db,
        "UPDATE system_config SET value = $previous WHERE key = 'schema_version';",
        "previous",
        (schema::LATEST_VERSION - 1).to_string()
    );
    schema::migrate(db.inner()).await.unwrap();

    let stamped = run!(
        &db,
        "SELECT * FROM system_config WHERE key = 'schema_version' LIMIT 1"
    );
    assert_eq!(
        stamped[0]["value"].as_str().unwrap(),
        schema::LATEST_VERSION.to_string(),
        "the retry must re-stamp the head, not just stop erroring"
    );
    assert_eq!(
        row(&db, &alice).await,
        before,
        "the migrate() retry rewrote account state"
    );
}

/// Project identity is workspace-qualified: two accounts with the same repository alias get two
/// projects and no shared data, while a second project with that alias inside *one* workspace is
/// refused by the schema rather than by an application convention a new call site can forget.
#[tokio::test]
async fn same_alias_in_two_workspaces() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let bob_workspace = workspace_of(&db, &bob).await;

    let alice_project = project(&db, &alice_workspace, "alexandria").await;
    let bob_project = project(&db, &bob_workspace, "alexandria").await;
    assert_ne!(
        alice_project, bob_project,
        "the same alias in two workspaces must be two distinct projects"
    );
    assert_eq!(count_rows(&db, "project").await, 2);

    let error = refused!(
        &db,
        "CREATE project SET workspace = $workspace, normalized_alias = 'alexandria';",
        "workspace",
        alice_workspace.clone()
    );
    assert!(
        error.contains("idx_project_alias") || error.contains("already contains"),
        "one workspace accepted the same normalized alias twice, and not through the qualified \
         index: {error}"
    );

    // Explicit additional aliases are qualified the same way: one row per alias, unique within the
    // workspace, and reusable by another workspace.
    run!(
        &db,
        "CREATE project_alias SET workspace = $workspace, project = $project, alias = 'git-remote-a';",
        "workspace",
        alice_workspace.clone(),
        "project",
        alice_project.clone()
    );
    let duplicate_alias = refused!(
        &db,
        "CREATE project_alias SET workspace = $workspace, project = $project, alias = 'git-remote-a';",
        "workspace",
        alice_workspace.clone(),
        "project",
        bob_project.clone()
    );
    assert!(
        duplicate_alias.contains("idx_project_alias_lookup")
            || duplicate_alias.contains("already contains"),
        "one workspace accepted the same alias for two projects: {duplicate_alias}"
    );
    run!(
        &db,
        "CREATE project_alias SET workspace = $workspace, project = $project, alias = 'git-remote-a';",
        "workspace",
        bob_workspace.clone(),
        "project",
        bob_project.clone()
    );

    // Each account's qualified lookup resolves exactly its own project and never the other's.
    let for_alice = run!(
        &db,
        "SELECT * FROM project WHERE workspace = $w AND normalized_alias = 'alexandria';",
        "w",
        alice_workspace.clone()
    );
    assert_eq!(
        for_alice.len(),
        1,
        "alice's alias must resolve to exactly one project: {for_alice:?}"
    );
    let for_bob = run!(
        &db,
        "SELECT * FROM project WHERE workspace = $w AND normalized_alias = 'alexandria';",
        "w",
        bob_workspace.clone()
    );
    assert_eq!(for_bob.len(), 1);
    assert_eq!(
        for_bob[0]["id"],
        Json::String(record_id_to_string(&bob_project)),
        "bob's lookup resolved somebody else's project"
    );
}

/// One private workspace per account, and no workspace without an owner: the unit ownership resolves
/// to cannot be doubled, cannot be orphaned, and cannot be claimed by a second account.
#[tokio::test]
async fn workspace_is_private_to_exactly_one_owner() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let first = workspace_of(&db, &alice).await;

    let error = refused!(
        &db,
        "CREATE workspace SET owner = $account;",
        "account",
        alice.clone()
    );
    assert!(
        error.contains("idx_workspace_owner") || error.contains("already contains"),
        "one account was given a second private workspace: {error}"
    );

    refused!(&db, "CREATE workspace SET owner = NONE;");
    refused!(&db, "CREATE account;");
    refused!(
        &db,
        "CREATE account SET login = 'MixedCase', password_hash = 'x';"
    );
    refused!(&db, "CREATE account SET login = 'al', password_hash = 'x';");
    refused!(
        &db,
        "CREATE account SET login = 'alice', password_hash = 'x';"
    );
    refused!(
        &db,
        "CREATE account SET login = 'alice', password_hash = '   ';"
    );
    refused!(
        &db,
        "CREATE credential SET account = $account, kind = 'not-a-kind';",
        "account",
        alice.clone()
    );

    // Positive control from the same fixture: the first workspace is real and readable.
    let stored = row(&db, &first).await;
    assert!(
        has_value(&stored, "owner"),
        "the workspace lost its owner: {stored:?}"
    );
    assert_eq!(count_rows(&db, "workspace").await, 1);
}

/// Undefined ownership is the migration-pending state and nothing else. A fact written in the
/// pre-v009 shape comes back with NO value for account/workspace/scope — not `personal`, not a
/// nominated account — and the partition predicate every scoped reader uses excludes it. If v009
/// defaulted `scope` to 'personal', or rode in with an `UPDATE fact SET …` backfill, every legacy
/// memory would become readable in the first account's automatic recall.
#[tokio::test]
async fn missing_ownership_never_means_personal() {
    let legacy = migrated_to(8).await;
    let legacy_id = fact(&legacy, "legacy unowned memory").await;
    // The ordinary live write path, unchanged: no owner, no workspace, no scope.
    run!(
        &legacy,
        "UPDATE ONLY $target SET deleted = true;",
        "target",
        legacy_id.clone()
    );
    schema::migrate(legacy.inner()).await.unwrap();

    let unowned = row(&legacy, &legacy_id).await;
    assert!(
        !has_value(&unowned, "account"),
        "v009 invented an owner for a legacy fact: {unowned:?}"
    );
    assert!(
        !has_value(&unowned, "workspace"),
        "v009 invented a workspace for a legacy fact: {unowned:?}"
    );
    assert!(
        !has_value(&unowned, "scope"),
        "an unowned fact must not read as a scope, least of all personal: {unowned:?}"
    );
    assert_eq!(
        unowned["content"].as_str().unwrap(),
        "legacy unowned memory",
        "a legacy row must survive the upgrade intact"
    );

    let alice = account(&legacy, "alice").await;
    let alice_workspace = workspace_of(&legacy, &alice).await;
    let owned = create_id!(
        &legacy,
        "CREATE fact SET content = 'owned', embedding = [1.0, 0.0], tags = [], account = $account, \
         workspace = $workspace, scope = 'personal';",
        "account",
        alice.clone(),
        "workspace",
        alice_workspace.clone()
    );
    let bob = account(&legacy, "bob").await;
    let bob_workspace = workspace_of(&legacy, &bob).await;

    // The partition predicate: alice's Personal partition holds alice's owned rows only, and bob's
    // Personal partition — same scope name, different workspace — holds none of them.
    let alice_visible = run!(
        &legacy,
        "SELECT * FROM fact WHERE workspace = $w AND scope = 'personal' AND deleted = false;",
        "w",
        alice_workspace.clone()
    );
    assert_eq!(
        alice_visible.len(),
        1,
        "alice's Personal partition should hold exactly her owned row: {alice_visible:?}"
    );
    let bob_visible = run!(
        &legacy,
        "SELECT * FROM fact WHERE workspace = $w AND scope = 'personal' AND deleted = false;",
        "w",
        bob_workspace.clone()
    );
    assert!(
        bob_visible.is_empty(),
        "bob's Personal partition reached alice's rows: {bob_visible:?}"
    );

    // And a row with no scope at all is excluded from *every* partition, including Unassigned:
    // unowned is not a wildcard, and Unassigned is a decision rather than an absence.
    for partition in ["personal", "project", "unassigned"] {
        let matched = run!(
            &legacy,
            "SELECT * FROM fact WHERE workspace = $w AND scope = $partition;",
            "w",
            alice_workspace.clone(),
            "partition",
            partition.to_string()
        );
        assert!(
            !matched
                .iter()
                .any(|r| r["content"] == Json::String("legacy unowned memory".into())),
            "partition `{partition}` selected a row with no ownership at all"
        );
    }

    // A scope outside the three shapes is refused.
    refused!(
        &legacy,
        "UPDATE ONLY $target SET scope = 'public';",
        "target",
        owned.clone()
    );
    refused!(
        &legacy,
        "UPDATE ONLY $target SET scope = 'shared';",
        "target",
        legacy_id.clone()
    );
}

/// A journal row is keyed `(workspace, operation_key)`, never a globally client-chosen key: two
/// owners may both use `retry-1`, and one owner cannot get a second operation — let alone a
/// *different* request — under a key that already committed. The refused write must leave the
/// recorded request untouched, so a retry can never be answered with somebody's earlier success.
#[tokio::test]
async fn operation_key_is_isolated_per_workspace() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let bob_workspace = workspace_of(&db, &bob).await;

    let first = operation(
        &db,
        &alice_workspace,
        &alice,
        "retry-1",
        "fingerprint-original-value",
        &["fact:content"],
    )
    .await;
    let bob_same_key = operation(
        &db,
        &bob_workspace,
        &bob,
        "retry-1",
        "fingerprint-original-value",
        &["fact:content"],
    )
    .await;
    assert_ne!(
        first, bob_same_key,
        "one key resolved two owners' operations"
    );
    assert_eq!(count_rows(&db, "semantic_operation").await, 2);

    // Same key, same workspace, different requested effects: the second row cannot exist, so no
    // lookup by key can return the original success for a different request.
    let error = refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = 'retry-1', request_fingerprint = 'fingerprint-CHANGED-value', \
         expected_effects = ['fact:content', 'edge:add'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone()
    );
    assert!(
        error.contains("idx_semantic_operation_key") || error.contains("already contains"),
        "a workspace accepted its committed operation key for a different request: {error}"
    );
    let kept = row(&db, &first).await;
    assert_eq!(
        kept["request_fingerprint"].as_str().unwrap(),
        "fingerprint-original-value",
        "the conflicting write overwrote the recorded request"
    );
    assert_eq!(kept["expected_effects"].as_array().unwrap().len(), 1);
    assert_eq!(count_rows(&db, "semantic_operation").await, 2);

    // An operation with no actor is not auditable; a blank key, an empty effect set, an unknown
    // kind and a scope kind outside the three shapes are not anchors for anything.
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, kind = 'update', \
         operation_key = 'anon-key', request_fingerprint = 'fingerprint-anon-value-00', \
         expected_effects = ['fact:content'];",
        "workspace",
        alice_workspace.clone()
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = '   ', request_fingerprint = 'fingerprint-blank-value-000', \
         expected_effects = ['fact:content'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = 'no-effects', request_fingerprint = 'fingerprint-empty-value-00', \
         expected_effects = [];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'do-anything', \
         operation_key = 'bad-kind', request_fingerprint = 'fingerprint-kind-value-000', \
         expected_effects = ['fact:content'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = 'bad-scope', request_fingerprint = 'fingerprint-scope-value-000', \
         expected_effects = ['fact:content'], affected_scope_kinds = ['public'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = 'short-fp', request_fingerprint = 'abc', expected_effects = ['fact:content'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, kind = 'update', \
         operation_key = 'blank-actor', request_fingerprint = 'fingerprint-null-actor-value-0';",
        "workspace",
        alice_workspace.clone()
    );

    // An `initiator` must be a credential, not an account: the journal distinguishes whose authority
    // from which presented credential, and a wrong-typed link would silently survive a read.
    let credential = create_id!(
        &db,
        "CREATE credential SET account = $account, kind = 'browser';",
        "account",
        alice.clone()
    );
    let linked = create_id!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, initiator = \
         $credential, kind = 'update', operation_key = 'with-initiator', request_fingerprint = \
         'fingerprint-initiator-value', expected_effects = ['fact:content'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone(),
        "credential",
        credential.clone()
    );
    assert_eq!(
        row(&db, &linked).await["initiator"],
        Json::String(record_id_to_string(&credential))
    );
    refused!(
        &db,
        "CREATE semantic_operation SET workspace = $workspace, actor = $actor, initiator = \
         $account, kind = 'update', operation_key = 'wrong-initiator', request_fingerprint = \
         'fingerprint-wrong-init-value', expected_effects = ['fact:content'];",
        "workspace",
        alice_workspace.clone(),
        "actor",
        alice.clone(),
        "account",
        alice.clone()
    );
    // A revert link is a link to an operation, not to anything with an id.
    refused!(
        &db,
        "UPDATE ONLY $target SET reverts = $account;",
        "target",
        linked.clone(),
        "account",
        alice.clone()
    );
    run!(
        &db,
        "UPDATE ONLY $target SET reverts = $other, committed_at = time::now();",
        "target",
        linked.clone(),
        "other",
        first.clone()
    );
    let reverted = row(&db, &linked).await;
    assert_eq!(
        reverted["reverts"],
        Json::String(record_id_to_string(&first)),
        "the reverted-operation link is the journal's only reversibility trail"
    );
    assert!(has_value(&reverted, "committed_at"));
}

/// Snapshots are typed variants, and they are not general-purpose record dumps. The `fact` variant
/// carries the semantic state and the embedding identity it was written under; the `reminder` variant
/// carries the mapping decision; an unknown variant, a subject that is not a row of the variant's own
/// table, a payload written under the wrong variant, and a variant that never names its own record are
/// all refused by the engine. Anything that could restore an access clock or a delivery counter has
/// no column to be written into at all.
#[tokio::test]
async fn snapshots_are_typed_and_cannot_carry_access_or_clock_payload() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let alice_project = project(&db, &alice_workspace, "alexandria").await;
    let op = operation(
        &db,
        &alice_workspace,
        &alice,
        "snap-1",
        "fingerprint-snapshot-value",
        &["fact:content"],
    )
    .await;
    let fact_id = fact(&db, "snapshotted content").await;
    let other_fact = fact(&db, "edge endpoint").await;
    let reminder = create_id!(
        &db,
        "CREATE reminder SET message = 'snapshotted reminder', schedule_kind = 'once', \
         next_due_at = d'2030-01-01T00:00:00Z';"
    );
    let edge = create_id!(
        &db,
        "RELATE $in_node->memory_edge->$out_node SET edge_type = 'relates_to', strength = 0.8;",
        "in_node",
        fact_id.clone(),
        "out_node",
        other_fact.clone()
    );
    let cluster = create_id!(
        &db,
        "CREATE cluster SET label = 'snapshotted', centroid = [1.0, 0.0], depth = 0"
    );

    // The fact variant: semantic data plus embedding identity, and the variant's own handle.
    let fact_snapshot = create_id!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
         subject = $subject, fact = $subject, fact_content = 'snapshotted content', \
         fact_tags = ['t'], fact_confidence = 0.5, fact_deleted = false, fact_scope = 'personal', \
         fact_project = $project, fact_revision = 3, fact_embedding = [1.0, 0.0], \
         fact_embedding_model = 'minilm', fact_embedding_dimensions = 2, fact_relations = [$edge];",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        fact_id.clone(),
        "project",
        alice_project.clone(),
        "edge",
        edge.clone()
    );
    let stored = row(&db, &fact_snapshot).await;
    assert_eq!(
        stored["fact_content"].as_str().unwrap(),
        "snapshotted content"
    );
    assert_eq!(
        stored["fact_embedding_model"].as_str().unwrap(),
        "minilm",
        "the embedding identity rides with the vector: {stored:?}"
    );
    assert_eq!(stored["fact_embedding_dimensions"], Json::Number(2.into()));
    assert_eq!(stored["fact_revision"], Json::Number(3.into()));
    assert_eq!(
        stored["fact_relations"]
            .as_array()
            .expect("the guarded relation ids ride with the fact's preimage")
            .len(),
        1
    );

    // The other three variants are writable in their own shapes.
    create_id!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'edge', \
         subject = $subject, edge = $subject, edge_in = $in, edge_out = $out, \
         edge_type = 'relates_to', edge_strength = 0.8;",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        edge.clone(),
        "in",
        fact_id.clone(),
        "out",
        other_fact.clone()
    );
    create_id!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'cluster', \
         subject = $subject, cluster = $subject, cluster_label = 'snapshotted', \
         cluster_centroid = [1.0, 0.0], cluster_depth = 0, cluster_membership_revision = 1, \
         cluster_member_embedding_revision = 2, cluster_centroid_revision = 3;",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        cluster.clone()
    );
    let reminder_snapshot = create_id!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'reminder', \
         subject = $subject, reminder = $subject, reminder_account = $actor, \
         reminder_message = 'snapshotted reminder', reminder_target_project = 'alexandria', \
         reminder_legacy_target_project = 'alexandria', reminder_project = $project, \
         reminder_migration_hold = true, reminder_semantic_revision = 4, \
         reminder_schedule_kind = 'once';",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        reminder.clone(),
        "actor",
        alice.clone(),
        "project",
        alice_project.clone()
    );
    let reminder_row = row(&db, &reminder_snapshot).await;
    assert_eq!(
        reminder_row["reminder_migration_hold"],
        Json::Bool(true),
        "the mapping decision must be restorable: {reminder_row:?}"
    );
    assert_eq!(
        reminder_row["reminder_legacy_target_project"]
            .as_str()
            .unwrap(),
        "alexandria",
        "the original target text survives verbatim as provenance"
    );

    // Nothing that could rewind a delivery clock or a delivery counter has a column at all: these
    // are the columns a rollback must not be able to carry, and the engine says so.
    for column in [
        "reminder_delivered_count = 5",
        "reminder_last_delivered_at = d'2026-01-01T00:00:00Z'",
        "reminder_next_due_at = d'2026-01-01T00:00:00Z'",
        "reminder_status = 'pending'",
        "reminder_cancelled_at = d'2026-01-01T00:00:00Z'",
    ] {
        let sql = format!(
            "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'reminder', \
             subject = $subject, reminder = $subject, reminder_account = $actor, {column};"
        );
        let error = refused!(
            &db,
            sql.as_str(),
            "w",
            alice_workspace.clone(),
            "op",
            op.clone(),
            "subject",
            reminder.clone(),
            "actor",
            alice.clone()
        );
        assert!(
            error.contains("no such field exists"),
            "a reminder snapshot accepted a delivery-clock column `{column}`: {error}"
        );
    }

    // Nor can a fact snapshot carry heat or access history — including the real column names, which
    // is the difference between "the writer promises not to" and "there is nowhere to put it".
    for column in [
        "heat = 0.1",
        "stability = 7.0",
        "access_count = 12",
        "last_touched = d'2026-01-01T00:00:00Z'",
        "last_accessed_at = d'2026-01-01T00:00:00Z'",
        "fact_heat = 0.1",
        "fact_access_count = 12",
        "fact_last_accessed_at = d'2026-01-01T00:00:00Z'",
        "fact_stability = 3.0",
        "reminder_heat = 0.1",
        "delivered_count = 4",
    ] {
        let sql = format!(
            "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
             subject = $subject, fact = $subject, fact_content = 'x', {column};"
        );
        let error = refused!(
            &db,
            sql.as_str(),
            "w",
            alice_workspace.clone(),
            "op",
            op.clone(),
            "subject",
            fact_id.clone()
        );
        assert!(
            error.contains("no such field exists"),
            "a snapshot accepted an access column `{column}`: {error}"
        );
    }

    // The reminder columns that carry their own extra rule are the easy ones to get wrong: an
    // assertion written `$this.kind = 'reminder' AND $value = NONE OR …` parses as
    // `(kind = 'reminder' AND value = NONE) OR …`, so the trailing disjunct accepts the value under
    // *any* variant. These two are the columns where that mattered, and both are refused on a fact
    // snapshot while the same values are accepted on a reminder snapshot.
    for (column, value) in [
        ("reminder_semantic_revision", "4"),
        ("reminder_schedule_kind", "'cron'"),
    ] {
        let sql = format!(
            "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
             subject = $subject, fact = $subject, fact_content = 'x', {column} = {value};"
        );
        let error = refused!(
            &db,
            sql.as_str(),
            "w",
            alice_workspace.clone(),
            "op",
            op.clone(),
            "subject",
            fact_id.clone()
        );
        assert!(
            error.contains(column) || error.contains("kind"),
            "a fact snapshot accepted the reminder column `{column}` for a reason that says \
             nothing about the rule: {error}"
        );
    }
    create_id!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'reminder', \
         subject = $subject, reminder = $subject, reminder_account = $actor, \
         reminder_message = 'mapping', reminder_semantic_revision = 4, \
         reminder_schedule_kind = 'cron';",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        reminder.clone(),
        "actor",
        alice.clone()
    );

    // Variants are closed, and a variant cannot be pointed at another table's row or written under
    // the wrong name. Without these, "typed variants" is a comment.
    refused!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'session', \
         subject = $subject, fact = $subject, fact_content = 'x';",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        fact_id.clone()
    );
    let mismatched_subject = refused!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
         subject = $subject, fact = $subject, fact_content = 'x';",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        reminder.clone()
    );
    assert!(
        mismatched_subject.contains("fact"),
        "the subject/variant refusal should name the type it wanted: {mismatched_subject}"
    );
    refused!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
         subject = $subject, fact = $subject, fact_content = 'x', reminder_message = 'leak';",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        fact_id.clone()
    );
    refused!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
         subject = $subject, fact_content = 'x';",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        fact_id.clone()
    );
    refused!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'edge', \
         subject = $subject, edge = $edge_id, edge_in = $in, edge_out = $in;",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        fact_id.clone(),
        "edge_id",
        edge.clone(),
        "in",
        fact_id.clone()
    );
    // A snapshot must belong to an operation and a workspace: an unlinkable preimage is not audit
    // material.
    refused!(
        &db,
        "CREATE semantic_snapshot SET kind = 'fact', subject = $subject, fact = $subject, \
         fact_content = 'x';",
        "subject",
        fact_id.clone()
    );
    refused!(
        &db,
        "CREATE semantic_snapshot SET operation = $op, kind = 'fact', subject = $subject, \
         fact = $subject, fact_content = 'x';",
        "op",
        op.clone(),
        "subject",
        fact_id.clone()
    );
    // A zero-dimensional embedding is not an embedding identity.
    refused!(
        &db,
        "CREATE semantic_snapshot SET workspace = $w, operation = $op, kind = 'fact', \
         subject = $subject, fact = $subject, fact_content = 'x', fact_embedding_dimensions = 0;",
        "w",
        alice_workspace.clone(),
        "op",
        op.clone(),
        "subject",
        fact_id.clone()
    );
}

/// Legacy reminder payloads keep writing, and the hold/revision columns arrive as
/// absent-or-default rather than as a guess about the owner. On this engine a `DEFAULT` is applied at
/// CREATE only and never retroactively, so a pre-v009 reminder must still read, must still update — a
/// bare non-optional column would fail that update with a coerce error — and must still be nobody's
/// until Task 14 maps it. `target_project` keeps its v007 meaning untouched.
#[tokio::test]
async fn reminder_hold_defaults_are_migration_safe() {
    let legacy = migrated_to(7).await;
    let legacy_id = create_id!(
        &legacy,
        "CREATE reminder SET message = 'stand up', target_project = 'alexandria', \
         schedule_kind = 'once', due_at = d'2026-01-01T00:00:00Z', \
         next_due_at = d'2026-01-01T00:00:00Z', status = 'pending', delivered_count = 2;"
    );
    let before = row(&legacy, &legacy_id).await;

    schema::migrate(legacy.inner()).await.unwrap();

    let after = row(&legacy, &legacy_id).await;
    // v007 content untouched: same text, same target, same delivery history and clock.
    assert_eq!(after["message"], before["message"]);
    assert_eq!(after["target_project"], before["target_project"]);
    assert_eq!(after["delivered_count"], before["delivered_count"]);
    assert_eq!(after["next_due_at"], before["next_due_at"]);
    assert_eq!(after["status"], before["status"]);
    // And nothing was inferred about who owns it or what it is held for.
    assert!(
        !has_value(&after, "account"),
        "v009 invented a reminder owner: {after:?}"
    );
    assert!(
        !has_value(&after, "migration_hold"),
        "a legacy reminder row was rewritten by the schema replay: {after:?}"
    );
    assert!(!has_value(&after, "legacy_target_project"));
    assert!(!has_value(&after, "project"));

    // The ordinary live write path still works on that row — the reason these columns are options
    // with defaults rather than bare columns with defaults.
    run!(
        &legacy,
        "UPDATE ONLY $target SET delivered_count = 3, last_delivered_at = time::now();",
        "target",
        legacy_id.clone()
    );
    run!(
        &legacy,
        "UPDATE ONLY $target SET status = 'cancelled', cancelled_at = time::now();",
        "target",
        legacy_id.clone()
    );

    // A row written *after* v009, in the same legacy shape, gets the safe defaults.
    let created = create_id!(
        &legacy,
        "CREATE reminder SET message = 'new but unowned', target_project = 'alexandria', \
         schedule_kind = 'once', next_due_at = d'2030-01-01T00:00:00Z';"
    );
    let created_row = row(&legacy, &created).await;
    assert_eq!(
        created_row["migration_hold"],
        Json::Bool(false),
        "a new reminder must default to not-held: {created_row:?}"
    );
    assert_eq!(
        created_row["semantic_revision"],
        Json::Number(0.into()),
        "{created_row:?}"
    );
    assert!(
        !has_value(&created_row, "account"),
        "no default owner was invented: {created_row:?}"
    );
    assert!(!has_value(&created_row, "project"));

    // The trap Tasks 6/12 inherit, measured before anything is held: the pre-v009 row has NO value
    // for the column, so `= false` — the predicate that looks obvious — silently drops it, while
    // `!= true` keeps it deliverable.
    let none_rows = count_where(
        &legacy,
        "SELECT count() AS total FROM reminder WHERE migration_hold = NONE GROUP ALL",
    )
    .await;
    let false_rows = count_where(
        &legacy,
        "SELECT count() AS total FROM reminder WHERE migration_hold = false GROUP ALL",
    )
    .await;
    let deliverable_before = count_where(
        &legacy,
        "SELECT count() AS total FROM reminder WHERE migration_hold != true GROUP ALL",
    )
    .await;
    assert_eq!(none_rows, 1, "the pre-v009 row still has no hold value");
    assert_eq!(
        false_rows, 1,
        "only the post-v009 row carries an explicit false"
    );
    assert_eq!(
        deliverable_before, 2,
        "`!= true` must also deliver the rows that predate the column"
    );

    // The hold is real and expressible, and a legacy name is copied verbatim rather than cast into a
    // project id.
    run!(
        &legacy,
        "UPDATE ONLY $target SET migration_hold = true, legacy_target_project = 'alexandria';",
        "target",
        legacy_id.clone()
    );
    let held = row(&legacy, &legacy_id).await;
    assert_eq!(held["migration_hold"], Json::Bool(true));
    assert_eq!(
        held["legacy_target_project"].as_str().unwrap(),
        "alexandria",
        "the original target text must survive verbatim"
    );
    refused!(
        &legacy,
        "UPDATE ONLY $target SET migration_hold = 'held-until-mapped';",
        "target",
        legacy_id.clone()
    );
    refused!(
        &legacy,
        "UPDATE ONLY $target SET semantic_revision = -1;",
        "target",
        legacy_id.clone()
    );
    // A `project` handle has to be a project.
    refused!(
        &legacy,
        "UPDATE ONLY $target SET project = $fact;",
        "target",
        legacy_id.clone(),
        "fact",
        fact(&legacy, "not a project").await
    );

    // The trap Tasks 6/12 inherit: `= false` silently misses every row that predates the column, so
    // the held population is `= true` and the deliverable population is `!= true`.
    let all = count_rows(&legacy, "reminder").await;
    let held_now = count_where(
        &legacy,
        "SELECT count() AS total FROM reminder WHERE migration_hold = true GROUP ALL",
    )
    .await;
    let deliverable_after = count_where(
        &legacy,
        "SELECT count() AS total FROM reminder WHERE migration_hold != true GROUP ALL",
    )
    .await;
    assert_eq!(all, 2);
    assert_eq!(held_now, 1, "exactly one held reminder");
    assert_eq!(
        deliverable_after, 1,
        "holding the unmapped legacy reminder must take it out of the due population"
    );
}

/// `maintenance_progress` is system coordination data, not corpus content and not an audit row:
/// exactly one versioned record per row-capped job, an optional last-workspace cursor, and typed
/// per-workspace scope cursors whose records cannot name the wrong table — and a schema replay must
/// leave an in-flight cursor exactly where it was, because Task 12's fairness depends on a partial
/// pass not restarting at the first partition. The fixture is raw SQL on purpose: this cannot pass by
/// calling a Task 12 repo method that does not exist yet.
#[tokio::test]
async fn maintenance_progress_schema_replay_preserves_cursor() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let bob = account(&db, "bob").await;
    let bob_workspace = workspace_of(&db, &bob).await;
    let bob_project = project(&db, &bob_workspace, "alexandria").await;

    let sweep = create_id!(
        &db,
        "CREATE maintenance_progress SET job = 'sweep', version = 4, last_workspace = $w, \
         cursors = [{workspace: $w, scope: 'personal', project: NONE, row_cursor: $row, \
                     units_done: 17, exhausted: false}, \
                    {workspace: $other, scope: 'project', project: $project, units_done: 0}];",
        "w",
        alice_workspace.clone(),
        "other",
        bob_workspace.clone(),
        "project",
        bob_project.clone(),
        "row",
        fact(&db, "cursor target").await
    );
    let before = row(&db, &sweep).await;
    assert_eq!(before["version"], Json::Number(4.into()));
    let cursors = before["cursors"].as_array().expect("cursors is an array");
    assert_eq!(cursors.len(), 2);
    assert_eq!(cursors[0]["units_done"], Json::Number(17.into()));
    assert_eq!(cursors[0]["exhausted"], Json::Bool(false));
    assert_eq!(cursors[1]["scope"].as_str().unwrap(), "project");
    assert_ne!(
        cursors[0]["workspace"], cursors[1]["workspace"],
        "the two cursors must be per-workspace, not one copied pair"
    );

    // One ledger per capped job: a second sweep row would let two passes each spend an allowance.
    let error = refused!(
        &db,
        "CREATE maintenance_progress SET job = 'sweep', version = 0;"
    );
    assert!(
        error.contains("idx_maintenance_progress_job") || error.contains("already contains"),
        "a second progress record for one job was accepted: {error}"
    );
    for job in ["collapse", "appraise"] {
        run!(
            &db,
            "CREATE maintenance_progress SET job = $job, version = 0;",
            "job",
            job.to_string()
        );
    }
    // The uncapped jobs are absent on purpose: cluster/merge have no row budget, `refine` is Task
    // 19's enqueue cadence rather than a row-capped ledger, and sweep has a ledger but still no
    // audit row (`models::maintenance::job` excludes it).
    for job in ["cluster", "merge", "refine", "", "swept"] {
        refused!(
            &db,
            "CREATE maintenance_progress SET job = $job, version = 0;",
            "job",
            job.to_string()
        );
    }

    // Cursors are typed where the table is knowable: a workspace cursor naming a fact, an unknown
    // scope, no workspace, a project from the wrong shape, or a negative unit count is refused rather
    // than stored for a later pass to distrust.
    refused!(
        &db,
        "UPDATE ONLY $target SET cursors = [{workspace: $row, scope: 'personal'}];",
        "target",
        sweep.clone(),
        "row",
        fact(&db, "wrong table cursor").await
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET cursors = [{workspace: $w, scope: 'everywhere'}];",
        "target",
        sweep.clone(),
        "w",
        alice_workspace.clone()
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET cursors = [{scope: 'personal'}];",
        "target",
        sweep.clone()
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET cursors = [{workspace: $w, scope: 'project', project: $row}];",
        "target",
        sweep.clone(),
        "w",
        alice_workspace.clone(),
        "row",
        fact(&db, "wrong project cursor").await
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET cursors = [{workspace: $w, scope: 'personal', units_done: -1}];",
        "target",
        sweep.clone(),
        "w",
        alice_workspace.clone()
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET version = -1;",
        "target",
        sweep.clone()
    );

    // `row_cursor` is the deliberate exception: `option<record>` over *any* table, because the three
    // capped jobs do not page one table (sweep/appraise walk heat rows, collapse works on facts).
    // Accepting a `project` record as a row cursor pins that generic typing as a choice, so Task 12
    // narrows it knowingly per job instead of discovering a silent schema restriction. The write keeps
    // the pair of cursors and the resumed `units_done` intact, because the replay below asserts them.
    run!(
        &db,
        "UPDATE ONLY $target SET cursors = [\
           {workspace: $w, scope: 'personal', project: NONE, row_cursor: $any, units_done: 17, exhausted: false},\
           {workspace: $bob, scope: 'project', project: $project, units_done: 0}];",
        "target",
        sweep.clone(),
        "w",
        alice_workspace.clone(),
        "any",
        bob_project.clone(),
        "bob",
        bob_workspace.clone(),
        "project",
        bob_project.clone()
    );
    let generic = row(&db, &sweep).await;
    assert_eq!(
        generic["cursors"].as_array().unwrap().len(),
        2,
        "the row-cursor probe replaced the ledger instead of writing through it: {generic:?}"
    );
    assert_eq!(
        generic["cursors"][0]["row_cursor"], generic["cursors"][1]["project"],
        "a non-`fact` row cursor did not store as the record it named: {generic:?}"
    );
    assert_eq!(
        generic["cursors"][0]["units_done"],
        Json::Number(17.into()),
        "the row-cursor probe disturbed the resumed cursor: {generic:?}"
    );

    let before_all = run!(&db, "SELECT * FROM maintenance_progress ORDER BY job;",);

    // Replay the head file: the in-flight cursor must be byte-identical afterwards.
    let head_sql = schema::MIGRATIONS.last().unwrap().2.to_string();
    run!(&db, head_sql.as_str());
    let after_all = run!(&db, "SELECT * FROM maintenance_progress ORDER BY job;");
    assert_eq!(
        after_all, before_all,
        "replaying v009 reset or dropped persisted maintenance progress"
    );
    let swept = row(&db, &sweep).await;
    assert_eq!(swept["version"], Json::Number(4.into()));
    assert_eq!(swept["cursors"].as_array().unwrap().len(), 2);
    assert_eq!(
        swept["cursors"][0]["units_done"],
        Json::Number(17.into()),
        "a resumed sweep must start where the last pass stopped"
    );
    assert!(has_value(&swept, "last_workspace"));
    assert_eq!(count_rows(&db, "maintenance_progress").await, 3);
}

/// Existing tables keep their write shapes: a fact, a session, a raw import, a lineage edge, a
/// membership edge, a heat row and a reminder written exactly the way every current caller writes
/// them must still succeed after v009 — before and after the upgrade — and must not gain an owner.
/// Applying the ownership schema may not be the thing that breaks the store.
#[tokio::test]
async fn v009_preserves_legacy_write_shapes_without_inventing_ownership() {
    use alexandria_storage::models::schedule_kind;
    use alexandria_storage::repos::{MemoryRepo, NewReminder, ReminderRepo, SessionRepo};

    let legacy = migrated_to(8).await;
    let memories = MemoryRepo::new(legacy.inner());
    let sessions = SessionRepo::new(legacy.inner());
    let reminders = ReminderRepo::new(legacy.inner());

    let old_fact = memories
        .create_fact("legacy content", 0.5, &[1.0, 0.0], &["t".to_string()])
        .await
        .unwrap();
    let old_raw = memories.create_raw("legacy document").await.unwrap();
    let old_session = sessions
        .find_or_create("external-handle", Some("claude-code"), Some("model-x"))
        .await
        .unwrap();
    let old_reminder = reminders
        .create(&NewReminder {
            message: "legacy reminder".to_string(),
            target_project: Some("alexandria".to_string()),
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: schedule_kind::ONCE.to_string(),
            due_at: None,
            freq: None,
            time_of_day: None,
            weekdays: vec![],
            day_of_month: None,
            cron_expr: None,
            next_due_at: Some(chrono::Utc::now() + chrono::Duration::days(1)),
        })
        .await
        .unwrap();

    // The same writes again, *after* the upgrade: this is the regression the brief names.
    schema::migrate(legacy.inner()).await.unwrap();
    let new_fact = memories
        .create_fact("post-v009 content", 0.5, &[1.0, 0.0], &[])
        .await;
    assert!(
        new_fact.is_ok(),
        "v009 broke the ordinary store path: {new_fact:?}"
    );
    let new_raw = memories.create_raw("post-v009 document").await;
    assert!(
        new_raw.is_ok(),
        "v009 broke the raw import path: {new_raw:?}"
    );
    let new_session = sessions.find_or_create("second-handle", None, None).await;
    assert!(
        new_session.is_ok(),
        "v009 broke implicit session creation: {new_session:?}"
    );
    let new_reminder = reminders
        .create(&NewReminder {
            message: "post-v009 reminder".to_string(),
            target_project: None,
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: schedule_kind::CRON.to_string(),
            due_at: None,
            freq: None,
            time_of_day: None,
            weekdays: vec![],
            day_of_month: None,
            cron_expr: Some("0 9 * * MON".to_string()),
            next_due_at: Some(chrono::Utc::now() + chrono::Duration::days(1)),
        })
        .await;
    assert!(
        new_reminder.is_ok(),
        "v009 broke the reminder path: {new_reminder:?}"
    );

    // The graph, membership and heat shapes the store already writes, after the upgrade.
    let edge_target_id = typed_id(&legacy, &edge_target(&legacy).await).await;
    let new_fact_id = typed_id(&legacy, new_fact.as_ref().unwrap()).await;
    run!(
        &legacy,
        "RELATE $from->memory_edge->$to SET edge_type = 'derived_from', strength = 1.0;",
        "from",
        new_fact_id.clone(),
        "to",
        edge_target_id.clone()
    );
    let cluster = create_id!(
        &legacy,
        "CREATE cluster SET label = 'legacy cluster', centroid = [1.0, 0.0], depth = 0"
    );
    run!(
        &legacy,
        "RELATE $cluster->contains_memory->$target;",
        "cluster",
        cluster.clone(),
        "target",
        edge_target_id.clone()
    );
    run!(
        &legacy,
        "CREATE heat_state SET memory = $target, heat = 1.0, stability = 1.0;",
        "target",
        edge_target_id.clone()
    );
    let session_id = typed_id(&legacy, &old_session).await;
    let old_fact_id = typed_id(&legacy, &old_fact).await;
    run!(
        &legacy,
        "RELATE $session_row->contains_session_memory->$target;",
        "session_row",
        session_id,
        "target",
        old_fact_id
    );
    let raw_id = typed_id(&legacy, &old_raw).await;
    run!(
        &legacy,
        "RELATE $target->extracted_from->$raw;",
        "target",
        new_fact_id,
        "raw",
        raw_id
    );

    // None of it acquired an owner, a workspace or a scope by virtue of the schema existing.
    for (label, id_sql) in [
        ("fact", old_fact.as_str()),
        ("raw", old_raw.as_str()),
        ("reminder", old_reminder.as_str()),
        ("session", old_session.as_str()),
    ] {
        let id = typed_id(&legacy, id_sql).await;
        let stored = row(&legacy, &id).await;
        assert!(
            !has_value(&stored, "account") && !has_value(&stored, "owner"),
            "{label} gained an invented owner: {stored:?}"
        );
        assert!(
            !has_value(&stored, "workspace"),
            "{label} gained an invented workspace: {stored:?}"
        );
        assert!(
            !has_value(&stored, "scope"),
            "{label} gained an invented scope: {stored:?}"
        );
    }

    // The ordinary live read paths still see the rows they always saw.
    let live = memories
        .get_fact(&old_fact)
        .await
        .unwrap()
        .expect("legacy fact must still be readable");
    assert_eq!(live.content, "legacy content");
    let session = sessions
        .find_by_external_id("external-handle")
        .await
        .unwrap()
        .expect("legacy session must still resolve by its global handle until Task 6");
    assert_eq!(session.external_id, "external-handle");
    let reminder = reminders
        .get(&old_reminder)
        .await
        .unwrap()
        .expect("legacy reminder must still be readable");
    assert_eq!(reminder.target_project.as_deref(), Some("alexandria"));

    // Paired with the scoped refusal: none of these unowned rows is reachable through any workspace
    // partition, including a bootstrapped owner's own Personal partition.
    let alice = account(&legacy, "alice").await;
    let alice_workspace = workspace_of(&legacy, &alice).await;
    for partition in ["personal", "project", "unassigned"] {
        let found = run!(
            &legacy,
            "SELECT * FROM fact WHERE workspace = $w AND scope = $partition AND deleted = false;",
            "w",
            alice_workspace.clone(),
            "partition",
            partition.to_string()
        );
        assert!(
            found.is_empty(),
            "partition `{partition}` returned rows that have no owner at all: {found:?}"
        );
    }
    let owned_reminders = run!(
        &legacy,
        "SELECT * FROM reminder WHERE account = $account;",
        "account",
        alice.clone()
    );
    assert!(
        owned_reminders.is_empty(),
        "legacy reminders were given to alice without a mapping: {owned_reminders:?}"
    );
    let owned_sessions = run!(
        &legacy,
        "SELECT * FROM session WHERE account = $account;",
        "account",
        alice.clone()
    );
    assert!(
        owned_sessions.is_empty(),
        "legacy sessions were given to alice without a mapping: {owned_sessions:?}"
    );
    let owned_clusters = run!(
        &legacy,
        "SELECT * FROM cluster WHERE workspace = $w;",
        "w",
        alice_workspace.clone()
    );
    assert!(
        owned_clusters.is_empty(),
        "a legacy cluster was assigned to a workspace by the schema: {owned_clusters:?}"
    );
}

/// A fact created by the ordinary write path, used as an edge/membership target.
async fn edge_target(db: &Database) -> String {
    use alexandria_storage::repos::MemoryRepo;
    MemoryRepo::new(db.inner())
        .create_fact("edge target", 0.5, &[0.0, 1.0], &[])
        .await
        .unwrap()
}

/// Task 6's checkpoint: the session handle index is re-keyed *together with* the readers that qualify
/// by account. v009 deliberately left the global index alone, and said so in its own comment, because a
/// partial re-key lets two accounts' colliding handles both insert while every existing reader still
/// resolves globally — data loss, not a privacy improvement.
///
/// This gate is the one that test forbade, and it now asserts the swapped state: the global index is
/// gone, `(account, external_id)` is the unique key, and a colliding handle for two *real* accounts
/// inserts both rows instead of being refused. That last part is what makes the swap observable in the
/// schema rather than in a comment.
#[tokio::test]
async fn session_handle_index_is_account_local_after_task_six() {
    let db = migrated().await;
    let info = run!(&db, "INFO FOR TABLE session STRUCTURE;");
    let indexes = info[0]["indexes"].as_array().cloned().unwrap_or_default();
    assert!(
        !indexes
            .iter()
            .any(|index| index["name"] == Json::String("session_external_id_idx".into())),
        "v010 must remove the global handle index, not merely add a second one: {indexes:?}"
    );
    let local = indexes
        .iter()
        .find(|index| index["name"] == Json::String("session_account_external_id_idx".into()))
        .unwrap_or_else(|| {
            panic!("v010 did not define the account-local handle index: {indexes:?}")
        });
    assert_eq!(local["index"].as_str().unwrap(), "UNIQUE");
    assert_eq!(
        local["cols"].as_array().unwrap().as_slice(),
        &[
            Json::String("account".into()),
            Json::String("external_id".into())
        ],
        "the key is (account, external_id), in that order: {local:?}"
    );

    // Concretely: one handle under two accounts is now two rows, and one handle under one account is
    // still refused by the index rather than by an application check.
    run!(
        &db,
        "CREATE account:idx_a SET login = 'idx-a-login', password_hash = 'x';"
    );
    run!(
        &db,
        "CREATE account:idx_b SET login = 'idx-b-login', password_hash = 'x';"
    );
    run!(
        &db,
        "CREATE session SET external_id = 'shared-handle', account = account:idx_a, tags = [];"
    );
    run!(
        &db,
        "CREATE session SET external_id = 'shared-handle', account = account:idx_b, tags = [];"
    );
    let across = run!(
        &db,
        "SELECT count() FROM session WHERE external_id = 'shared-handle' GROUP ALL"
    );
    assert_eq!(
        across[0]["count"].as_i64().unwrap(),
        2,
        "a shared handle must be two sessions once the key is account-local"
    );
    let error = refused!(
        &db,
        "CREATE session SET external_id = 'shared-handle', account = account:idx_a, tags = [];"
    );
    assert!(
        error.contains("session_account_external_id_idx") || error.contains("already contains"),
        "the account-local uniqueness rule is not enforced: {error}"
    );

    // And the ownership column stays optional, so a migration-pending store keeps its rows: the schema
    // does not invent an owner for data the backfill (Task 14) has not claimed.
    run!(
        &db,
        "CREATE session SET external_id = 'unowned-handle', tags = [];"
    );
    let stored = run!(
        &db,
        "SELECT * FROM session WHERE external_id = 'unowned-handle' LIMIT 1"
    );
    assert!(
        !has_value(&stored[0], "account"),
        "the session account column must stay optional: {:?}",
        stored[0]
    );
}

/// Task 4's two browser-session columns — `credential.absolute_expires_at` and `credential.csrf_digest` —
/// are `option<…>` with no default, so the absence a row never filled stays readable as absence, the legacy
/// `UPDATE` shapes still land on it, and the only spellings that distinguish the two are the ones this
/// engine actually evaluates. The CSRF digest is length-checked like `secret_digest`, and neither half is
/// reachable through the digest index except the session one.
#[tokio::test]
async fn session_deadline_and_csrf_columns_are_migration_safe() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let browser = create_id!(
        &db,
        "CREATE credential SET account = $account, kind = 'browser';",
        "account",
        alice.clone()
    );
    let agent = create_id!(
        &db,
        "CREATE credential SET account = $account, kind = 'agent', secret_digest = $digest;",
        "account",
        alice.clone(),
        "digest",
        "Zm91ckJ5dGVTZWNyZXREaWdlc3RWYWx1ZUhlcmVGb3JBZ2VudFRva2Vu"
    );

    let fresh = row(&db, &browser).await;
    assert!(
        !has_value(&fresh, "absolute_expires_at"),
        "a session written without an absolute cap gained one anyway: {fresh}"
    );
    assert!(
        !has_value(&fresh, "csrf_digest"),
        "a session written without a CSRF half gained one anyway"
    );

    // Absent is `NONE`, and the two spellings that look like they ask are both satisfied by it: the engine
    // trap that makes `= NONE` the only correct question here.
    assert_eq!(
        count_where(
            &db,
            "SELECT count() AS total FROM credential WHERE absolute_expires_at = NONE GROUP ALL;"
        )
        .await,
        2,
        "`= NONE` did not match the rows that never carried a cap"
    );
    assert_eq!(
        count_where(
            &db,
            "SELECT count() AS total FROM credential WHERE absolute_expires_at IS NOT NULL GROUP ALL;"
        )
        .await,
        2,
        "`IS NOT NULL` filtered nothing here, exactly as it does everywhere else in this file"
    );

    // A legacy-shaped update of a row that predates the columns still lands, which is the whole
    // compatibility rule this migration is written against.
    run!(
        &db,
        "UPDATE ONLY $target SET revoked = true;",
        "target",
        browser.clone()
    );
    assert_eq!(row(&db, &browser).await["revoked"], Json::Bool(true));

    let csrf = "Zm91ckJ5dGVTZWNyZXREaWdlc3RWYWx1ZUhlcmVGb3JDbXNG";
    run!(
        &db,
        "UPDATE ONLY $target SET revoked = false, absolute_expires_at = d'2999-01-01T00:00:00Z', \
         csrf_digest = $csrf;",
        "target",
        browser.clone(),
        "csrf",
        csrf.to_string()
    );
    let paired = row(&db, &browser).await;
    assert!(has_value(&paired, "absolute_expires_at"), "{paired}");
    assert_eq!(paired["csrf_digest"], Json::String(csrf.to_string()));

    // The cap is a datetime and nothing else, and a truncated or hand-typed CSRF digest is refused at the
    // write rather than stored and silently never matching.
    for sets in [
        "csrf_digest = 'short'",
        "csrf_digest = ''",
        "absolute_expires_at = '2999-01-01'",
        "absolute_expires_at = 42",
    ] {
        refused!(
            &db,
            format!("UPDATE ONLY $target SET {sets};"),
            "target",
            browser.clone()
        );
    }
    // Writing `NONE` clears the column again instead of storing a null.
    run!(
        &db,
        "UPDATE ONLY $target SET absolute_expires_at = NONE;",
        "target",
        browser.clone()
    );
    assert!(!has_value(&row(&db, &browser).await, "absolute_expires_at"));

    // Only the session half is indexed for resolution, so a CSRF token cannot be looked up as a credential.
    // `INFO FOR TABLE … STRUCTURE` is the engine's own answer, so this cannot drift from the file.
    let info = run!(&db, "INFO FOR TABLE credential STRUCTURE;");
    let indexes = info[0]["indexes"].as_array().cloned().unwrap_or_default();
    let digest = indexes
        .iter()
        .find(|index| index["name"] == Json::String("idx_credential_digest".into()))
        .unwrap_or_else(|| panic!("the digest index is gone: {indexes:?}"));
    assert_eq!(
        digest["cols"].as_array().unwrap().as_slice(),
        &[Json::String("secret_digest".into())],
        "a credential resolves by `secret_digest` alone: {digest:?}"
    );
    assert!(
        !indexes.iter().any(|index| {
            index["cols"]
                .as_array()
                .map(|cols| cols.iter().any(|c| c.as_str() == Some("csrf_digest")))
                .unwrap_or(false)
        }),
        "a CSRF digest became resolvable as a credential: {indexes:?}"
    );
    // And the agent row the same fixture wrote is untouched by any of the session-shaped writes.
    assert!(!has_value(&row(&db, &agent).await, "csrf_digest"));
}

/// Cluster derived state is guarded by three revision columns — membership, member embedding and
/// centroid — defined here so every later writer increments a guard that already exists. Existing
/// clusters keep reading absent (the backfill is Task 14's), new clusters read zero, and a guard only
/// accepts a non-negative integer.
#[tokio::test]
async fn cluster_revision_guards_default_without_rewriting_members() {
    let legacy = migrated_to(8).await;
    let old_cluster = create_id!(
        &legacy,
        "CREATE cluster SET label = 'legacy', centroid = [1.0, 0.0], depth = 0"
    );
    let old_fact = fact(&legacy, "clustered").await;
    run!(
        &legacy,
        "RELATE $cluster->contains_memory->$target;",
        "cluster",
        old_cluster.clone(),
        "target",
        old_fact.clone()
    );

    schema::migrate(legacy.inner()).await.unwrap();

    let before = row(&legacy, &old_cluster).await;
    assert_eq!(before["label"].as_str().unwrap(), "legacy");
    for guard in [
        "membership_revision",
        "member_embedding_revision",
        "centroid_revision",
    ] {
        assert!(
            !has_value(&before, guard),
            "a pre-v009 cluster row was rewritten by the schema: {before:?}"
        );
    }
    assert!(
        !has_value(&before, "workspace"),
        "v009 assigned a legacy cluster to a workspace: {before:?}"
    );
    // The membership edge still exists and still traverses.
    let members = run!(&legacy, "SELECT * FROM contains_memory LIMIT 1",);
    assert_eq!(members.len(), 1, "v009 lost a membership edge");

    let fresh = create_id!(
        &legacy,
        "CREATE cluster SET label = 'fresh', centroid = [0.0, 1.0], depth = 1"
    );
    let fresh_row = row(&legacy, &fresh).await;
    for guard in [
        "membership_revision",
        "member_embedding_revision",
        "centroid_revision",
    ] {
        assert_eq!(
            fresh_row[guard],
            Json::Number(0.into()),
            "`{guard}` must exist and default to 0: {fresh_row:?}"
        );
    }

    run!(
        &legacy,
        "UPDATE ONLY $target SET membership_revision = 2;",
        "target",
        fresh.clone()
    );
    assert_eq!(
        row(&legacy, &fresh).await["membership_revision"],
        Json::Number(2.into())
    );

    for (guard, value) in [
        ("centroid_revision", "'dirty'"),
        ("membership_revision", "-1"),
        ("member_embedding_revision", "'2'"),
    ] {
        let sql = format!("UPDATE ONLY $target SET {guard} = {value};");
        refused!(&legacy, sql.as_str(), "target", fresh.clone());
    }

    // A pre-v009 cluster row must still accept the writes the dreaming loop actually performs, which
    // is the whole reason the guards are `option<…> DEFAULT …`: a bare `int DEFAULT 0` is refused on
    // every later `UPDATE` of a row whose guard has never existed ("Expected `int` but found `NONE`",
    // measured on 3.3.0), so v009 would have broken existing cluster writes instead of guarding them.
    // A fresh cluster cannot show this — its guards were never absent — so the writes below go to the
    // row that predates the file, and assert the write landed, the guard moved, and nothing about the
    // row's membership or its unowned state changed.
    run!(
        &legacy,
        "UPDATE ONLY $target SET centroid = [0.0, 1.0], centroid_revision = 3;",
        "target",
        old_cluster.clone()
    );
    let legacy_after = row(&legacy, &old_cluster).await;
    assert_eq!(
        legacy_after["centroid"],
        Json::from(vec![0.0, 1.0]),
        "a post-v009 centroid write did not land on a pre-v009 cluster row"
    );
    assert_eq!(
        legacy_after["centroid_revision"],
        Json::Number(3.into()),
        "a pre-v009 cluster row could not carry a guarded revision write"
    );
    // The guards this write did not name stay un-backfilled: v009 defaults a row from now on, it does
    // not rewrite rows that already exist.
    for guard in ["membership_revision", "member_embedding_revision"] {
        assert!(
            !has_value(&legacy_after, guard),
            "a legacy cluster write backfilled the untouched guard `{guard}`: {legacy_after:?}"
        );
    }
    // Still unowned, still the same cluster, and its membership edge still traverses.
    assert!(
        !has_value(&legacy_after, "workspace") && !has_value(&legacy_after, "account"),
        "a legacy cluster write invented an owner: {legacy_after:?}"
    );
    assert_eq!(legacy_after["label"], Json::String("legacy".into()));
    let members = run!(
        &legacy,
        "SELECT * FROM contains_memory WHERE in = $cluster;",
        "cluster",
        old_cluster.clone()
    );
    assert_eq!(
        members.len(),
        1,
        "a centroid write on the legacy cluster lost its membership edge"
    );
    // The guard's own ASSERT applies to the legacy row too, not only to rows created after v009.
    for (guard, value) in [
        ("centroid_revision", "'dirty'"),
        ("membership_revision", "-1"),
        ("member_embedding_revision", "'2'"),
    ] {
        let sql = format!("UPDATE ONLY $target SET {guard} = {value};");
        refused!(&legacy, sql.as_str(), "target", old_cluster.clone());
    }

    // Replaying the file must not reset a guard another writer already moved — on either row.
    let before_replay = row(&legacy, &fresh).await;
    let legacy_before_replay = legacy_after.clone();
    let head_sql = schema::MIGRATIONS.last().unwrap().2.to_string();
    run!(&legacy, head_sql.as_str());
    assert_eq!(
        row(&legacy, &fresh).await,
        before_replay,
        "replaying v009 reset a cluster guard mid-flight"
    );
    assert_eq!(
        row(&legacy, &old_cluster).await,
        legacy_before_replay,
        "replaying v009 reset a legacy cluster's moved guard"
    );
}

/// Background summaries link to the journal operation they describe, so a dreaming pass that made
/// semantic writes is traceable to its pre/postimages. The link is optional — rows written before
/// this file have no operation to name — and it is a link, not a new public or global memory log.
#[tokio::test]
async fn maintenance_log_links_to_its_operation_optionally() {
    let db = migrated().await;
    let log = create_id!(
        &db,
        "CREATE maintenance_log SET action = 'collapse', source_id = 'fact:one', \
         target_ids = ['fact:two'], members_moved = 1;"
    );
    let unlinked = row(&db, &log).await;
    assert!(
        !has_value(&unlinked, "operation"),
        "a summary row gained an invented operation: {unlinked:?}"
    );
    assert_eq!(unlinked["action"].as_str().unwrap(), "collapse");

    let alice = account(&db, "alice").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let op = operation(
        &db,
        &alice_workspace,
        &alice,
        "collapse-1",
        "fingerprint-collapse-value",
        &["fact:delete"],
    )
    .await;

    run!(
        &db,
        "UPDATE ONLY $target SET operation = $operation;",
        "target",
        log.clone(),
        "operation",
        op.clone()
    );
    let linked = row(&db, &log).await;
    assert_eq!(
        linked["operation"],
        Json::String(record_id_to_string(&op)),
        "the summary row must carry the journal link"
    );

    // A link to the wrong table is refused: this records *which* operation, not any record.
    let error = refused!(
        &db,
        "UPDATE ONLY $target SET operation = $account;",
        "target",
        log.clone(),
        "account",
        alice.clone()
    );
    assert!(
        error.contains("semantic_operation"),
        "the operation-link refusal should name the expected type: {error}"
    );

    // Replay keeps it.
    let before = row(&db, &log).await;
    let head_sql = schema::MIGRATIONS.last().unwrap().2.to_string();
    run!(&db, head_sql.as_str());
    assert_eq!(row(&db, &log).await, before);
}

/// New tables keep their own constraints even though the compatibility rule protects *existing* rows:
/// an account has a status from a closed set, a role from a closed set, and an authentication epoch
/// that only moves forward; a credential names its account, its kind and the epoch it was minted
/// under. The epoch ladder is what recovery rides on, so an obsolete credential is distinguishable
/// from a revoked one.
#[tokio::test]
async fn account_and_credential_state_is_constrained() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;

    let fresh = row(&db, &alice).await;
    assert_eq!(fresh["status"].as_str().unwrap(), "active");
    assert_eq!(fresh["auth_epoch"], Json::Number(1.into()));
    assert_eq!(fresh["role"].as_str().unwrap(), "owner");

    for status in ["active", "suspended"] {
        run!(
            &db,
            "UPDATE ONLY $target SET status = $status;",
            "target",
            alice.clone(),
            "status",
            status.to_string()
        );
    }
    for status in ["deleted", "ACTIVE", "", "pending", "none"] {
        refused!(
            &db,
            "UPDATE ONLY $target SET status = $status;",
            "target",
            alice.clone(),
            "status",
            status.to_string()
        );
    }
    for role in ["owner", "admin"] {
        run!(
            &db,
            "UPDATE ONLY $target SET role = $role;",
            "target",
            alice.clone(),
            "role",
            role.to_string()
        );
    }
    for role in ["superuser", "", "Owner"] {
        refused!(
            &db,
            "UPDATE ONLY $target SET role = $role;",
            "target",
            alice.clone(),
            "role",
            role.to_string()
        );
    }
    for epoch in [0, -1] {
        refused!(
            &db,
            "UPDATE ONLY $target SET auth_epoch = $epoch;",
            "target",
            alice.clone(),
            "epoch",
            epoch
        );
    }
    run!(
        &db,
        "UPDATE ONLY $target SET auth_epoch = 7;",
        "target",
        alice.clone()
    );
    assert_eq!(
        row(&db, &alice).await["auth_epoch"],
        Json::Number(7.into()),
        "recovery moves the epoch forward and that is the whole invalidation"
    );

    refused!(
        &db,
        "CREATE account SET login = 'alice', password_hash = 'x';"
    );

    let credential = create_id!(
        &db,
        "CREATE credential SET account = $account, kind = 'agent', label = 'work laptop', \
         secret_digest = $digest, auth_epoch = 7;",
        "account",
        alice.clone(),
        "digest",
        "agent-token-digest-fixture-value-0000000000"
    );
    let credential_row = row(&db, &credential).await;
    assert_eq!(credential_row["revoked"], Json::Bool(false));
    assert_eq!(credential_row["consumed"], Json::Bool(false));
    assert_eq!(credential_row["auth_epoch"], Json::Number(7.into()));
    assert!(!has_value(&credential_row, "revoked_at"));
    assert_eq!(
        credential_row["account"],
        Json::String(record_id_to_string(&alice)),
        "a credential must name the account it belongs to"
    );

    for kind in ["browser", "invite", "recovery"] {
        run!(
            &db,
            "CREATE credential SET account = $account, kind = $kind;",
            "account",
            alice.clone(),
            "kind",
            kind.to_string()
        );
    }
    for kind in ["oauth", "agent ", "", "Agent"] {
        refused!(
            &db,
            "CREATE credential SET account = $account, kind = $kind;",
            "account",
            alice.clone(),
            "kind",
            kind.to_string()
        );
    }
    refused!(&db, "CREATE credential SET kind = 'agent';");
    refused!(
        &db,
        "CREATE credential SET account = $account, kind = 'agent', secret_digest = 'abc';",
        "account",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE credential SET account = $account, kind = 'agent', label = '   ';",
        "account",
        alice.clone()
    );

    run!(
        &db,
        "UPDATE ONLY $target SET revoked = true, revoked_at = time::now();",
        "target",
        credential.clone()
    );
    let revoked_row = row(&db, &credential).await;
    assert_eq!(revoked_row["revoked"], Json::Bool(true));
    assert!(has_value(&revoked_row, "revoked_at"));
    // A credential cannot drop its own epoch to look current again.
    refused!(
        &db,
        "UPDATE ONLY $target SET auth_epoch = 0;",
        "target",
        credential.clone()
    );
    // An invitation is not an account, and an account row cannot be reached through one.
    refused!(
        &db,
        "UPDATE ONLY $target SET account = $fact;",
        "target",
        credential.clone(),
        "fact",
        fact(&db, "not an account").await
    );
}

/// Proposals and projects are private to one workspace and typed within it: the destination is one of
/// the three scopes, a `project` destination names a project handle and no other destination may, the
/// state ladder is closed, and the same record id read under another workspace resolves to nothing.
#[tokio::test]
async fn proposals_are_workspace_private_and_typed() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    let alice_workspace = workspace_of(&db, &alice).await;
    let bob_workspace = workspace_of(&db, &bob).await;
    let bob_project = project(&db, &bob_workspace, "alexandria").await;
    let alice_fact = fact(&db, "classify me").await;

    let proposal = create_id!(
        &db,
        "CREATE classification_proposal SET workspace = $workspace, account = $account, \
         fact = $fact, destination_kind = 'personal', source_revision = 5, \
         reason = 'session context';",
        "workspace",
        alice_workspace.clone(),
        "account",
        alice.clone(),
        "fact",
        alice_fact.clone()
    );
    let stored = row(&db, &proposal).await;
    assert_eq!(stored["state"].as_str().unwrap(), "pending");
    assert_eq!(stored["revision"], Json::Number(0.into()));
    assert_eq!(stored["source_revision"], Json::Number(5.into()));

    // Another workspace cannot resolve the same record id; its own read is legitimately empty.
    let leaked = run!(
        &db,
        "SELECT * FROM classification_proposal WHERE workspace = $workspace;",
        "workspace",
        bob_workspace.clone()
    );
    assert!(
        leaked.is_empty(),
        "a proposal leaked across workspaces: {leaked:?}"
    );
    let own = run!(
        &db,
        "SELECT * FROM classification_proposal WHERE workspace = $workspace;",
        "workspace",
        alice_workspace.clone()
    );
    assert_eq!(own.len(), 1, "the owning workspace lost its own proposal");

    for destination in ["personal", "unassigned"] {
        run!(
            &db,
            "UPDATE ONLY $target SET destination_kind = $destination, destination_project = NONE;",
            "target",
            proposal.clone(),
            "destination",
            destination.to_string()
        );
    }
    for destination in ["Personal", "shared", "", "public"] {
        refused!(
            &db,
            "UPDATE ONLY $target SET destination_kind = $destination;",
            "target",
            proposal.clone(),
            "destination",
            destination.to_string()
        );
    }

    // A `project` destination has to name a project, and no other destination may carry one. Which
    // workspace the named project belongs to is a read, not a type check, and is refused at the
    // resolver — `foreign_project_selector_is_not_found` in `access.rs`.
    run!(
        &db,
        "UPDATE ONLY $target SET destination_kind = 'project', destination_project = $project;",
        "target",
        proposal.clone(),
        "project",
        bob_project.clone()
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET destination_kind = 'project', destination_project = NONE;",
        "target",
        proposal.clone()
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET destination_kind = 'personal', destination_project = $project;",
        "target",
        proposal.clone(),
        "project",
        bob_project.clone()
    );

    for state in ["pending", "approved", "dismissed", "stale"] {
        run!(
            &db,
            "UPDATE ONLY $target SET state = $state;",
            "target",
            proposal.clone(),
            "state",
            state.to_string()
        );
    }
    refused!(
        &db,
        "UPDATE ONLY $target SET state = 'auto-applied';",
        "target",
        proposal.clone()
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET revision = -1;",
        "target",
        proposal.clone()
    );

    // A proposal needs the fact, the workspace and the account: nothing here may be orphaned or
    // resolved by a guessed owner.
    refused!(
        &db,
        "CREATE classification_proposal SET workspace = $workspace, account = $account, \
         destination_kind = 'personal';",
        "workspace",
        alice_workspace.clone(),
        "account",
        alice.clone()
    );
    refused!(
        &db,
        "CREATE classification_proposal SET workspace = $workspace, destination_kind = 'personal', \
         fact = $fact;",
        "workspace",
        alice_workspace.clone(),
        "fact",
        alice_fact.clone()
    );

    let visible_to_bob = run!(
        &db,
        "SELECT * FROM project WHERE workspace = $workspace;",
        "workspace",
        bob_workspace.clone()
    );
    assert_eq!(visible_to_bob.len(), 1);
    let visible_to_alice = run!(
        &db,
        "SELECT * FROM project WHERE workspace = $workspace;",
        "workspace",
        alice_workspace.clone()
    );
    assert!(
        visible_to_alice.is_empty(),
        "alice saw bob's project: {visible_to_alice:?}"
    );
}

/// Ownership-migration state exists before Task 14 writes it: a closed ladder, an explicitly
/// nominated owner (never inferred), a receipt that is a value rather than a flag, and cursors typed
/// to the tables they page. Unknown or incomplete state is what a later boot reads and refuses to
/// serve on, so the states that could be confused with "ready" must not be accepted loosely.
#[tokio::test]
async fn ownership_migration_state_is_explicit_and_closed() {
    let db = migrated().await;
    let alice = account(&db, "alice").await;
    let alice_workspace = workspace_of(&db, &alice).await;

    let started = create_id!(
        &db,
        "CREATE ownership_migration SET nominated_owner = $owner, nominated_workspace = \
         $workspace, backup_receipt = 'receipt-fixture-value';",
        "owner",
        alice.clone(),
        "workspace",
        alice_workspace.clone()
    );
    let fresh = row(&db, &started).await;
    assert_eq!(
        fresh["state"].as_str().unwrap(),
        "pending",
        "a migration starts pending, not running: {fresh:?}"
    );
    assert!(!has_value(&fresh, "fact_cursor"));
    assert_eq!(fresh["rows_migrated"], Json::Number(0.into()));

    for state in ["running", "paused", "verifying", "complete", "failed"] {
        run!(
            &db,
            "UPDATE ONLY $target SET state = $state;",
            "target",
            started.clone(),
            "state",
            state.to_string()
        );
    }
    for state in ["done", "PENDING", "", "ready"] {
        refused!(
            &db,
            "UPDATE ONLY $target SET state = $state;",
            "target",
            started.clone(),
            "state",
            state.to_string()
        );
    }

    // Cursors are typed records: a resumed run cannot point at another table's row and then trust
    // what it reads.
    let wrong_fact_cursor = refused!(
        &db,
        "UPDATE ONLY $target SET fact_cursor = $workspace;",
        "target",
        started.clone(),
        "workspace",
        alice_workspace.clone()
    );
    assert!(
        wrong_fact_cursor.contains("fact"),
        "a fact cursor should say which table it wanted: {wrong_fact_cursor}"
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET session_cursor = $fact;",
        "target",
        started.clone(),
        "fact",
        fact(&db, "wrong session cursor").await
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET reminder_cursor = $fact;",
        "target",
        started.clone(),
        "fact",
        fact(&db, "wrong reminder cursor").await
    );
    run!(
        &db,
        "UPDATE ONLY $target SET fact_cursor = $fact, rows_migrated = 12;",
        "target",
        started.clone(),
        "fact",
        fact(&db, "cursor target").await
    );
    refused!(
        &db,
        "UPDATE ONLY $target SET rows_migrated = -1;",
        "target",
        started.clone()
    );

    // A blank receipt is not a backup.
    refused!(
        &db,
        "UPDATE ONLY $target SET backup_receipt = '   ';",
        "target",
        started.clone()
    );

    // Nothing is nominated by default: the row Task 14 has not decided about yet has no owner, which
    // is what makes "the first account to sign in owns the corpus" impossible.
    let undecided = create_id!(&db, "CREATE ownership_migration;");
    let row = row(&db, &undecided).await;
    assert!(
        !has_value(&row, "nominated_owner"),
        "a migration invented an owner: {row:?}"
    );
    assert!(!has_value(&row, "nominated_workspace"));
    assert_eq!(row["state"].as_str().unwrap(), "pending");

    // A store that never ran a migration reads *no* row at all, which is why a reader must fail
    // closed rather than treat "absent" as "complete".
    assert_eq!(
        count_where(
            &db,
            "SELECT count() AS total FROM ownership_migration WHERE state = 'pending' GROUP ALL"
        )
        .await,
        1,
        "only the never-decided row is still pending; the walked one is `failed`"
    );
    assert_eq!(
        count_where(
            &db,
            "SELECT count() AS total FROM ownership_migration WHERE state = 'complete' GROUP ALL"
        )
        .await,
        0
    );
}

/// Enrollment is an operator command that may have to be run again — the terminal scrolled, the
/// receipt was lost. Re-running the same `OperationId` with the same request must return the same
/// owner and create nothing; re-running it with a *different* request must not be answered with
/// somebody's earlier success. Both halves are pinned here, and the second is the reason the account
/// row carries a fingerprint of the request it was created by.
#[tokio::test]
async fn bootstrap_retry_creates_one_owner() {
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let identities = IdentityRepo::new(db.inner());
    let operation = OperationId::new("bootstrap-1");

    let first = identities
        .bootstrap_owner(" Alice ", "$argon2id$fixture-hash", &operation)
        .await
        .expect("the first enrollment should succeed");
    let retry = identities
        .bootstrap_owner("alice", "$argon2id$fixture-hash", &operation)
        .await
        .expect("retrying the same operation with the same request should succeed");

    assert_eq!(
        (first.account.clone(), first.workspace.clone()),
        (retry.account, retry.workspace),
        "the retry returned a different owner for the same operation"
    );
    assert_eq!(count_rows(&db, "account").await, 1);
    assert_eq!(count_rows(&db, "workspace").await, 1);

    // A different operation cannot re-enroll a taken login: silently returning the existing owner
    // would tell a caller "yes, your owner exists" to a command that meant to create one.
    let conflict = identities
        .bootstrap_owner(
            "alice",
            "$argon2id$fixture-hash",
            &OperationId::new("bootstrap-2"),
        )
        .await;
    assert!(
        matches!(
            conflict,
            Err(alexandria_storage::error::StorageError::Stale)
        ),
        "a second operation re-enrolled an existing login: {conflict:?}"
    );
    assert_eq!(count_rows(&db, "account").await, 1);

    // The login the account row carries is the normalised one the schema's own assertion demands, so
    // Task 4's password lookup can find it, and the retry anchor is stored with it.
    let stored = run!(&db, "SELECT * FROM account WHERE login = 'alice' LIMIT 1");
    assert_eq!(stored.len(), 1, "' Alice ' was not normalised on write");
    assert_eq!(
        stored[0]["bootstrap_operation"].as_str().unwrap(),
        "bootstrap-1",
        "the retry anchor is what makes this idempotent, so it has to be stored"
    );
    let fingerprint = stored[0]["bootstrap_fingerprint"].as_str().unwrap_or("");
    assert!(
        fingerprint.len() == 64
            && fingerprint
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the recorded request must be a SHA-256 hex digest, got {fingerprint:?}"
    );
    assert!(
        !stored[0]["password_hash"]
            .as_str()
            .unwrap()
            .contains(fingerprint),
        "the fingerprint must not be a re-encoding of the stored hash"
    );

    // A blank login or a blank hash is refused before a write, and creates nothing.
    assert!(
        identities
            .bootstrap_owner("   ", "$argon2id$fixture-hash", &OperationId::new("b3"))
            .await
            .is_err()
    );
    assert!(
        identities
            .bootstrap_owner("carol", "   ", &OperationId::new("b4"))
            .await
            .is_err()
    );
    assert_eq!(
        count_rows(&db, "account").await,
        1,
        "a rejected enrollment still created a row"
    );
}

/// The same key, a different request: refused, and the recorded enrollment untouched.
///
/// This is the `semantic_operation` rule applied to the one Task-3 writer that has an operation key.
/// The comparison cannot be against the account's *current* `password_hash`, because Task 4 rotates
/// that (recovery) and a legitimate rotation must not redefine the original request — so what is
/// compared is the fingerprint of the exact prepared input the original request carried. Note what
/// this does *not* claim: hashing a salted Argon2id digest does not make two independently prepared
/// hashes of the same password equal. Retrying an operation reuses that operation's prepared hash; a
/// new preparation is a new request.
#[tokio::test]
async fn bootstrap_retry_refuses_a_changed_request_under_the_same_key() {
    use alexandria_storage::error::StorageError;
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let identities = IdentityRepo::new(db.inner());
    let operation = OperationId::new("bootstrap-1");
    let first = identities
        .bootstrap_owner("alice", "$argon2id$first-prepared-hash", &operation)
        .await
        .unwrap();
    let before_account = row(&db, first.account.as_record_id()).await;

    let changed = identities
        .bootstrap_owner("alice", "$argon2id$SECOND-prepared-hash", &operation)
        .await;
    match changed {
        Err(StorageError::InvalidInput(message)) => assert!(
            message.contains("fingerprint") || message.contains("request"),
            "the refusal should say which part of the request differed: {message}"
        ),
        other => panic!("a changed request under a used key was accepted: {other:?}"),
    }

    // Nothing about the recorded enrollment moved: same key, same fingerprint, same stored hash.
    let after_account = row(&db, first.account.as_record_id()).await;
    assert_eq!(
        after_account, before_account,
        "the refused retry changed the account row"
    );
    assert_eq!(count_rows(&db, "account").await, 1);
    assert_eq!(count_rows(&db, "workspace").await, 1);

    // The original request still retries successfully afterwards — the refusal was not a lock-out.
    let same = identities
        .bootstrap_owner("alice", "$argon2id$first-prepared-hash", &operation)
        .await
        .expect("the original request must remain retryable");
    assert_eq!(same.account, first.account);
    assert_eq!(same.workspace, first.workspace);
}

/// A legitimate later password rotation must not redefine the original request.
#[tokio::test]
async fn rotation_does_not_rewrite_the_recorded_request() {
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let identities = IdentityRepo::new(db.inner());
    let operation = OperationId::new("bootstrap-1");
    let first = identities
        .bootstrap_owner("alice", "$argon2id$original-prepared-hash", &operation)
        .await
        .unwrap();
    let before = row(&db, first.account.as_record_id()).await;

    // What Task 4's recovery write looks like: the current hash changes, the auth epoch moves.
    run!(
        &db,
        "UPDATE ONLY $account SET password_hash = $rotated, auth_epoch = 2;",
        "account",
        first.account.as_record_id().clone(),
        "rotated",
        "$argon2id$rotated-prepared-hash".to_string()
    );

    let after_rotation = row(&db, first.account.as_record_id()).await;
    assert_eq!(
        after_rotation["bootstrap_fingerprint"], before["bootstrap_fingerprint"],
        "rotating the password rewrote the identity of the original request"
    );
    assert_eq!(
        after_rotation["bootstrap_operation"], before["bootstrap_operation"],
        "rotating the password rewrote the operation that owns this enrollment"
    );
    assert_ne!(
        after_rotation["password_hash"], before["password_hash"],
        "the rotation fixture did not actually change the current hash"
    );

    // Retrying the ORIGINAL request still returns the original pair, while the rotated hash under
    // the same key is a different request and is refused.
    let original = identities
        .bootstrap_owner("alice", "$argon2id$original-prepared-hash", &operation)
        .await
        .expect("the original request must stay retryable across a rotation");
    assert_eq!(original.account, first.account);
    assert_eq!(original.workspace, first.workspace);
    assert!(
        identities
            .bootstrap_owner("alice", "$argon2id$rotated-prepared-hash", &operation)
            .await
            .is_err(),
        "the rotated hash is a different request and must not be answered with the old success"
    );
}

/// Enrollment history written before this column existed is unknown, and unknown is not permission
/// to adopt whatever the caller now presents.
#[tokio::test]
async fn enrollment_history_without_a_fingerprint_is_refused_not_adopted() {
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let account = account(&db, "alice").await;
    workspace_of(&db, &account).await;
    // The shape a row would have if the anchor were written by an older build or by hand: an
    // enrollment key with no recorded request, but a real workspace, so the only thing the retry
    // cannot recover is the history it would have to match.
    run!(
        &db,
        "UPDATE ONLY $account SET bootstrap_operation = 'bootstrap-1';",
        "account",
        account.clone()
    );

    let identities = IdentityRepo::new(db.inner());
    let attempt = identities
        .bootstrap_owner(
            "alice",
            "$argon2id$fixture-hash",
            &OperationId::new("bootstrap-1"),
        )
        .await;
    assert!(
        attempt.is_err(),
        "an unverifiable enrollment history was adopted as the caller's own request: {attempt:?}"
    );
    let stored = row(&db, &account).await;
    assert_eq!(
        stored["bootstrap_operation"].as_str().unwrap(),
        "bootstrap-1",
        "the refusal must not rewrite the recorded anchor"
    );
    assert!(
        stored["bootstrap_fingerprint"].as_str().is_none()
            || stored["bootstrap_fingerprint"].is_null(),
        "a refused retry back-filled the fingerprint of a request it never honored: {stored:?}"
    );
    assert_eq!(count_rows(&db, "account").await, 1);
    assert_eq!(count_rows(&db, "workspace").await, 1);
}

/// Two operators (or one operator twice, concurrently) presenting different requests under one key:
/// exactly one enrollment may exist, and the loser must not be told it succeeded. This is the
/// recovery read after a lost unique-index race, which is the path that could otherwise turn a
/// changed-request error into a misleading success.
#[tokio::test]
async fn concurrent_different_requests_under_one_key_yield_one_owner_and_one_refusal() {
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let operation = OperationId::new("bootstrap-race");
    let identities = IdentityRepo::new(db.inner());
    let first_request =
        identities.bootstrap_owner("alice", "$argon2id$request-one-prepared", &operation);
    let second_request =
        identities.bootstrap_owner("alice", "$argon2id$request-two-prepared", &operation);
    let (first, second) = tokio::join!(first_request, second_request);

    let succeeded = usize::from(first.is_ok()) + usize::from(second.is_ok());
    assert_eq!(
        succeeded, 1,
        "exactly one of two different requests under one key may succeed: first={first:?} second={second:?}"
    );
    assert!(
        count_rows(&db, "account").await == 1,
        "two accounts were created for one key"
    );
    assert!(
        count_rows(&db, "workspace").await == 1,
        "two workspaces were created for one key"
    );
}

/// The workspace is created in the same transaction as the account, so a *missing* workspace under a
/// used key is not a half-written enrollment to paper over — it is a later deletion or corruption.
/// The retry therefore fails closed and changes nothing, rather than minting a replacement id that
/// would contradict the receipt the first run handed back.
#[tokio::test]
async fn bootstrap_retry_refuses_a_workspace_that_no_longer_exists() {
    use alexandria_storage::error::StorageError;
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let identities = IdentityRepo::new(db.inner());
    let operation = OperationId::new("bootstrap-1");
    let first = identities
        .bootstrap_owner("alice", "$argon2id$fixture-hash", &operation)
        .await
        .unwrap();

    let before = row(&db, first.account.as_record_id()).await;
    run!(
        &db,
        "DELETE workspace WHERE owner = $account;",
        "account",
        first.account.as_record_id().clone()
    );
    assert_eq!(
        count_rows(&db, "workspace").await,
        0,
        "the fixture did not actually delete the owner's workspace"
    );

    let retry = identities
        .bootstrap_owner("alice", "$argon2id$fixture-hash", &operation)
        .await;
    assert!(
        matches!(retry, Err(StorageError::Internal(_))),
        "a missing workspace was healed instead of refused: {retry:?}"
    );

    // Fail closed means no repair either: the account row is untouched and no workspace reappeared.
    assert_eq!(
        row(&db, first.account.as_record_id()).await,
        before,
        "the refused retry changed the account row"
    );
    assert_eq!(count_rows(&db, "workspace").await, 0);
    assert_eq!(count_rows(&db, "account").await, 1);
}

/// A second owner's data is unreachable from the first, and the resolver refuses to widen the
/// population to reach it: the enrollment path creates two independent private workspaces even when
/// both accounts are bootstrapped by the same operator command.
#[tokio::test]
async fn bootstrap_two_owners_keeps_their_workspaces_private() {
    use alexandria_storage::models::identity::OperationId;
    use alexandria_storage::repos::IdentityRepo;

    let db = migrated().await;
    let identities = IdentityRepo::new(db.inner());
    let alice = identities
        .bootstrap_owner(
            "alice",
            "$argon2id$fixture-hash",
            &OperationId::new("b-alice"),
        )
        .await
        .unwrap();
    let bob = identities
        .bootstrap_owner("bob", "$argon2id$fixture-hash", &OperationId::new("b-bob"))
        .await
        .unwrap();
    assert_ne!(alice.account, bob.account);
    assert_ne!(alice.workspace, bob.workspace);

    // The colliding alias is the point: two projects, one per workspace, and each qualified lookup
    // resolves exactly one of them.
    let alice_project = project(&db, alice.workspace.as_record_id(), "alexandria").await;
    let bob_project = project(&db, bob.workspace.as_record_id(), "alexandria").await;
    for (owner, expected, other) in [
        (&alice, alice_project.clone(), bob_project.clone()),
        (&bob, bob_project.clone(), alice_project.clone()),
    ] {
        let label = if owner == &alice { "alice" } else { "bob" };
        let resolved = run!(
            &db,
            "SELECT * FROM project WHERE workspace = $w AND normalized_alias = 'alexandria';",
            "w",
            owner.workspace.as_record_id().clone()
        );
        assert_eq!(
            resolved.len(),
            1,
            "{label}'s qualified lookup did not resolve exactly one project"
        );
        assert_eq!(
            resolved[0]["id"],
            Json::String(record_id_to_string(&expected)),
            "{label}'s lookup resolved the other owner's project"
        );
        let foreign = run!(
            &db,
            "SELECT * FROM project WHERE id = $project AND workspace = $w;",
            "project",
            other,
            "w",
            owner.workspace.as_record_id().clone()
        );
        assert!(
            foreign.is_empty(),
            "{label} resolved a project that lives in another workspace: {foreign:?}"
        );
    }
}
