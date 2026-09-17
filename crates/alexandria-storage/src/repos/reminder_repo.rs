use anyhow::Result;
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::RecordId;

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
    ///
    /// Deliberately unbounded: this is the delivery query, and a row this returns
    /// but does not render would be consumed invisibly. Callers that only want a
    /// sample (the read-only `due_reminders` piggyback) use
    /// [`Self::list_due_sample`] so the bound is paid for in the database.
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

    /// At most `limit` pending reminders due at or before `now`, oldest first.
    ///
    /// The `LIMIT` is in the query, not applied afterwards: the informational
    /// piggyback runs on every memory lookup, so fetching the whole overdue set to
    /// show a few of it deserializes every message and note on each one. Callers
    /// that must also drop rows they cannot interpret pass a `limit` with slack —
    /// see `DUE_SAMPLE_SLACK` in the MCP layer.
    ///
    /// `next_due_at != NONE` is also load-bearing for ordering, not just tidiness:
    /// NONE sorts below every datetime and `next_due_at <= $now` *selects* such a
    /// row (verified against the embedded 3.2 engine), so an admin- or
    /// migration-written row with no due time would otherwise sit at the head of
    /// the sample and displace real reminders, permanently.
    ///
    /// The spelling is a SurrealDB 3.2 trap, not a style choice: `IS NOT NULL` (and
    /// `!= NULL`) are both satisfied by a `NONE` field, so they filter nothing here.
    /// Only `!= NONE` — or `NOT (= NONE)` — distinguishes an absent field.
    pub async fn list_due_sample(&self, now: DateTime<Utc>, cap: i64) -> Result<Vec<Reminder>> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM reminder \
                 WHERE status = 'pending' AND next_due_at <= $now AND next_due_at != NONE \
                 ORDER BY next_due_at ASC \
                 LIMIT $cap",
            )
            .bind(("now", now))
            .bind(("cap", cap))
            .await?;
        let reminders: Vec<Reminder> = response.take(0)?;
        Ok(reminders)
    }

    /// Status/project filters; both optional. Ordered by next_due_at.
    ///
    /// `limit` bounds the page and `offset` skips one; the returned flag says
    /// whether more rows exist beyond it. One extra row is read to answer that
    /// without a second `count()` query — the history this pages over is
    /// append-only (`delivered` and `cancelled` rows are kept forever), so the
    /// caller cannot infer it from the page it got.
    pub async fn list(
        &self,
        status: Option<&str>,
        target_project: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Reminder>, bool)> {
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

        // `id` breaks ties: `next_due_at` is not unique (two reminders set for the
        // same minute share it), and without a stable secondary key `LIMIT`/`START`
        // can repeat or skip a row between pages.
        let query = format!(
            "SELECT * FROM reminder {where_clause} ORDER BY next_due_at ASC, id ASC LIMIT $limit START $offset"
        );

        let mut q = self.db.query(&query);
        if let Some(s) = status {
            q = q.bind(("status", s.to_string()));
        }
        if let Some(p) = target_project {
            q = q.bind(("target_project", p.to_string()));
        }
        // Read one past the page: that row's existence is the `truncated` answer.
        q = q.bind(("limit", limit + 1)).bind(("offset", offset.max(0)));

        let mut response = q.await?;
        let mut reminders: Vec<Reminder> = response.take(0)?;
        let truncated = reminders.len() as i64 > limit;
        if truncated {
            reminders.pop();
        }
        Ok((reminders, truncated))
    }

    /// Claim a delivery — a conditional write, not an update.
    ///
    /// `seen_next_due_at` is the caller's *verbatim* observed `next_due_at` for
    /// the row (the value `list_due` returned), and both UPDATE paths are guarded
    /// by `status = 'pending' AND next_due_at = $seen`: the write only lands if
    /// the row still looks exactly as it did when it was read. That is what makes
    /// consumption race-safe against the two things an unguarded read-modify-write
    /// gets wrong — two overlapping `check_reminders` calls both delivering the
    /// same row (`delivered_count` 2), and a `cancel` landing between the read and
    /// the write being clobbered back to `status = 'delivered'` while
    /// `cancelled_at` stays set.
    ///
    /// `new_next_due_at = None` → one-shot consumed (status = 'delivered').
    /// `Some(next)` → recurring, stays pending with an advanced next_due_at.
    ///
    /// Returns whether the claim was taken: `true` = this caller consumed the row,
    /// `false` = the guard stopped matching, so someone else already consumed or
    /// advanced it, or it was cancelled — the caller must skip the row and must
    /// not report it. SurrealDB returns exactly the rows an UPDATE wrote (1 for a
    /// winner, 0 for a loser), which is what the boolean is read from.
    ///
    /// A `None` seen value is not "skip the guard": it compares against a NULL
    /// `next_due_at`, so it can only claim a row that was observed without one.
    /// `do_check_reminders` skips those rows before recording, so at the call
    /// site the seen value is always a real datetime.
    pub async fn record_delivery(
        &self,
        id: &str,
        new_next_due_at: Option<DateTime<Utc>>,
        seen_next_due_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
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

        // The WHERE guard is the claim itself: `next_due_at` is compared before
        // `SET` writes the new one, so the row is matched on what the caller read,
        // not on what this statement is about to make it.
        let query = format!(
            "UPDATE type::record($id) SET {set_clause} \
             WHERE status = 'pending' AND next_due_at = $seen"
        );

        let mut q = self
            .db
            .query(&query)
            .bind(("id", rid))
            .bind(("seen", seen_next_due_at));
        if let Some(next) = new_next_due_at {
            q = q.bind(("next", next));
        }
        let mut response = q.await?.check()?;
        let updated: Vec<Reminder> = response.take(0)?;
        Ok(updated.len() == 1)
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

        let due = Utc::now() - Duration::minutes(1);
        let id = repo.create(&probe("one", due, None)).await.unwrap();
        let seen = repo.get(&id).await.unwrap().unwrap().next_due_at;
        assert!(
            repo.record_delivery(&id, None, seen).await.unwrap(),
            "first claim on an untouched row must succeed"
        );

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

        let due = Utc::now() - Duration::minutes(1);
        let id = repo.create(&probe("daily", due, None)).await.unwrap();
        let seen = repo.get(&id).await.unwrap().unwrap().next_due_at;
        let next = Utc::now() + Duration::hours(24);
        assert!(
            repo.record_delivery(&id, Some(next), seen).await.unwrap(),
            "first claim on an untouched row must succeed"
        );

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "pending");
        assert_eq!(r.delivered_count, 1);
        let stored_next = r.next_due_at.unwrap();
        assert!((stored_next - next).num_seconds().abs() <= 1); // sub-second truncation tolerance
    }

    /// Claim semantics: the first consumer takes the row, every later claim with
    /// the same seen value loses — the guard is what stops `list_due` →
    /// `record_delivery` being an unguarded read-modify-write.
    #[tokio::test]
    async fn record_delivery_claim_is_taken_only_once() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let now = Utc::now();
        let id = repo
            .create(&probe("one", now - Duration::minutes(1), None))
            .await
            .unwrap();
        let due = repo.list_due(now).await.unwrap();
        assert_eq!(due.len(), 1);
        let seen = due[0].next_due_at;

        assert!(
            repo.record_delivery(&id, None, seen).await.unwrap(),
            "the first claim must be taken"
        );
        assert!(
            !repo.record_delivery(&id, None, seen).await.unwrap(),
            "a second claim with the same seen value must lose (double-delivery guard)"
        );

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "delivered");
        assert_eq!(r.delivered_count, 1, "the loser must not consume again");
    }

    /// A `cancel` landing between `list_due` and the write must survive: without
    /// the status half of the guard the row was silently rewritten to
    /// `delivered` with `cancelled_at` still set, and the user was told a
    /// cancelled reminder had been delivered.
    #[tokio::test]
    async fn record_delivery_after_cancel_loses_claim_and_keeps_row_cancelled() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let now = Utc::now();
        let id = repo
            .create(&probe("gone", now - Duration::minutes(1), None))
            .await
            .unwrap();
        let seen = repo.list_due(now).await.unwrap()[0].next_due_at;

        repo.cancel(&id).await.unwrap();

        assert!(
            !repo.record_delivery(&id, None, seen).await.unwrap(),
            "a cancelled row must not be claimable"
        );
        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(
            r.status, "cancelled",
            "the lost claim must not resurrect the row as delivered"
        );
        assert!(r.cancelled_at.is_some());
        assert_eq!(r.delivered_count, 0);
        assert_eq!(r.last_delivered_at, None);
    }

    /// Recurring advance is the same read-modify-write, so it races the same way:
    /// the winner moves `next_due_at`, and the loser's observed value no longer
    /// matches, so it cannot re-advance the schedule (or re-count the delivery).
    #[tokio::test]
    async fn record_delivery_recurring_second_claim_sees_stale_due_time() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let now = Utc::now();
        let base = probe("daily", now - Duration::minutes(1), None);
        // A genuinely recurring row: `record_delivery` does not read the
        // schedule, but this is the shape the caller hands it a `Some(next)` for.
        let row = NewReminder {
            schedule_kind: crate::models::schedule_kind::PATTERN.to_string(),
            due_at: None,
            freq: Some("daily".to_string()),
            time_of_day: Some("09:00".to_string()),
            ..base
        };
        let id = repo.create(&row).await.unwrap();
        let seen = repo.list_due(now).await.unwrap()[0].next_due_at;
        let next = now + Duration::hours(24);

        assert!(
            repo.record_delivery(&id, Some(next), seen).await.unwrap(),
            "the first claim must be taken"
        );
        assert!(
            !repo.record_delivery(&id, Some(next), seen).await.unwrap(),
            "the stale seen value must lose the claim"
        );

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "pending", "a recurring row stays pending");
        assert_eq!(r.delivered_count, 1, "the loser must not re-count it");
        assert_ne!(
            r.next_due_at, seen,
            "the winner must have advanced the schedule"
        );
        assert!(repo.list_due(now).await.unwrap().is_empty());
        assert_eq!(
            repo.list_due(next + Duration::minutes(1))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The seen value is a guard, not a hint: a caller that observed no due time
    /// must not be able to consume a row that has one. (`do_check_reminders`
    /// skips NULL-`next_due_at` rows before recording, so it never gets here with
    /// `None`; this pins what `None` does mean.)
    #[tokio::test]
    async fn record_delivery_seen_none_does_not_consume_a_row_with_a_due_time() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo
            .create(&probe("one", Utc::now() - Duration::minutes(1), None))
            .await
            .unwrap();

        assert!(
            !repo.record_delivery(&id, None, None).await.unwrap(),
            "an unobserved due time must not claim a row that has one"
        );
        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "pending");
        assert_eq!(r.delivered_count, 0);
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

        let (all, truncated) = repo.list(None, None, 100, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            !truncated,
            "a page larger than the row count must not claim more"
        );
        assert_eq!(
            repo.list(Some("pending"), None, 100, 0)
                .await
                .unwrap()
                .0
                .len(),
            2
        );
        assert_eq!(
            repo.list(Some("pending"), Some("alexandria"), 100, 0)
                .await
                .unwrap()
                .0
                .len(),
            1
        );
        assert_eq!(
            repo.list(Some("cancelled"), None, 100, 0)
                .await
                .unwrap()
                .0
                .len(),
            0
        );
        // The page bound is enforced in SQL, and `truncated` is the signal that a
        // second page exists — the caller cannot infer it from a full page.
        let (first_page, truncated) = repo.list(None, None, 1, 0).await.unwrap();
        assert_eq!(first_page.len(), 1);
        assert!(truncated);
        let (second_page, truncated) = repo.list(None, None, 1, 1).await.unwrap();
        assert_eq!(second_page.len(), 1);
        assert!(!truncated);
        assert_ne!(
            first_page[0].message, second_page[0].message,
            "offset must skip, not repeat"
        );
    }

    #[tokio::test]
    async fn list_pages_with_limit_and_offset() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());
        let base = Utc::now();
        for (i, minute) in [10, 20, 30].into_iter().enumerate() {
            repo.create(&probe(
                &format!("page {i}"),
                base + Duration::minutes(minute),
                None,
            ))
            .await
            .unwrap();
        }

        let (first, truncated) = repo.list(None, None, 2, 0).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(truncated, "a third row exists beyond the page");
        let (second, truncated) = repo.list(None, None, 2, 2).await.unwrap();
        assert_eq!(second.len(), 1);
        assert!(!truncated, "the last page must not claim more");
        assert_ne!(
            first[0].message, second[0].message,
            "START must skip, not repeat"
        );
        assert_eq!(
            repo.list(None, None, 100, 0).await.unwrap().0.len(),
            3,
            "a page wider than the set returns everything"
        );
    }
}
