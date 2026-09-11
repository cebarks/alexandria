use anyhow::Result;
use chrono::{DateTime, Utc};
use surrealdb::engine::any::Any;
use surrealdb::types::RecordId;
use surrealdb::Surreal;

use crate::models::Reminder;

/// Insert payload for `ReminderRepo::create` (no id/state fields).
#[derive(Debug, Clone)]
pub struct NewReminder {
    pub message: String,
    pub target_project: Option<String>,
    pub prov_project: Option<String>,
    pub prov_session_id: Option<String>,
    pub note: Option<String>,
    pub schedule_kind: String,
    pub due_at: Option<DateTime<Utc>>,
    pub freq: Option<String>,
    pub time_of_day: Option<String>,
    pub weekdays: Vec<String>,
    pub day_of_month: Option<i64>,
    pub cron_expr: Option<String>,
    pub next_due_at: Option<DateTime<Utc>>,
}

pub struct ReminderRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> ReminderRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    /// Create a reminder. State fields are initialized here (status = 'pending',
    /// delivered_count = 0); `created_at` comes from the schema default.
    pub async fn create(&self, r: &NewReminder) -> Result<String> {
        let mut response = self
            .db
            .query(
                "CREATE reminder SET \
                 message = $message, \
                 target_project = $target_project, \
                 prov_project = $prov_project, \
                 prov_session_id = $prov_session_id, \
                 note = $note, \
                 schedule_kind = $schedule_kind, \
                 due_at = $due_at, \
                 freq = $freq, \
                 time_of_day = $time_of_day, \
                 weekdays = $weekdays, \
                 day_of_month = $day_of_month, \
                 cron_expr = $cron_expr, \
                 next_due_at = $next_due_at, \
                 status = 'pending', \
                 delivered_count = 0",
            )
            .bind(("message", r.message.clone()))
            .bind(("target_project", r.target_project.clone()))
            .bind(("prov_project", r.prov_project.clone()))
            .bind(("prov_session_id", r.prov_session_id.clone()))
            .bind(("note", r.note.clone()))
            .bind(("schedule_kind", r.schedule_kind.clone()))
            .bind(("due_at", r.due_at))
            .bind(("freq", r.freq.clone()))
            .bind(("time_of_day", r.time_of_day.clone()))
            .bind(("weekdays", r.weekdays.clone()))
            .bind(("day_of_month", r.day_of_month))
            .bind(("cron_expr", r.cron_expr.clone()))
            .bind(("next_due_at", r.next_due_at))
            .await?;

        let created: Option<Reminder> = response.take(0)?;
        let reminder = created.ok_or_else(|| anyhow::anyhow!("Failed to create reminder"))?;
        let id = reminder
            .id
            .ok_or_else(|| anyhow::anyhow!("Created reminder has no id"))?;
        Ok(crate::record_id_to_string(&id))
    }

    pub async fn get(&self, id: &str) -> Result<Option<Reminder>> {
        let rid = RecordId::parse_simple(id)?;
        let mut response = self
            .db
            .query("SELECT * FROM type::record($id)")
            .bind(("id", rid))
            .await?;
        let reminders: Vec<Reminder> = response.take(0)?;
        Ok(reminders.into_iter().next())
    }

    /// All pending reminders with next_due_at <= now, oldest first.
    pub async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Reminder>> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM reminder \
                 WHERE status = 'pending' AND next_due_at <= $now \
                 ORDER BY next_due_at ASC",
            )
            .bind(("now", now))
            .await?;
        let reminders: Vec<Reminder> = response.take(0)?;
        Ok(reminders)
    }

    /// Status/project filters; both optional. Ordered by next_due_at.
    pub async fn list(
        &self,
        status: Option<&str>,
        target_project: Option<&str>,
    ) -> Result<Vec<Reminder>> {
        let mut conditions = Vec::new();
        if status.is_some() {
            conditions.push("status = $status".to_string());
        }
        if target_project.is_some() {
            conditions.push("target_project = $target_project".to_string());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let query = format!("SELECT * FROM reminder {where_clause} ORDER BY next_due_at ASC");

        let mut q = self.db.query(&query);
        if let Some(s) = status {
            q = q.bind(("status", s.to_string()));
        }
        if let Some(p) = target_project {
            q = q.bind(("target_project", p.to_string()));
        }

        let mut response = q.await?;
        let reminders: Vec<Reminder> = response.take(0)?;
        Ok(reminders)
    }

    /// Consume a delivery. `new_next_due_at = None` → one-shot consumed
    /// (status = 'delivered'). `Some(next)` → recurring, stays pending with
    /// advanced next_due_at.
    pub async fn record_delivery(
        &self,
        id: &str,
        new_next_due_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let rid = RecordId::parse_simple(id)?;
        let set_clause = match new_next_due_at {
            Some(_) => {
                "last_delivered_at = time::now(), \
                 delivered_count += 1, \
                 next_due_at = $next"
            }
            None => {
                "last_delivered_at = time::now(), \
                 delivered_count += 1, \
                 status = 'delivered'"
            }
        };

        let query = format!("UPDATE type::record($id) SET {set_clause}");

        let mut q = self.db.query(&query).bind(("id", rid));
        if let Some(next) = new_next_due_at {
            q = q.bind(("next", next));
        }
        q.await?.check()?;
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> Result<()> {
        let rid = RecordId::parse_simple(id)?;
        self.db
            .query("UPDATE type::record($id) SET status = 'cancelled', cancelled_at = time::now()")
            .bind(("id", rid))
            .await?
            .check()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;
    use chrono::{Duration, Utc};

    fn probe(msg: &str, next_due_at: chrono::DateTime<Utc>, target: Option<&str>) -> NewReminder {
        NewReminder {
            message: msg.to_string(),
            target_project: target.map(str::to_string),
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: "once".to_string(),
            due_at: Some(next_due_at),
            freq: None,
            time_of_day: None,
            weekdays: vec![],
            day_of_month: None,
            cron_expr: None,
            next_due_at: Some(next_due_at),
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo
            .create(&probe("standup", Utc::now() + Duration::hours(1), None))
            .await
            .unwrap();
        assert!(id.starts_with("reminder:"));

        let fetched = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(fetched.message, "standup");
        assert_eq!(fetched.status, "pending");
        assert_eq!(fetched.delivered_count, 0);
    }

    #[tokio::test]
    async fn list_due_respects_time_and_status() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());
        let now = Utc::now();

        repo.create(&probe("past", now - Duration::minutes(5), None))
            .await
            .unwrap();
        repo.create(&probe("future", now + Duration::hours(1), None))
            .await
            .unwrap();
        let cancelled = repo
            .create(&probe("gone", now - Duration::minutes(5), None))
            .await
            .unwrap();
        repo.cancel(&cancelled).await.unwrap();

        let due = repo.list_due(now).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message, "past");
    }

    #[tokio::test]
    async fn record_delivery_once_marks_delivered() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo
            .create(&probe("one", Utc::now() - Duration::minutes(1), None))
            .await
            .unwrap();
        repo.record_delivery(&id, None).await.unwrap();

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "delivered");
        assert_eq!(r.delivered_count, 1);
        assert!(r.last_delivered_at.is_some());
    }

    #[tokio::test]
    async fn record_delivery_recurring_advances() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo
            .create(&probe("daily", Utc::now() - Duration::minutes(1), None))
            .await
            .unwrap();
        let next = Utc::now() + Duration::hours(24);
        repo.record_delivery(&id, Some(next)).await.unwrap();

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "pending");
        assert_eq!(r.delivered_count, 1);
        let stored_next = r.next_due_at.unwrap();
        assert!((stored_next - next).num_seconds().abs() <= 1); // sub-second truncation tolerance
    }

    #[tokio::test]
    async fn cancel_hides_from_due_and_pending() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo
            .create(&probe("x", Utc::now() - Duration::minutes(1), None))
            .await
            .unwrap();
        repo.cancel(&id).await.unwrap();

        assert!(repo.list_due(Utc::now()).await.unwrap().is_empty());
        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "cancelled");
        assert!(r.cancelled_at.is_some());
    }

    #[tokio::test]
    async fn list_filters_by_status_and_project() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        repo.create(&probe("a", Utc::now(), Some("alexandria")))
            .await
            .unwrap();
        repo.create(&probe("b", Utc::now(), None)).await.unwrap();

        assert_eq!(repo.list(Some("pending"), None).await.unwrap().len(), 2);
        assert_eq!(
            repo.list(Some("pending"), Some("alexandria"))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(repo.list(Some("cancelled"), None).await.unwrap().len(), 0);
        assert_eq!(repo.list(None, None).await.unwrap().len(), 2);
    }
}
