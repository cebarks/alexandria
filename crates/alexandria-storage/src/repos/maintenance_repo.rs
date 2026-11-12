//! Writes to `maintenance_log`: the audit trail for background housekeeping.
//!
//! Cluster splits and merges have logged themselves from inside `ClusterRepo` since v004. This repo
//! exists so the dreaming jobs that are *not* cluster moves — collapse and demote — write the same
//! table in the same shape, and so every row a run touched can carry that run's id. Attribution
//! lives on the repo rather than on each `record` call because it is a property of the writer, not
//! of the row: one tick produces one `run_id`, and a call site that forgot to pass it would write an
//! unattributable row rather than fail to compile.

use anyhow::Result;
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::SurrealValue;

use crate::access::AdminCapability;
use crate::error::StorageError;
use crate::models::maintenance::ACTOR_DREAMING;

/// Who wrote a row, and which run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditContext {
    /// One tick's unit of reversal: every row a run touched shares this id, so a pass that
    /// soft-deleted 40 memories is one event a human can undo wholesale.
    pub run_id: String,
    /// Which job wrote it — a `models::maintenance::job` constant.
    pub job: String,
    /// Writer identity. `system:dreaming` for every job, because dreaming is the one writer that can
    /// never produce a user identity.
    pub actor: String,
}

impl AuditContext {
    pub fn dreaming(run_id: impl Into<String>, job: &str) -> Self {
        Self {
            run_id: run_id.into(),
            job: job.to_string(),
            actor: ACTOR_DREAMING.to_string(),
        }
    }
}

/// One audit row.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// The verb: a `models::maintenance::action` constant.
    pub action: String,
    /// What was acted on — the collapsed fact, the demoted fact, the cluster that was split.
    pub source_id: String,
    /// What it became: the survivor, the merge target, the two split halves. Empty for a demote,
    /// which changes a value rather than moving anything.
    pub target_ids: Vec<String>,
    pub members_moved: i64,
    /// Which rung of the demote → quarantine → soft-delete ladder the row landed on, or `None` for
    /// the cluster moves, which are not a disposition.
    pub disposition: Option<String>,
    /// For a value-changing verb, the value before the write. `Appraise` sets it to the confidence it
    /// is about to overwrite; every other writer passes `None`, which is what pre-v008 rows read as.
    pub previous_value: Option<f64>,
}

pub struct MaintenanceRepo<'a> {
    db: &'a Surreal<Any>,
    audit: Option<AuditContext>,
}

impl<'a> MaintenanceRepo<'a> {
    /// Unattributed: rows get `NONE` for `run_id`, `job` and `actor`, exactly as every row written
    /// before v008 does.
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db, audit: None }
    }

    /// Attribute every row this repo writes to one run of one job.
    pub fn with_audit(db: &'a Surreal<Any>, audit: AuditContext) -> Self {
        Self {
            db,
            audit: Some(audit),
        }
    }

    /// The audit trail as account-management **health**: one row per scheduler run, with counts and
    /// timings and nothing else.
    ///
    /// `maintenance_log` carries no owner or workspace column (v004/v008), so no read of it can be
    /// narrowed to one corpus — which is why this exists at all, and why it is bounded by
    /// [`AdminCapability`] instead of being a scoped read. It reports what a pass *did* in aggregate
    /// (`entries`, `members_moved`, when it ran, which verbs it used) and never names a record: no
    /// `source_id`, no `target_ids`, no `previous_value`. An owner's per-record history is Task 10's
    /// journal page, over `semantic_operation`, which does carry a workspace.
    ///
    /// Bounded by `limit`/`offset` and ordered newest-run-first, so a long-lived store costs one
    /// grouped read per page rather than a sweep of every row ever written.
    ///
    /// `_admin` is a *type-level* gate rather than a query predicate: the statement names no row of
    /// anybody's, so there is nothing to scope by, and the only thing that can be checked is that the
    /// caller holds a capability `AccessRepo::authorize_admin` issued from a live `role = 'admin'` row.
    /// A `pub` method with no such argument would be reachable from any handler that happened to want
    /// it, which is how an aggregate surface quietly becomes a route anyone can mount.
    pub async fn run_rollup(
        &self,
        _admin: &AdminCapability,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MaintenanceRunSummary>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT run_id, array::group(job) AS jobs, array::group(action) AS actions, \n                 count() AS entries, math::sum(members_moved) AS members_moved, \n                 time::min(created_at) AS first_at, time::max(created_at) AS last_at \n                 FROM maintenance_log GROUP BY run_id ORDER BY last_at DESC LIMIT $limit START $offset;",
            )
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<RunRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .map(|row| MaintenanceRunSummary {
                // An unattributed row set groups under `None`, which is what every pre-v008 row and
                // every writer that held no `AuditContext` produced. It is reported as its own run
                // rather than dropped, because hiding it would understate what the scheduler did.
                run_id: row.run_id,
                jobs: row.jobs.into_iter().flatten().collect(),
                actions: row.actions.into_iter().flatten().collect(),
                entries: row.entries.max(0) as usize,
                members_moved: row.members_moved.max(0),
                first_at: row.first_at,
                last_at: row.last_at,
            })
            .collect())
    }

    /// How many runs the rollup pages over. The same grouping as [`Self::run_rollup`], so the pager
    /// cannot disagree with the table under it.
    pub async fn count_runs(&self, _admin: &AdminCapability) -> Result<usize, StorageError> {
        #[derive(SurrealValue)]
        struct CountRow {
            runs: i64,
        }
        let mut response = self
            .db
            .query("SELECT count() AS runs FROM (SELECT run_id FROM maintenance_log GROUP BY run_id) GROUP ALL;")
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<CountRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .first()
            .map(|row| row.runs.max(0) as usize)
            .unwrap_or(0))
    }

    pub async fn record(&self, entry: &LogEntry) -> Result<()> {
        // The three attribution columns are bound as `Option`, so an unattributed repo writes NONE
        // rather than an empty string — and NONE is what every pre-v008 row already reads as, so
        // `/debug/maintenance` renders old and new rows the same way.
        let run_id = self.audit.as_ref().map(|a| a.run_id.clone());
        let job = self.audit.as_ref().map(|a| a.job.clone());
        let actor = self.audit.as_ref().map(|a| a.actor.clone());

        self.db
            .query(
                "CREATE maintenance_log SET action = $action, source_id = $source, \
                 target_ids = $targets, members_moved = $count, disposition = $disposition, \
                 previous_value = $previous_value, \
                 run_id = $run_id, job = $job, actor = $actor",
            )
            .bind(("action", entry.action.clone()))
            .bind(("source", entry.source_id.clone()))
            .bind(("targets", entry.target_ids.clone()))
            .bind(("count", entry.members_moved))
            .bind(("disposition", entry.disposition.clone()))
            .bind(("previous_value", entry.previous_value))
            .bind(("run_id", run_id))
            .bind(("job", job))
            .bind(("actor", actor))
            .await?
            .check()?;
        Ok(())
    }
}

/// One scheduler run, as the admin health page reports it: counts, verbs, timings. No identifier of
/// any row a run touched.
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceRunSummary {
    /// The run this row belongs to; `None` for the unattributed rows every pre-v008 writer left.
    pub run_id: Option<String>,
    /// Which jobs wrote under this run — `cluster`/`merge`/`collapse`/`appraise`, never a per-row verb
    /// identity. A run that mixed jobs reports each once per log row, so this is a list, not a set.
    pub jobs: Vec<String>,
    /// The action verbs the run used (`split`, `merge`, `collapse`, `demote`), one entry per log row.
    pub actions: Vec<String>,
    pub entries: usize,
    pub members_moved: i64,
    pub first_at: Option<DateTime<Utc>>,
    pub last_at: Option<DateTime<Utc>>,
}

/// The grouped row [`MaintenanceRepo::run_rollup`] reads. `jobs`/`actions` come back as arrays because
/// the grouping is per run, and the counts are `i64` because that is what the engine's `count()` is.
#[derive(SurrealValue)]
struct RunRow {
    run_id: Option<String>,
    /// `Vec<Option<String>>` because the grouped value carries one element per log row, and an
    /// unattributed row — every writer that held no `AuditContext`, and every row predating v008 —
    /// contributes a `NONE` job and action. Deserialising those as `String` fails the whole page
    /// (measured), so the absence is read as what it is and dropped here rather than invented.
    jobs: Vec<Option<String>>,
    actions: Vec<Option<String>>,
    entries: i64,
    members_moved: i64,
    first_at: Option<DateTime<Utc>>,
    last_at: Option<DateTime<Utc>>,
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OperationId, account_role, account_status};
    use crate::repos::{AuthRepo, IdentityRepo};

    /// A capability for the rollup's type-level gate.
    ///
    /// `AdminCapability`'s only producer is `AccessRepo::authorize_admin`, which needs a real
    /// authenticated principal — so the fixture builds one the way the authorization tests do: enrol
    /// an account, promote its `role` through the row the store reports, mint a browser session,
    /// authenticate it, ask storage. There is deliberately no test-only constructor to reach for.
    async fn capability(db: &crate::Database) -> AdminCapability {
        let digest =
            AuthRepo::create_password_digest("rollup-fixture-password-rollup-fixture-padding")
                .await
                .unwrap();
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "rollup-admin",
                digest.as_str(),
                &OperationId::new("rollup-1"),
            )
            .await
            .unwrap();
        db.inner()
            .query("UPDATE ONLY $target SET role = $role RETURN NONE;")
            .bind(("target", ids.account.as_record_id().clone()))
            .bind(("role", account_role::ADMIN.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT role, status FROM $target;")
            .bind(("target", ids.account.as_record_id().clone()))
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            rows[0]["role"],
            account_role::ADMIN,
            "the role write did not land"
        );
        assert_eq!(rows[0]["status"], account_status::ACTIVE);
        let session = AuthRepo::new(db.inner())
            .mint_browser_session(
                &ids.account,
                chrono::Duration::minutes(30),
                chrono::Duration::hours(24),
                Utc::now(),
            )
            .await
            .unwrap();
        let principal = AuthRepo::new(db.inner())
            .authenticate_browser_session(
                session.cookie.render().expose(),
                chrono::Duration::minutes(30),
                Utc::now(),
            )
            .await
            .unwrap();
        crate::access::AccessRepo::new(db)
            .authorize_admin(&principal)
            .await
            .expect("the fixture administrator authorizes")
    }

    /// Writes `rows` under one `run_id`, each attributed to its own job — the shape a scheduler tick
    /// that ran two jobs produces.
    async fn seed_run(db: &crate::Database, run: &str, rows: &[(&str, &str, i64)]) {
        for (job, action, moved) in rows {
            MaintenanceRepo::with_audit(db.inner(), AuditContext::dreaming(run, job))
                .record(&LogEntry {
                    action: action.to_string(),
                    source_id: format!("cluster:source-{run}-{action}"),
                    target_ids: vec![format!("cluster:target-{run}-{action}")],
                    members_moved: *moved,
                    disposition: None,
                    previous_value: Some(0.42),
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_run_rollup_aggregates_without_naming_any_record() {
        let db = crate::Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let admin = capability(&db).await;
        seed_run(
            &db,
            "run-9",
            &[
                (crate::models::maintenance::job::CLUSTER, "split", 4),
                (crate::models::maintenance::job::APPRAISE, "demote", 0),
            ],
        )
        .await;
        seed_run(
            &db,
            "run-10",
            &[(crate::models::maintenance::job::MERGE, "merge", 7)],
        )
        .await;

        let repo = MaintenanceRepo::new(db.inner());
        let rollup = repo.run_rollup(&admin, 20, 0).await.unwrap();
        assert_eq!(rollup.len(), 2, "one row per run: {rollup:?}");
        // Newest run first, by the last write it made.
        assert_eq!(rollup[0].run_id.as_deref(), Some("run-10"));
        assert_eq!(rollup[0].entries, 1);
        assert_eq!(rollup[0].members_moved, 7);
        assert_eq!(rollup[1].run_id.as_deref(), Some("run-9"));
        assert_eq!(rollup[1].entries, 2);
        assert_eq!(rollup[1].members_moved, 4);
        assert_eq!(rollup[1].jobs.len(), 2);
        assert!(rollup[1].actions.contains(&"split".to_string()));
        assert!(rollup[1].actions.contains(&"demote".to_string()));
        assert!(rollup[1].first_at.is_some() && rollup[1].last_at.is_some());
        assert_eq!(repo.count_runs(&admin).await.unwrap(), 2);

        // The disclosure this page exists to guarantee: no record identifier of any kind, and no
        // value a row carried.
        let rendered = format!("{rollup:?}");
        for leak in ["cluster:source", "cluster:target", "0.42", "previous_value"] {
            assert!(
                !rendered.contains(leak),
                "the rollup carried {leak}: {rendered}"
            );
        }
        assert!(
            !rendered.contains("fact:"),
            "the rollup named a fact: {rendered}"
        );
    }

    #[tokio::test]
    async fn test_run_rollup_pages_and_keeps_unattributed_rows_visible_as_a_group() {
        let db = crate::Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let admin = capability(&db).await;
        seed_run(&db, "run-a", &[("cluster", "split", 2)]).await;
        seed_run(&db, "run-b", &[("merge", "merge", 3)]).await;
        // An unattributed write, exactly as a repo built with `MaintenanceRepo::new` produces today.
        MaintenanceRepo::new(db.inner())
            .record(&LogEntry {
                action: "collapse".to_string(),
                source_id: "fact:gone".to_string(),
                target_ids: vec!["fact:survivor".to_string()],
                members_moved: 1,
                disposition: Some("soft_delete".to_string()),
                previous_value: None,
            })
            .await
            .unwrap();

        let repo = MaintenanceRepo::new(db.inner());
        let first = repo.run_rollup(&admin, 1, 0).await.unwrap();
        assert_eq!(first.len(), 1, "the page is one run: {first:?}");
        assert_eq!(
            repo.count_runs(&admin).await.unwrap(),
            3,
            "two runs plus the unattributed group"
        );
        let all = repo.run_rollup(&admin, 10, 0).await.unwrap();
        assert!(
            all.iter().any(|run| run.run_id.is_none()),
            "the unattributed rows vanished rather than grouping: {all:?}"
        );
        assert_eq!(
            all.iter()
                .filter(|run| run.run_id.is_some())
                .map(|run| run.entries)
                .sum::<usize>()
                + all
                    .iter()
                    .filter(|run| run.run_id.is_none())
                    .map(|run| run.entries)
                    .sum::<usize>(),
            3,
            "the pages must account for every log row: {all:?}"
        );
    }
    use crate::connection::Database;
    use crate::models::MaintenanceLog;
    use crate::models::maintenance::{action, disposition, job};

    async fn log(db: &Database) -> Vec<MaintenanceLog> {
        let mut response = db
            .inner()
            .query("SELECT * FROM maintenance_log")
            .await
            .unwrap();
        response.take(0).unwrap()
    }

    /// Find a row by what it says rather than by where it sits. `maintenance_log` ids are generated,
    /// so their sort order is not insertion order — a test that asserts `rows[0]` is the row written
    /// first passes on one run and fails on the next. Same class of bug as the NULL `ended_at` tail
    /// that made `SessionRepo::list` pagination repeat a row.
    fn find<'a>(rows: &'a [MaintenanceLog], action: &str) -> &'a MaintenanceLog {
        rows.iter()
            .find(|row| row.action == action)
            .unwrap_or_else(|| panic!("no {action} row in {rows:?}"))
    }

    fn entry(
        action: &str,
        source: &str,
        targets: Vec<&str>,
        disposition: Option<&str>,
    ) -> LogEntry {
        LogEntry {
            action: action.to_string(),
            source_id: source.to_string(),
            target_ids: targets.into_iter().map(str::to_string).collect(),
            members_moved: 0,
            disposition: disposition.map(str::to_string),
            previous_value: None,
        }
    }

    /// The v008 columns are the reason this repo exists, so they are asserted rather than assumed:
    /// an attributed row must carry all three, and an unattributed one must read as `None` — the same
    /// shape as every row written before v008, which is what keeps `/debug/maintenance` rendering one
    /// table instead of two kinds of row.
    #[tokio::test]
    async fn attributed_and_unattributed_rows_round_trip() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();

        MaintenanceRepo::with_audit(db.inner(), AuditContext::dreaming("run-1", job::COLLAPSE))
            .record(&entry(
                action::COLLAPSE,
                "fact:dup",
                vec!["fact:survivor"],
                Some(disposition::SOFT_DELETE),
            ))
            .await
            .unwrap();

        MaintenanceRepo::new(db.inner())
            .record(&entry(
                action::DEMOTE,
                "fact:cold",
                vec![],
                Some(disposition::DEMOTE),
            ))
            .await
            .unwrap();

        let rows = log(&db).await;
        assert_eq!(rows.len(), 2);

        let collapse = find(&rows, action::COLLAPSE);
        assert_eq!(collapse.action, action::COLLAPSE);
        assert_eq!(collapse.source_id, "fact:dup");
        assert_eq!(collapse.target_ids, vec!["fact:survivor".to_string()]);
        assert_eq!(
            collapse.disposition.as_deref(),
            Some(disposition::SOFT_DELETE)
        );
        assert_eq!(collapse.run_id.as_deref(), Some("run-1"));
        assert_eq!(collapse.job.as_deref(), Some(job::COLLAPSE));
        assert_eq!(
            collapse.actor.as_deref(),
            Some(crate::models::maintenance::ACTOR_DREAMING)
        );

        let demote = find(&rows, action::DEMOTE);
        assert!(demote.target_ids.is_empty(), "a demote moves nothing");
        assert_eq!(demote.run_id, None, "unattributed reads as NONE, not \"\"");
        assert_eq!(demote.job, None);
        assert_eq!(demote.actor, None);
    }

    /// Every row one run writes must share its id, or `run_id` is not the unit of reversal it is
    /// documented to be: an operator undoing a bad pass needs to select all of it in one query.
    #[tokio::test]
    async fn one_run_shares_one_id_across_rows() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MaintenanceRepo::with_audit(
            db.inner(),
            AuditContext::dreaming("run-42", job::COLLAPSE),
        );

        for i in 0..3 {
            repo.record(&entry(
                action::COLLAPSE,
                &format!("fact:dup{i}"),
                vec!["fact:survivor"],
                Some(disposition::SOFT_DELETE),
            ))
            .await
            .unwrap();
        }

        let rows = log(&db).await;
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| row.run_id.as_deref() == Some("run-42")),
            "all rows of a run share its id: {rows:?}"
        );
    }
}
