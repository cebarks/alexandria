use std::path::Path;
use std::time::Duration;

use surrealdb::types::QueryError;
use tempfile::TempDir;

use super::*;
use crate::models::OperationId;
use crate::offline_copy::OfflineStoreGuard;
use crate::repos::IdentityRepo;

#[derive(Debug, PartialEq)]
struct PageState {
    journal: Vec<Value>,
    members: Vec<Value>,
    facts: Vec<Value>,
}

struct Fixture {
    db: Database,
    id: MigrationId,
    owner: PrincipalIds,
    _temp: TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("store");
        let backup = temp.path().join("backup");
        let db = Database::connect_persistent(&source).await.unwrap();
        for (_, _, sql) in schema::MIGRATIONS.iter().take_while(|(v, _, _)| *v <= 8) {
            db.inner().query(*sql).await.unwrap().check().unwrap();
        }
        system_config::set_config(db.inner(), "schema_version", "8")
            .await
            .unwrap();
        system_config::check_embedding_model(db.inner(), "checkpoint-fixture", 2, 128)
            .await
            .unwrap();
        db.inner()
            .query(
                "CREATE $live SET content = 'live', embedding = [1.0, 0.0];
                 CREATE $deleted SET content = 'historical', embedding = [0.0, 1.0], deleted = true;
                 CREATE $cluster SET centroid = [1.0, 0.0];
                 RELATE $cluster->contains_memory->$live;
                 RELATE $cluster->contains_memory->$deleted;",
            )
            .bind(("live", RecordId::new("fact", "live")))
            .bind(("deleted", RecordId::new("fact", "historical")))
            .bind(("cluster", RecordId::new("cluster", "legacy")))
            .await
            .unwrap()
            .check()
            .unwrap();
        drop(db);
        let original = wait_for_offline_lock(&source)
            .await
            .copy_to(&backup)
            .unwrap();
        let receipt = BackupReceipt::read(&backup).unwrap();
        assert_eq!(receipt.original_files(), &original);

        let mut db = Database::connect_persistent(&source).await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let owner = IdentityRepo::new(db.inner())
            .bootstrap_owner("checkpoint-test", "unused-hash", &OperationId::new("seed"))
            .await
            .unwrap();
        let migration = OwnershipMigration::new(&db);
        let id = migration
            .begin(&system, owner.clone(), receipt)
            .await
            .unwrap();
        assert!(!migration.step(&id, 2).await.unwrap().ready_to_verify);
        let result = Self {
            db,
            id,
            owner,
            _temp: temp,
        };
        let before = result.state().await;
        assert_eq!(before.members.len(), 2);
        assert_eq!(before.facts.len(), 2);
        assert_eq!(result.journal().await.count().unwrap(), 2);
        result
    }

    fn migration(&self) -> OwnershipMigration<'_> {
        OwnershipMigration::new(&self.db)
    }

    async fn state(&self) -> PageState {
        let mut read = self
            .db
            .inner()
            .query(
                "SELECT * FROM ownership_migration;
                 SELECT * FROM contains_memory ORDER BY id;
                 SELECT * FROM fact ORDER BY id;",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        PageState {
            journal: read.take(0).unwrap(),
            members: read.take(1).unwrap(),
            facts: read.take(2).unwrap(),
        }
    }

    async fn journal(&self) -> Journal {
        let mut read = self
            .db
            .inner()
            .query(JOURNAL)
            .await
            .unwrap()
            .check()
            .unwrap();
        Journal::read(read.take(0).unwrap()).unwrap().unwrap()
    }

    async fn conflict_at_commit(&self, tx: Transaction<Any>) -> surrealdb::Error {
        // A second SDK session on the same synthetic store, never a second engine open.
        let writer = self.db.inner().clone();
        let key = self.owner.workspace.as_record_id().clone();
        let written: Vec<Datetime> = writer
            .query("UPDATE $space SET updated_at = updated_at + 1s RETURN VALUE updated_at")
            .bind(("space", key.clone()))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            written.len(),
            1,
            "the independent write must reach the workspace"
        );
        let Err(error) = tx.commit().await else {
            panic!("the real checkpoint commit must conflict");
        };
        assert!(
            matches!(error.query_details(), Some(QueryError::TransactionConflict)),
            "explicit persistent commit did not preserve a typed rejection: {error:?}",
        );
        let current: Vec<Datetime> = writer
            .query("SELECT VALUE updated_at FROM $space")
            .bind(("space", key))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(current, written, "the independent write survived the abort");
        error
    }

    async fn assert_incomplete(&self) {
        let error = check_ownership_ready(self.db.inner()).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Ownership migration incomplete; resume it offline before serving"
        );
    }
}

async fn wait_for_offline_lock(path: &Path) -> OfflineStoreGuard {
    for _ in 0..100 {
        match OfflineStoreGuard::acquire(path) {
            Ok(guard) => return guard,
            Err(err) if err.to_string().contains("Source store is locked") => {
                // SurrealKV releases the lock asynchronously after the last SDK handle drops.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("offline lock admission failed: {err:#}"),
        }
    }
    panic!("synthetic store did not close");
}

#[tokio::test]
async fn persistent_checkpoint_retries_definite_conflict() {
    let fixture = Fixture::new().await;
    let before = fixture.state().await;
    let receipt = fixture.journal().await.backup_receipt;
    let mut attempts = 0;
    let progress = fixture
        .migration()
        .step_with_commit(&fixture.id, 1, async |tx: Transaction<Any>| {
            attempts += 1;
            if attempts != 1 {
                return tx.commit().await.map(|_| ());
            }
            let error = fixture.conflict_at_commit(tx).await;
            assert_eq!(
                fixture.state().await,
                before,
                "aborted checkpoint leaked writes"
            );
            Err(error)
        })
        .await
        .expect("a definitely aborted checkpoint must retry");
    assert_eq!(attempts, 2);
    assert!(!progress.ready_to_verify);
    let after = fixture.state().await;
    assert_eq!(after.members.len(), 1, "retry must commit just one row");
    assert!(before.members.contains(&after.members[0]));
    assert_eq!(after.facts, before.facts);
    let journal = fixture.journal().await;
    assert_eq!(journal.count().unwrap(), 3);
    assert_eq!(
        journal.checkpoint().unwrap().population,
        Some(Population::Memberships)
    );
    assert_eq!(journal.backup_receipt, receipt);
    fixture.assert_incomplete().await;
}

#[tokio::test]
async fn persistent_checkpoint_exhaustion_preserves_journal_and_edges() {
    let fixture = Fixture::new().await;
    let before = fixture.state().await;
    let migration = fixture.migration();
    let mut attempts = 0;
    let mut last = None;
    let error = migration
        .step_with_commit(&fixture.id, 1, async |tx| {
            attempts += 1;
            assert!(attempts <= 3, "must not start a fourth attempt");
            let error = fixture.conflict_at_commit(tx).await;
            assert_eq!(fixture.state().await, before, "aborted page leaked writes");
            last = Some(error.clone());
            Err(error)
        })
        .await
        .expect_err("three definite aborts exhaust the retry budget");
    assert_eq!(attempts, 3);
    assert_eq!(error.to_string(), "Commit ownership migration transaction");
    assert_eq!(error.downcast_ref::<surrealdb::Error>(), last.as_ref());
    fixture.assert_incomplete().await;

    assert!(
        !migration
            .step(&fixture.id, 1)
            .await
            .unwrap()
            .ready_to_verify
    );
    assert_eq!(fixture.journal().await.count().unwrap(), 3);
    assert!(
        migration
            .step(&fixture.id, 2)
            .await
            .unwrap()
            .ready_to_verify
    );
    fixture.assert_incomplete().await;
    let report = migration.verify_and_complete(&fixture.id).await.unwrap();
    assert_eq!(report.rows_migrated, 4);
    assert_eq!(report.held_reminders, 0);
    let after = fixture.state().await;
    assert_eq!(after.facts, before.facts);
    assert!(after.members.is_empty());
    check_ownership_ready(fixture.db.inner()).await.unwrap();
}

#[tokio::test]
async fn checkpoint_ambiguous_commit_does_not_retry() {
    let fixture = Fixture::new().await;
    // Model a lost acknowledgement after a real commit. The pinned core maps
    // CommitOutcomeUnknown to Internal, not Query(TransactionConflict).
    let unknown = surrealdb::Error::internal("Commit outcome unknown: acknowledgement lost".into());
    let mut attempts = 0;
    let error = fixture
        .migration()
        .step_with_commit(&fixture.id, 1, async |tx| {
            attempts += 1;
            tx.commit().await.unwrap();
            Err(unknown.clone())
        })
        .await
        .expect_err("unknown acknowledgement is not success or retryable");
    assert_eq!(attempts, 1);
    assert_eq!(error.downcast_ref::<surrealdb::Error>(), Some(&unknown));
    assert_eq!(fixture.journal().await.count().unwrap(), 3);
    assert_eq!(fixture.state().await.members.len(), 1);
    fixture.assert_incomplete().await;
}

#[tokio::test]
async fn checkpoint_conflict_text_without_typed_rejection_does_not_retry() {
    let fixture = Fixture::new().await;
    let before = fixture.state().await;
    // A nonconflict Internal error must not retry, even with identical text.
    let nonconflict = surrealdb::Error::internal(
        "Transaction conflict: Transaction write conflict. This transaction can be retried".into(),
    );
    let mut attempts = 0;
    let error = fixture
        .migration()
        .step_with_commit(&fixture.id, 1, async |tx| {
            attempts += 1;
            tx.cancel().await.unwrap();
            Err(nonconflict.clone())
        })
        .await
        .expect_err("conflict text is not a definite rejection");
    assert_eq!(attempts, 1);
    assert_eq!(error.downcast_ref::<surrealdb::Error>(), Some(&nonconflict));
    assert_eq!(fixture.state().await, before);
}

#[tokio::test]
async fn checkpoint_retry_rechecks_owner_and_refuses_before_second_commit() {
    let fixture = Fixture::new().await;
    let before = fixture.state().await;
    let mut attempts = 0;
    let error = fixture
        .migration()
        .step_with_commit(&fixture.id, 1, async |tx| {
            attempts += 1;
            assert_eq!(
                attempts, 1,
                "retry must notice the suspended owner before commit"
            );
            let status: Vec<String> = fixture
                .db
                .inner()
                .query("UPDATE $owner SET status = 'suspended' RETURN VALUE status")
                .bind(("owner", fixture.owner.account.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap()
                .take(0)
                .unwrap();
            assert_eq!(status, ["suspended"]);
            Err(fixture.conflict_at_commit(tx).await)
        })
        .await
        .expect_err("the suspended owner must refuse the next attempt");
    assert_eq!(attempts, 1);
    assert_eq!(
        error.to_string(),
        "Nominated ownership migration principal is inactive or no longer owns the workspace"
    );
    assert_eq!(fixture.state().await, before);
    fixture.assert_incomplete().await;
}
