use alexandria_storage::embedding_state::INCOMPLETE_MODEL;
use alexandria_storage::ownership_migration::check_ownership_ready;
use alexandria_storage::{Database, schema, system_config};

#[tokio::test]
async fn interrupted_ownership_phases_cannot_serve() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    db.inner()
        .query("CREATE fact:legacy SET content = 'legacy', embedding = [1.0, 0.0]")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        check_ownership_ready(db.inner()).await.is_err(),
        "legacy corpus without an ownership journal must not serve"
    );

    for phase in ["pending", "running", "paused", "verifying", "failed"] {
        db.inner()
            .query("UPSERT ownership_migration:legacy SET state = $phase")
            .bind(("phase", phase))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(
            check_ownership_ready(db.inner()).await.is_err(),
            "ownership phase {phase} must not serve"
        );
    }
}

#[tokio::test]
async fn interrupted_embedding_migration_blocks_otherwise_ready_store() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    assert!(
        check_ownership_ready(db.inner()).await.is_ok(),
        "an empty initialized store must be eligible for fresh bootstrap"
    );
    system_config::set_config(db.inner(), "embedding_model", INCOMPLETE_MODEL)
        .await
        .unwrap();
    assert!(
        check_ownership_ready(db.inner()).await.is_err(),
        "ownership admission must also refuse the incomplete embedding sentinel"
    );
}

// The existing offline-copy lock guard is supported on Linux only. These tests
// use real, independently copied on-disk backups, not invented digest strings.
#[cfg(target_os = "linux")]
mod persistent {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use alexandria_storage::access::{AccessRepo, SystemContext};
    use alexandria_storage::models::{OperationId, PrincipalIds, ReadSelection};
    use alexandria_storage::offline_copy::{OfflineStoreGuard, source_manifest_path};
    use alexandria_storage::ownership_migration::{BackupReceipt, MigrationId, OwnershipMigration};
    use alexandria_storage::parse_record_id;
    use alexandria_storage::repos::{IdentityRepo, RetrievalStrategy, ScopedMemoryRepo};
    use surrealdb::types::Value;

    use super::*;

    const FACTS: &str = "SELECT id, content, embedding, confidence, tags, created_at, deleted, \
        quarantined_at FROM fact ORDER BY id";
    const SCHEDULES: &str = "SELECT id, message, schedule_kind, due_at, freq, time_of_day, weekdays, \
        day_of_month, cron_expr, next_due_at, status, created_at, cancelled_at, last_delivered_at, \
        delivered_count FROM reminder ORDER BY id";
    const EMBEDDING_KEYS: &str = "SELECT * FROM system_config WHERE key IN ['embedding_model', \
        'embedding_dimensions', 'embedding_max_tokens', 'embedding_space', \
        'embedding_migration_journal', 'embedding_migration_baseline'] ORDER BY key";
    const CHECKPOINTS: &str = "SELECT id, state, nominated_owner, nominated_workspace, backup_receipt, \
        fact_cursor, session_cursor, reminder_cursor, rows_migrated FROM ownership_migration ORDER BY id";
    const HISTORY: &[&str] = &[
        "SELECT id, content, created_at, deleted FROM raw ORDER BY id",
        "SELECT id, external_id, summary, tags, started_at, ended_at FROM `session` ORDER BY id",
        "SELECT * FROM heat_state ORDER BY id",
        "SELECT * FROM extracted_from ORDER BY id",
        "SELECT * FROM contains_session_memory ORDER BY id",
        "SELECT * FROM maintenance_log ORDER BY id",
    ];

    async fn rows(db: &Database, sql: &str) -> Vec<Value> {
        db.inner()
            .query(sql)
            .await
            .unwrap_or_else(|err| panic!("query {sql:?} failed: {err:#}"))
            .check()
            .unwrap_or_else(|err| panic!("statement {sql:?} failed: {err:#}"))
            .take(0)
            .unwrap()
    }

    async fn seed_legacy(db: &Database) {
        for (_, _, sql) in schema::MIGRATIONS.iter().take_while(|(v, _, _)| *v <= 8) {
            db.inner().query(*sql).await.unwrap().check().unwrap();
        }
        system_config::set_config(db.inner(), "schema_version", "8")
            .await
            .unwrap();
        system_config::check_embedding_model(db.inner(), "legacy-test-model", 2, 128)
            .await
            .unwrap();
        db.inner()
            .query(
                "CREATE $live SET content = 'legacy text', embedding = [1.0, 0.0],
                    confidence = 0.8, tags = ['archived'];
                 CREATE $deleted SET content = 'historical text', embedding = [0.0, 1.0],
                    deleted = true;
                 CREATE $raw SET content = 'original raw log';
                 CREATE $sess SET external_id = 'shared-session', agent_id = 'legacy-agent',
                    summary = 'private archive', tags = ['archive'],
                    ended_at = d'2025-01-01T00:00:00Z';
                 CREATE $linked SET kind = 'user', session_id = 'shared-session',
                    agent_id = 'legacy-agent';
                 CREATE provenance:unlinked SET kind = 'user', session_id = 'shared-session',
                    agent_id = 'legacy-agent';
                 CREATE $cluster SET label = 'pre-scope centroid', centroid = [1.0, 0.0];
                 CREATE heat_state:legacy SET memory = $live, heat = 0.5, stability = 2.0,
                    access_count = 3, last_touched = d'2025-01-03T09:00:00Z',
                    last_accessed_at = d'2025-01-02T09:00:00Z';
                 CREATE maintenance_log:legacy SET action = 'merge', source_id = 'cluster:old',
                    target_ids = ['cluster:legacy'];
                 CREATE reminder:weekly SET message = 'recurring legacy reminder',
                    target_project = ' Unmapped Legacy Project ', schedule_kind = 'pattern',
                    freq = 'weekly', time_of_day = '09:00', weekdays = ['mon'],
                    status = 'pending', next_due_at = d'2025-01-13T09:00:00Z',
                    last_delivered_at = d'2025-01-06T09:00:00Z', delivered_count = 7;
                 CREATE reminder:spent SET message = 'previously delivered reminder',
                    schedule_kind = 'once', status = 'delivered', delivered_count = 1,
                    due_at = d'2025-01-02T09:00:00Z', last_delivered_at = d'2025-01-02T09:00:00Z';
                 RELATE $sess->contains_session_memory->$live;
                 RELATE $live->extracted_from->$raw;
                 RELATE $live->has_provenance->$linked;
                 RELATE $cluster->contains_memory->$live;
                 RELATE $cluster->contains_memory->$deleted;",
            )
            .bind(("live", parse_record_id("fact:7").unwrap()))
            .bind(("deleted", parse_record_id("fact:⟨old-version⟩").unwrap()))
            .bind(("raw", parse_record_id("raw:legacy").unwrap()))
            .bind(("sess", parse_record_id("session:legacy").unwrap()))
            .bind(("linked", parse_record_id("provenance:linked").unwrap()))
            .bind(("cluster", parse_record_id("cluster:legacy").unwrap()))
            .await
            .expect("seed v008 legacy snapshot")
            .check()
            .expect("legacy schema accepts every fixture row and relation");
    }

    async fn offline_lock(path: &Path) -> OfflineStoreGuard {
        for _ in 0..100 {
            match OfflineStoreGuard::acquire(path) {
                Ok(guard) => return guard,
                Err(err) if err.to_string().contains("Source store is locked") => {
                    // Dropping the SDK handle releases SurrealKV asynchronously.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(err) => panic!("cannot inspect the existing offline LOCK: {err:#}"),
            }
        }
        panic!("SDK did not release the source LOCK after drop");
    }

    struct Fixture {
        db: Database,
        system: SystemContext,
        owner: PrincipalIds,
        other: PrincipalIds,
        receipt: BackupReceipt,
        source: PathBuf,
        _temp: tempfile::TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let backup = temp.path().join("backup");
            let db = Database::connect_persistent(&source).await.unwrap();
            seed_legacy(&db).await;
            drop(db);
            let copied = offline_lock(&source)
                .await
                .copy_to(&backup)
                .expect("copy the v008 source offline");
            let receipt = BackupReceipt::read(&backup).expect("read receipt of real offline copy");
            assert_eq!(receipt.original_files(), &copied);

            let mut db = Database::connect_persistent(&source).await.unwrap();
            schema::migrate(db.inner()).await.unwrap();
            let system = db.claim_system_authority().unwrap();
            let identities = IdentityRepo::new(db.inner());
            let owner = identities
                .bootstrap_owner("nominated", "unused-hash", &OperationId::new("first"))
                .await
                .unwrap();
            let other = identities
                .bootstrap_owner("alternate", "unused-hash", &OperationId::new("second"))
                .await
                .unwrap();
            Self {
                db,
                system,
                owner,
                other,
                receipt,
                source,
                _temp: temp,
            }
        }

        async fn begin(&self) -> MigrationId {
            let result = OwnershipMigration::new(&self.db)
                .begin(&self.system, self.owner.clone(), self.receipt.clone())
                .await;
            assert!(
                result.is_ok(),
                "verified backup and active nominated owner must start migration: {result:?}"
            );
            result.unwrap()
        }

        async fn reopen(mut self) -> Self {
            drop(self.db);
            drop(offline_lock(&self.source).await);
            self.db = Database::connect_persistent(&self.source).await.unwrap();
            self.system = self.db.claim_system_authority().unwrap();
            self
        }

        async fn complete(&self) {
            let id = self.begin().await;
            let migration = OwnershipMigration::new(&self.db);
            page_to_verifying(&migration, &id).await;
            migration
                .verify_and_complete(&id)
                .await
                .expect("a preserved corpus must pass full verification");
            check_ownership_ready(self.db.inner())
                .await
                .expect("a verified complete migration must admit startup");
        }
    }

    async fn page_to_verifying(migration: &OwnershipMigration<'_>, id: &MigrationId) {
        for _ in 0..32 {
            let progress = migration.step(id, 1).await.expect("migrate next page");
            if progress.ready_to_verify {
                return;
            }
        }
        panic!("bounded fixture never reached the verification phase");
    }

    #[tokio::test]
    async fn wrong_logical_backup_refuses_without_mutating_corpus() {
        let fixture = Fixture::new().await;
        fixture
            .db
            .inner()
            .query("UPDATE fact:7 SET content = 'changed after offline backup'")
            .await
            .unwrap()
            .check()
            .unwrap();
        let facts = rows(&fixture.db, FACTS).await;
        let checkpoint = rows(&fixture.db, CHECKPOINTS).await;
        let result = OwnershipMigration::new(&fixture.db)
            .begin(
                &fixture.system,
                fixture.owner.clone(),
                fixture.receipt.clone(),
            )
            .await;
        assert_eq!(rows(&fixture.db, FACTS).await, facts);
        assert_eq!(rows(&fixture.db, CHECKPOINTS).await, checkpoint);
        let error = format!(
            "{:#}",
            result.expect_err("a different corpus must not start")
        );
        let diagnostic = error.to_lowercase();
        assert!(
            diagnostic.contains("backup") && diagnostic.contains("snapshot"),
            "a logically wrong backup must be diagnosed: {error}"
        );
    }

    #[tokio::test]
    async fn corrupt_backup_payload_refuses_without_starting_migration() {
        let fixture = Fixture::new().await;
        let payload = &fixture.receipt.original_files().files[0].path;
        std::fs::OpenOptions::new()
            .append(true)
            .open(fixture.receipt.backup_dir().join(payload))
            .unwrap()
            .write_all(b"corrupted offline copy")
            .unwrap();
        let checkpoint = rows(&fixture.db, CHECKPOINTS).await;
        let result = OwnershipMigration::new(&fixture.db)
            .begin(
                &fixture.system,
                fixture.owner.clone(),
                fixture.receipt.clone(),
            )
            .await;
        assert_eq!(rows(&fixture.db, CHECKPOINTS).await, checkpoint);
        let error = format!(
            "{:#}",
            result.expect_err("modified backup bytes must not start")
        );
        let diagnostic = error.to_lowercase();
        assert!(
            diagnostic.contains("backup")
                && ["digest", "sha256", "size"]
                    .into_iter()
                    .any(|word| diagnostic.contains(word)),
            "corrupted backup bytes must be diagnosed: {error}"
        );
    }

    #[tokio::test]
    async fn missing_backup_receipt_refuses_without_mutating_corpus() {
        let fixture = Fixture::new().await;
        std::fs::remove_file(source_manifest_path(fixture.receipt.backup_dir()).unwrap()).unwrap();
        let facts = rows(&fixture.db, FACTS).await;
        let checkpoint = rows(&fixture.db, CHECKPOINTS).await;
        let result = OwnershipMigration::new(&fixture.db)
            .begin(
                &fixture.system,
                fixture.owner.clone(),
                fixture.receipt.clone(),
            )
            .await;
        assert_eq!(rows(&fixture.db, FACTS).await, facts);
        assert_eq!(rows(&fixture.db, CHECKPOINTS).await, checkpoint);
        let error = format!(
            "{:#}",
            result.expect_err("a missing backup receipt must not start")
        );
        let diagnostic = error.to_lowercase();
        assert!(
            diagnostic.contains("backup")
                && ["missing", "not found", "no such file"]
                    .into_iter()
                    .any(|word| diagnostic.contains(word)),
            "a missing backup must be diagnosed: {error}"
        );
    }

    #[tokio::test]
    async fn replay_resumes_same_owner_and_cursor_after_reopen() {
        let fixture = Fixture::new().await;
        let id = fixture.begin().await;
        let migration = OwnershipMigration::new(&fixture.db);
        assert!(
            !migration.step(&id, 1).await.unwrap().ready_to_verify,
            "one row cannot finish the multi-row corpus"
        );
        let checkpoint = rows(&fixture.db, CHECKPOINTS).await;
        assert_eq!(checkpoint.len(), 1, "there must be one durable journal");
        let progressed = "SELECT id FROM ownership_migration WHERE rows_migrated = 1 \
            AND (fact_cursor != NONE OR session_cursor != NONE OR reminder_cursor != NONE)";
        assert_eq!(
            rows(&fixture.db, progressed).await.len(),
            1,
            "a one-row step must durably advance one population cursor"
        );
        let fixture = fixture.reopen().await;
        assert!(check_ownership_ready(fixture.db.inner()).await.is_err());
        let migration = OwnershipMigration::new(&fixture.db);
        assert!(
            migration
                .begin(
                    &fixture.system,
                    fixture.other.clone(),
                    fixture.receipt.clone()
                )
                .await
                .is_err(),
            "replaying with a different owner must not change the nomination"
        );
        assert_eq!(rows(&fixture.db, CHECKPOINTS).await, checkpoint);
        let resumed = fixture.begin().await;
        assert_eq!(resumed, id, "replay must resume the same migration ID");
        assert_eq!(rows(&fixture.db, CHECKPOINTS).await, checkpoint);
        page_to_verifying(&migration, &resumed).await;
        migration.verify_and_complete(&resumed).await.unwrap();
        assert_owned(&fixture, "fact", 2).await;
    }

    async fn assert_owned(fixture: &Fixture, table: &str, expected: usize) {
        let sql = format!("SELECT * FROM `{table}` WHERE account = $owner AND workspace = $space");
        let facts: Vec<Value> = fixture
            .db
            .inner()
            .query(sql)
            .bind(("owner", fixture.owner.account.as_record_id().clone()))
            .bind(("space", fixture.owner.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(facts.len(), expected, "{table} ownership was not preserved");
    }

    #[tokio::test]
    async fn migration_retains_fact_ids_content_vectors_and_historical_records() {
        let fixture = Fixture::new().await;
        let facts = rows(&fixture.db, FACTS).await;
        let locks = rows(&fixture.db, EMBEDDING_KEYS).await;
        assert_eq!(facts.len(), 2);
        assert!(!locks.is_empty());
        let mut history = Vec::new();
        for query in HISTORY {
            let before = rows(&fixture.db, query).await;
            assert_eq!(before.len(), 1, "fixture must seed {query}");
            history.push(before);
        }

        fixture.complete().await;

        assert_eq!(rows(&fixture.db, FACTS).await, facts);
        assert_eq!(rows(&fixture.db, EMBEDDING_KEYS).await, locks);
        for (query, before) in HISTORY.iter().zip(history) {
            assert_eq!(rows(&fixture.db, query).await, before, "changed {query}");
        }
        for (table, count) in [("fact", 2), ("session", 1), ("raw", 1)] {
            assert_owned(&fixture, table, count).await;
        }
    }

    #[tokio::test]
    async fn unassigned_is_review_only_and_reminder_schedules_survive() {
        let fixture = Fixture::new().await;
        let schedules = rows(&fixture.db, SCHEDULES).await;
        assert_eq!(schedules.len(), 2);
        let legacy_members = "SELECT * FROM contains_memory WHERE in = cluster:legacy";
        assert_eq!(rows(&fixture.db, legacy_members).await.len(), 2);
        let before = rows(&fixture.db, "SELECT * FROM fact WHERE scope = NONE").await;
        assert_eq!(before.len(), 2, "fixture predates scope classification");

        fixture.complete().await;

        let access = AccessRepo::new(&fixture.db);
        for (ids, expected) in [(&fixture.owner, 1), (&fixture.other, 0)] {
            let principal = access
                .local_principal(&fixture.system, &ids.account)
                .await
                .unwrap();
            let owner = access.authorize(&principal).await.unwrap();
            let assigned = access
                .select(&owner, ReadSelection::Assigned)
                .await
                .unwrap();
            let recall = ScopedMemoryRepo::new(fixture.db.inner(), &owner, &assigned);
            assert!(
                recall
                    .nearest(&[1.0, 0.0], 10, RetrievalStrategy::Exact)
                    .await
                    .unwrap()
                    .is_empty(),
                "migrated Unassigned vectors must not enter assigned recall"
            );
            let review = access
                .select(&owner, ReadSelection::UnassignedReview)
                .await
                .unwrap();
            let reviewed = ScopedMemoryRepo::new(fixture.db.inner(), &owner, &review)
                .review_page(None, 10)
                .await
                .unwrap();
            assert_eq!(reviewed.len(), expected, "incorrect Unassigned visibility");
            if expected == 1 {
                assert_eq!(reviewed[0].id, Some(parse_record_id("fact:7").unwrap()));
            }
        }
        assert_eq!(
            rows(
                &fixture.db,
                "SELECT * FROM fact WHERE scope = 'unassigned' AND project = NONE"
            )
            .await
            .len(),
            2,
            "both live and deleted facts remain explicitly Unassigned"
        );
        assert!(rows(&fixture.db, legacy_members).await.is_empty());
        assert_eq!(rows(&fixture.db, SCHEDULES).await, schedules);
        let held: Vec<Value> = fixture
            .db
            .inner()
            .query(
                "SELECT id FROM reminder WHERE account = $owner AND migration_hold = true \
                 AND project = NONE AND legacy_target_project = ' Unmapped Legacy Project '",
            )
            .bind(("owner", fixture.owner.account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            held,
            rows(&fixture.db, "SELECT id FROM reminder:weekly").await
        );
        assert_eq!(
            rows(
                &fixture.db,
                "SELECT id FROM reminder WHERE migration_hold = true"
            )
            .await
            .len(),
            1,
            "an already delivered reminder without a target needs no mapping hold"
        );
    }

    #[tokio::test]
    async fn legacy_provenance_is_owned_but_gets_no_inferred_links() {
        let fixture = Fixture::new().await;
        let provenance =
            "SELECT id, kind, session_id, agent_id, timestamp FROM provenance ORDER BY id";
        let edges = "SELECT * FROM has_provenance ORDER BY id";
        let before_rows = rows(&fixture.db, provenance).await;
        let before_edges = rows(&fixture.db, edges).await;
        assert_eq!(before_rows.len(), 2);
        assert_eq!(before_edges.len(), 1);

        fixture.complete().await;

        assert_owned(&fixture, "provenance", 2).await;
        assert_eq!(rows(&fixture.db, provenance).await, before_rows);
        assert_eq!(
            rows(&fixture.db, edges).await,
            before_edges,
            "matching session or agent metadata is not a provenance edge"
        );
    }

    #[tokio::test]
    async fn failed_verification_cannot_publish_complete() {
        let fixture = Fixture::new().await;
        let id = fixture.begin().await;
        let migration = OwnershipMigration::new(&fixture.db);
        page_to_verifying(&migration, &id).await;
        assert!(check_ownership_ready(fixture.db.inner()).await.is_err());
        let before = rows(&fixture.db, FACTS).await;
        fixture
            .db
            .inner()
            .query("UPDATE fact:7 SET embedding = [-1.0, 0.0]")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_ne!(rows(&fixture.db, FACTS).await, before);
        assert!(
            migration.verify_and_complete(&id).await.is_err(),
            "changing a vector without changing its dimensions must fail full verification"
        );
        assert!(
            rows(
                &fixture.db,
                "SELECT * FROM ownership_migration WHERE state = 'complete'"
            )
            .await
            .is_empty(),
            "failed verification must not publish Complete"
        );
        assert!(check_ownership_ready(fixture.db.inner()).await.is_err());
    }
}
