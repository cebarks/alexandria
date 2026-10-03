//! Writes to `maintenance_log`: the audit trail for background housekeeping.
//!
//! Cluster splits and merges have logged themselves from inside `ClusterRepo` since v004. This repo
//! exists so the dreaming jobs that are *not* cluster moves — collapse and demote — write the same
//! table in the same shape, and so every row a run touched can carry that run's id. Attribution
//! lives on the repo rather than on each `record` call because it is a property of the writer, not
//! of the row: one tick produces one `run_id`, and a call site that forgot to pass it would write an
//! unattributable row rather than fail to compile.

use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

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
                 run_id = $run_id, job = $job, actor = $actor",
            )
            .bind(("action", entry.action.clone()))
            .bind(("source", entry.source_id.clone()))
            .bind(("targets", entry.target_ids.clone()))
            .bind(("count", entry.members_moved))
            .bind(("disposition", entry.disposition.clone()))
            .bind(("run_id", run_id))
            .bind(("job", job))
            .bind(("actor", actor))
            .await?
            .check()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
