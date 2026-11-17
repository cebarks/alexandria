//! The account-bound reminder repository.
//!
//! Reminders are the sharpest case in the whole scoped pass, because two of the four surfaces are not
//! reads. `cancel_reminder` and `check_reminders` **consume**: an unscoped claim advances another
//! account's `next_due_at` and bumps their `delivered_count`, so the victim's reminder is silently
//! retired or rescheduled by somebody who was never shown it. A leak that also destroys is worse than a
//! leak that only shows, and `ReminderRepo`'s existing statements carry no owner at all:
//! `list_due`/`list_due_sample` are `SELECT * FROM reminder WHERE status = 'pending' AND next_due_at <=
//! $now`, `cancel` is `UPDATE type::record($id) SET status = 'cancelled'`, and `record_delivery` is
//! guarded only on `status`/`next_due_at`.
//!
//! So every statement here carries `account = $account`, and the two claim paths keep their existing
//! conditional-write guards *in addition* — the account predicate narrows *which* rows may be claimed
//! and the `next_due_at = $seen` guard keeps the claim race-safe. Dropping either would reopen one of
//! the two races.
//!
//! ## Ownership, migration-pending rows, and why there is no stamp
//!
//! Every write here re-asserts its authority inside its own transaction with
//! [`guarded_run`](super::scope::guarded_run) — the same commit predicate the session, memory, graph and
//! cluster writers use. The four reminder surfaces used to be the exception: `set_reminder` is admitted
//! for hosted bearers without a corpus-write grant; explicit targets must still resolve to owned
//! projects. `create`, `stamp_owner`, `record_delivery` and `cancel` were the places where a
//! revocation landing after `admit` could still commit.
//!
//! `stamp_owner` is gone rather than guarded, for that reason: the create statement now carries the
//! `account` stamp itself, so there is no second statement left to interrupt. What remains for Task 10's
//! journal is the *other* half of the request path — `store_memory`'s fact/heat/provenance/cluster/session
//! sequence — and legacy ownership itself, which is Task 14's backfill.
//!
//! `reminder.account` is `option<record<account>>` (v009), so a legacy row has none, and
//! `account = $account` cannot match `NONE`. A migration-pending reminder is therefore invisible to
//! every scoped reader — which is the intended fail-closed answer, not a gap to "heal" by falling back
//! to an unscoped read. Task 14 maps legacy rows onto real projects; until then nothing here may serve
//! them, and `migration_hold` is honoured on top so a row explicitly held out of delivery is not
//! delivered by the scoped path either.
//!
//! ## Targeting
//!
//! New `target_project` inputs must be existing project handles owned by this workspace. They are
//! stored in `project`, never in the legacy free-text field. Unresolved historical text remains
//! pending and suppressed before sampling/consumption; mapping it is Task 14, not name guessing.

use crate::access::OwnerContext;
use crate::error::StorageError;
use crate::models::{ProjectId, Reminder};
use crate::repos::scope::{Bind, GuardedWrite, guarded_run};
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::RecordId;

use super::ScopedProjectRepo;
use super::reminder_repo::{NewReminder, ReminderRepo};

// Shared by discovery and the conditional claim: held/unresolved rows must not cost a sample slot
// or become claimable through a stale read. Legacy text is not authority, even after escalation.
const DELIVERABLE: &str = "(migration_hold = NONE OR migration_hold != true) AND \
    (project != NONE OR (target_project = NONE AND legacy_target_project = NONE)) AND \
    (project = NONE OR project.workspace = $workspace)";

pub struct ScopedReminderRepo<'a> {
    inner: ReminderRepo<'a>,
    owner: OwnerContext,
}

impl<'a> ScopedReminderRepo<'a> {
    pub fn new(db: &'a Surreal<Any>, owner: OwnerContext) -> Self {
        Self {
            inner: ReminderRepo::new(db),
            owner,
        }
    }

    fn account(&self) -> RecordId {
        self.owner.account().as_record_id().clone()
    }

    /// Validate an explicit handle without resolving an alias, name, or filesystem hint.
    pub async fn resolve_target(&self, input: &str) -> Result<ProjectId, StorageError> {
        let project = crate::project_handle::parse_project_handle(input)?;
        Ok(ScopedProjectRepo::new(self.owner_db(), &self.owner)
            .resolve_id(&project)
            .await?
            .id)
    }

    /// Creates a reminder owned by the bound account, in the statement that creates it.
    ///
    /// The `account` stamp comes from the context rather than an argument, so a caller cannot write a
    /// reminder into somebody else's corpus by naming their id — and there is no second statement that
    /// could fail and leave the row unowned. The shape this replaces was `ReminderRepo::create` (which
    /// writes no owner) plus a separate stamp: an interruption between the two left a row nobody could
    /// be served, which is why the stamp used to be aimed at the id the create returned.
    ///
    /// Guarded by [`guarded_run`], so a revocation, suspension or epoch bump landing after the caller's
    /// `admit` and before this commit costs the commit a write conflict rather than creating a row the
    /// caller no longer has authority over.
    pub async fn create(
        &self,
        r: &NewReminder,
        now: DateTime<Utc>,
    ) -> Result<Reminder, StorageError> {
        let project = match r.target_project.as_deref() {
            Some(target) => Some(self.resolve_target(target).await?.as_record_id().clone()),
            None => None,
        };
        let write = GuardedWrite::new(
            "CREATE reminder SET message = $message, project = $project, target_project = NONE, \
             legacy_target_project = NONE, migration_hold = false, prov_project \
             = $prov_project, prov_session_id = $prov_session_id, note = $note, schedule_kind = \
             $schedule_kind, due_at = $due_at, freq = $freq, time_of_day = $time_of_day, weekdays = \
             $weekdays, day_of_month = $day_of_month, cron_expr = $cron_expr, next_due_at = \
             $next_due_at, status = 'pending', delivered_count = 0, account = $account RETURN AFTER;",
        )
        .value("message", Bind::Text(r.message.clone()))
        .value("project", Bind::OptRecord(project))
        .value("prov_project", Bind::OptText(r.prov_project.clone()))
        .value("prov_session_id", Bind::OptText(r.prov_session_id.clone()))
        .value("note", Bind::OptText(r.note.clone()))
        .value("schedule_kind", Bind::Text(r.schedule_kind.clone()))
        .value("due_at", Bind::OptTime(r.due_at))
        .value("freq", Bind::OptText(r.freq.clone()))
        .value("time_of_day", Bind::OptText(r.time_of_day.clone()))
        .value("weekdays", Bind::TextList(r.weekdays.clone()))
        .value("day_of_month", Bind::OptInt(r.day_of_month))
        .value("cron_expr", Bind::OptText(r.cron_expr.clone()))
        .value("next_due_at", Bind::OptTime(r.next_due_at));
        let rows = guarded_run::<Reminder>(self.owner_db(), &self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)?;
        rows.into_iter().next().ok_or_else(missing)
    }

    /// The bound account's reminder for an id, or `None` for a foreign, absent or unparseable one.
    pub async fn get(&self, id: &str) -> Result<Option<Reminder>, StorageError> {
        let Some(rid) = parse(id)? else {
            return Ok(None);
        };
        let mut response = self
            .owner_db()
            .query("SELECT * FROM type::record('reminder', $key) WHERE id = $id AND account = $account LIMIT 1;")
            .bind(("key", key(&rid)?))
            .bind(("id", rid))
            .bind(("account", self.account()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Reminder> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// Every due row **of the bound account**, oldest first.
    ///
    /// Deliberately unbounded, for the same reason the unscoped `list_due` is: this is the delivery
    /// query, and a row it returns but does not render would be consumed invisibly. The bound variant is
    /// [`Self::due_sample`].
    ///
    /// `migration_hold` is excluded, so a row held out of delivery by the Task 14 migration cannot be
    /// delivered by a scoped caller while every other pending row of theirs can.
    pub async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Reminder>, StorageError> {
        let mut response = self
            .owner_db()
            .query(format!(
                "SELECT * FROM reminder WHERE account = $account AND status = 'pending' \
                 AND next_due_at <= $now AND next_due_at != NONE AND {DELIVERABLE} \
                 ORDER BY next_due_at ASC, id ASC;"
            ))
            .bind(("workspace", self.owner.workspace().as_record_id().clone()))
            .bind(("account", self.account()))
            .bind(("now", now))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// At most `cap` of the bound account's due rows, oldest first.
    ///
    /// The `LIMIT` is in SQL and the account predicate is before it, so the informational piggyback that
    /// runs on every memory lookup reads a bounded slice of *this caller's* rows rather than a global
    /// slice filtered afterwards — the latter would let another account's overdue volume push this
    /// caller's reminders out of their own sample.
    ///
    /// `next_due_at != NONE` is load-bearing exactly as it is on the unscoped path: `NONE` sorts below
    /// every datetime and `next_due_at <= $now` selects such a row, so a row with no due time would sit
    /// at the head of the sample permanently. `IS NOT NULL` and `!= NULL` are both satisfied by `NONE`
    /// on this engine and filter nothing.
    pub async fn due_sample(
        &self,
        now: DateTime<Utc>,
        cap: i64,
    ) -> Result<Vec<Reminder>, StorageError> {
        let mut response = self
            .owner_db()
            .query(format!(
                "SELECT * FROM reminder WHERE account = $account AND status = 'pending' \
                 AND next_due_at <= $now AND next_due_at != NONE AND {DELIVERABLE} \
                 ORDER BY next_due_at ASC, id ASC LIMIT $cap;"
            ))
            .bind(("workspace", self.owner.workspace().as_record_id().clone()))
            .bind(("account", self.account()))
            .bind(("now", now))
            .bind(("cap", cap))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// A page of the bound account's reminders, plus whether more exist.
    pub async fn list(
        &self,
        status: Option<&str>,
        target_project: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Reminder>, bool), StorageError> {
        let mut conditions = vec!["account = $account".to_string()];
        if status.is_some() {
            conditions.push("status = $status".to_string());
        }
        let project = match target_project {
            Some(target) => {
                conditions.push("project = $project".to_string());
                Some(self.resolve_target(target).await?.as_record_id().clone())
            }
            None => None,
        };
        let mut q = self
            .owner_db()
            .query(format!(
                "SELECT * FROM reminder WHERE {} ORDER BY next_due_at ASC, id ASC LIMIT $limit \
                 START $offset;",
                conditions.join(" AND ")
            ))
            .bind(("account", self.account()))
            .bind(("status", status.map(str::to_string)))
            .bind(("project", project))
            .bind(("limit", limit + 1))
            .bind(("offset", offset.max(0)));
        let _ = &mut q;
        let mut response = q.await.map_err(internal)?.check().map_err(internal)?;
        let mut reminders: Vec<Reminder> = response.take(0).map_err(internal)?;
        let truncated = reminders.len() as i64 > limit;
        if truncated {
            reminders.pop();
        }
        Ok((reminders, truncated))
    }

    /// Claims a delivery **of the bound account's own row**, inside the transaction that re-asserts the
    /// caller's authority.
    ///
    /// Three guards, and each is necessary. `account = $account` says whose rows may be claimed.
    /// `status = 'pending' AND next_due_at = $seen` says the row still looks exactly as this caller read
    /// it — without it, two overlapping `check_reminders` calls both deliver one row, and a `cancel`
    /// landing after the read gets clobbered into `delivered`. And the [`guarded_run`] claim says the
    /// caller is still who `admit` said they were: a token revoked between the read and this commit loses
    /// the commit rather than consuming a row it can no longer deliver. `Err(Stale)` is that refusal, and
    /// it is deliberately not `Ok(false)` — "your authority is gone" and "somebody else won this row" are
    /// different answers.
    pub async fn record_delivery(
        &self,
        id: &str,
        new_next_due_at: Option<DateTime<Utc>>,
        seen_next_due_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<bool, StorageError> {
        // An unparseable id claims nothing, the same way an absent or foreign one does.
        let Some(rid) = parse(id)? else {
            return Ok(false);
        };
        let set_clause = if new_next_due_at.is_some() {
            "last_delivered_at = time::now(), delivered_count += 1, next_due_at = $next"
        } else {
            "last_delivered_at = time::now(), delivered_count += 1, status = 'delivered'"
        };
        let sql = format!(
            "UPDATE type::record('reminder', $key) SET {set_clause} WHERE id = $id AND account = \
             $account AND status = 'pending' AND next_due_at = $seen AND next_due_at != NONE \
             AND {DELIVERABLE} RETURN AFTER;"
        );
        let mut write = GuardedWrite::new(sql)
            .id("id", rid.clone())
            .value("key", Bind::Text(key(&rid)?))
            .value("seen", Bind::OptTime(seen_next_due_at));
        if let Some(next) = new_next_due_at {
            write = write.value("next", Bind::Time(next));
        }
        let updated = guarded_run::<Reminder>(self.owner_db(), &self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)?;
        Ok(updated.len() == 1)
    }

    /// Soft-cancels the bound account's own reminder, guarded inside its own transaction.
    ///
    /// Returns whether a row was written, so the caller cannot report success for a reminder it did not
    /// own: the unscoped `cancel` returned `Ok(())` for any id, which is the shape
    /// `cancelling_a_foreign_reminder_is_refused_and_changes_nothing` is red about. The claim is what
    /// closes the window between `admit` and this commit — `Err(Stale)`, not a quiet `Ok(false)`, because
    /// a revoked bearer must not be handed an answer that reads like "it was already gone".
    pub async fn cancel(&self, id: &str, now: DateTime<Utc>) -> Result<bool, StorageError> {
        // An unparseable id writes nothing, the same way an absent or foreign one does.
        let Some(rid) = parse(id)? else {
            return Ok(false);
        };
        let write = GuardedWrite::new(
            "UPDATE type::record('reminder', $key) SET status = 'cancelled', cancelled_at = $now \
             WHERE id = $id AND account = $account RETURN AFTER;",
        )
        .id("id", rid.clone())
        .value("key", Bind::Text(key(&rid)?))
        .value("now", Bind::Time(now));
        let updated = guarded_run::<Reminder>(self.owner_db(), &self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)?;
        Ok(!updated.is_empty())
    }

    fn owner_db(&self) -> &Surreal<Any> {
        self.inner.db()
    }
}

/// The reminder record a caller-supplied id names, or `None` when the text is not one record
/// literal at all — an unparseable id is skipped exactly like a foreign or absent one, so the read
/// and claim layers' answer shapes never reveal which of the three it was. A parsed id in the wrong
/// table is still `InvalidInput`: that is a well-formed handle aimed at the wrong boundary, which
/// the request path reports rather than silently treating as absent.
fn parse(id: &str) -> Result<Option<RecordId>, StorageError> {
    let Some(rid) = crate::parse_record_id(id).ok() else {
        return Ok(None);
    };
    if rid.table.as_str() != "reminder" {
        return Err(StorageError::InvalidInput(
            "malformed reminder id".to_string(),
        ));
    }
    Ok(Some(rid))
}

/// The record key alone, bound as a parameter — `record_id_to_string` is not a parser-safe SQL round
/// trip for numeric or quoted opaque keys.
fn key(id: &RecordId) -> Result<String, StorageError> {
    use surrealdb::types::RecordIdKey;
    let text = match &id.key {
        RecordIdKey::String(text) => text.clone(),
        RecordIdKey::Number(number) => number.to_string(),
        RecordIdKey::Uuid(uuid) => uuid.to_string(),
        other => format!("{other:?}"),
    };
    if text.is_empty() {
        return Err(StorageError::Internal(anyhow::anyhow!(
            "reminder id has an empty key"
        )));
    }
    Ok(text)
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

fn missing() -> StorageError {
    StorageError::Internal(anyhow::anyhow!("reminder row was not returned"))
}
