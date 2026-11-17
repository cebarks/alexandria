//! The account-bound session repository: the reader half of the v010 handle swap.
//!
//! v010 removed the global `session_external_id_idx` and defined
//! `session_account_external_id_idx ON session FIELDS account, external_id UNIQUE`. That index is only
//! safe to have if every statement that touches a handle carries the account with it, because a store
//! with a qualified index and unqualified readers *resolves* another owner's session and then *writes*
//! it — which is exactly what the two red tests in `scoped_session_race_test.rs` demonstrate against
//! the old [`SessionRepo`]. This is that reader half.
//!
//! Why a separate type rather than a flag on [`SessionRepo`]: an `Option<OwnerContext>` that some
//! methods consult and others forget is the same leak with a `None` branch attached. A
//! [`ScopedSessionRepo`] value *is* the proof an account is bound, and there is no way to build one
//! without it. [`SessionRepo`] survives as the interim unscoped path; see that type's own documentation
//! for which consumers still use it, what v010 costs that path, and why the rest of the migration is an
//! open owner decision rather than an oversight.
//!
//! ## Every write is guarded inside its own transaction
//!
//! `touch`, `finalize` and the `find_or_create` fill do not re-check authority with a separate `SELECT`
//! beforehand. A guarded *read* is not atomic on this engine: measured on the pinned 3.3.0 build, a
//! revoke committed by another writer between the read and the commit silently won and the data write
//! persisted over a dead credential. What does hold — and what Task 4's one-use claims already rely on —
//! is a **conditional write on the authorization row itself, in the same transaction as the data
//! write**: the commit then fails with a write conflict and the data write rolls back. That is
//! [`ScopedSessionRepo::guard_then_write`], and
//! `scoped_repos_test::a_credential_revoked_mid_transaction_does_not_commit_a_scoped_write` pins it.
//!
//! The predicate is the whole Task 5 carry: account `status`, the account's current `auth_epoch`, and
//! for a hosted context that *specific* credential's ownership, `revoked`, `consumed`, `expires_at`,
//! `absolute_expires_at` and minted epoch. The credential id and the epoch are the ones
//! `AccessRepo::authorize` read off live rows when it issued this context — they are not arguments, and
//! there is no "any live credential for this account" substitution, which would let a revoked credential
//! hide behind a sibling.

use crate::access::{OwnerContext, ScopedContext};
use crate::error::StorageError;
use crate::models::{Fact, Session, SessionListItem};
use crate::repos::scope::{Bind, GuardedWrite, ScopeFilter, guarded_run, record_key};

use chrono::Utc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// The live-fact predicate, spelled once.
///
/// `quarantined_at = NONE` and never `IS NOT NULL` or `!= NULL`: on this engine both of those are
/// *satisfied by* `NONE` and so filter nothing at all.
const LIVE_FACT: &str = "deleted = false AND quarantined_at = NONE";

/// Which of the two session enumerations a read is.
///
/// A type rather than a `bool` because the two views differ in *two* ways (projected columns and the
/// membership filter) and a flag named for one of them invites a caller to change the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProjection {
    /// The caller's scope: no `summary`, no `tags`, and counts inside the resolved population.
    Scoped,
    /// The account-private whole-session read: prose included, across the caller's owned scopes.
    Archive,
}

/// One page of the caller's own session list: the filters, the resolved population, and the projection.
///
/// A struct rather than seven positional parameters because each value here is a *correctness* choice
/// that a transposition would silently change — `membership` and `projection` decide what a caller is
/// allowed to see, and the three filters are ANDed with the bound account before `LIMIT`/`START`, so a
/// page is this owner's page rather than a global page filtered afterwards. Fields are owned so a query
/// can outlive the request params it was built from; a page is built once and issued once.
///
/// `Debug` is derived, and that is deliberate: the three filter values are the caller's own words, and
/// [`ScopeFilter`] prints its shape rather than its record ids, so a rendered query cannot name a
/// resolved partition. There is deliberately no `Default`: an unstated `membership` selects *nothing*,
/// which is a page a caller never asked for rather than an empty one.
#[derive(Debug, Clone)]
pub struct SessionPageQuery {
    pub agent_id: Option<String>,
    /// Only honored by [`SessionProjection::Archive`]; the MCP layer refuses it on a scoped page, because
    /// the tag set is account-private prose that view withholds.
    pub tag: Option<String>,
    pub finalized: Option<bool>,
    /// The resolved population that membership and live counts are filtered to.
    pub membership: ScopeFilter,
    /// Whether the account-private prose is part of this answer.
    pub projection: SessionProjection,
    pub limit: usize,
    pub offset: usize,
}

/// What one guarded write concluded.
///
/// The two empty outcomes are not one event and must not be reported as one:
///
/// * [`Guard::Refused`] — the authorization predicate matched no row, so the caller's authority is gone
///   (revoked, consumed, expired, superseded by an epoch bump, or the account suspended). The honest
///   answer is `Stale`: "authenticate again", and nothing about anybody else's rows.
/// * [`Guard::Unmatched`] — the authority was fine and the data statement's own `WHERE` matched
///   nothing, which on these paths means the row this repo resolved moments ago is no longer there.
///
/// This is also what keeps `finalize`'s `Ok(None)` from being ambiguous: a revoked bearer must not be
/// handed a success-shaped answer, so a refusal is an error and only a vanished row is `Ok(None)`.
pub(crate) enum Guard {
    Refused,
    Unmatched,
    Written(Vec<Session>),
}

/// One row of the owner's archive projection.
///
/// No memory body is part of this shape *by construction*: the archive is the owner's prose about a
/// session, and the only thing it says about content is how much of it is still live.
#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct SessionArchiveEntry {
    pub external_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub live_memory_count: i64,
    /// Which partitions this session's live memories currently sit in, deduplicated and sorted.
    ///
    /// A session is the one population in this design that is *allowed* to be mixed: memories stored during
    /// one conversation can belong to several partitions, and a scope move leaves the session link where it
    /// was. So the archive — the only view that shows this row's prose — says so on its face, and a reader
    /// who then opens a single project's view is told by that view's silence that the prose is not there.
    /// This is the label, not a filter: it names partitions, never a memory body, and it is derived from
    /// the same live traversal as `live_memory_count` so the two cannot disagree about what is in the
    /// session.
    pub scopes: Vec<String>,
}

pub struct ScopedSessionRepo<'a> {
    db: &'a Surreal<Any>,
    owner: OwnerContext,
}

impl<'a> ScopedSessionRepo<'a> {
    /// Binds the repo to one account for its whole life.
    ///
    /// `OwnerContext` is sealed and its only producer is `AccessRepo::authorize`, so this is the only
    /// way in, and it is not reachable from a request payload: there is no `Default`, no
    /// `from_account_id`, and no test-only constructor.
    pub fn new(db: &'a Surreal<Any>, owner: OwnerContext) -> Self {
        Self { db, owner }
    }

    pub fn owner(&self) -> &OwnerContext {
        &self.owner
    }

    fn account(&self) -> RecordId {
        self.owner.account().as_record_id().clone()
    }

    fn workspace(&self) -> RecordId {
        self.owner.workspace().as_record_id().clone()
    }

    /// The bound account's session for a handle, or `None`.
    ///
    /// A foreign row under the same handle is `None` — the identical answer to an absent one, so the
    /// outward response cannot be used to probe which account holds a given handle.
    pub async fn find_by_external_id(
        &self,
        external_id: &str,
    ) -> Result<Option<Session>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM `session` WHERE account = $account AND external_id = $external_id \
                 LIMIT 1;",
            )
            .bind(("account", self.account()))
            .bind(("external_id", external_id.to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Session> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// Creates the bound account's session **inside the transaction that re-asserts its authority**,
    /// stamping the account and workspace from the context — neither is a parameter, so a caller
    /// cannot write a session into somebody else's namespace.
    ///
    /// A `CREATE` has no `WHERE` to hang a guard on, so the atomicity comes from the other direction:
    /// the conditional write on the authorization row and this statement share one transaction, which
    /// is what makes a revoke landing in between cost the commit an engine write conflict instead of
    /// leaving a row behind. That is the same shape [`ScopedMemoryRepo::guarded_create`] uses for a
    /// fact, and for the same reason — Task 6's fifth lane moved every *other* session write behind
    /// the claim, and this was the one that remained, reached from `store_memory` whenever a caller
    /// opens a handle nobody has used yet. `scoped_repos_test`'s
    /// `a_revoked_credential_stops_an_implicit_session_create` is the behavioral pin, and
    /// `a_credential_revoked_mid_transaction_does_not_commit_a_session_create` is the engine proof.
    ///
    /// The error distinguishes the two failure kinds on purpose, because `find_or_create`'s recovery
    /// may only be entered by the first: a lost authority (`Stale`) must never be answered by
    /// re-reading and adopting *some* row.
    async fn create(
        &self,
        external_id: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<Session, StorageError> {
        // Each optional column is named and bound in the same arm, so the pair cannot drift the way an
        // `is_some()` test with a later `expect()` can.
        let mut sets = vec![
            "external_id = $external_id".to_string(),
            "account = $account".to_string(),
            "workspace = $workspace".to_string(),
            "tags = []".to_string(),
        ];
        let mut binds: Vec<(&str, Bind)> = Vec::new();
        if let Some(agent_id) = agent_id {
            sets.push("agent_id = $agent_id".to_string());
            binds.push(("agent_id", Bind::Text(agent_id.to_string())));
        }
        if let Some(model) = model {
            sets.push("model = $model".to_string());
            binds.push(("model", Bind::Text(model.to_string())));
        }
        let write = GuardedWrite::new(format!("CREATE `session` SET {};", sets.join(", ")))
            .value("external_id", Bind::Text(external_id.to_string()))
            .with_values(binds);
        let created = guarded_run::<Session>(self.db, &self.owner, &write, Utc::now())
            .await?
            .ok_or(StorageError::Stale)?;
        created.into_iter().next().ok_or_else(|| missing("session"))
    }

    /// Find the bound account's session for a handle, creating it when missing.
    ///
    /// **The unique-error recovery read is account-bound too**, and that is the whole point of this
    /// shape. Under the global index the loser of a race was handed *whoever's* row collided. Here the
    /// collided create is re-read under the bound account, so a foreign row sharing the handle cannot
    /// satisfy it; if the re-read finds nothing, the collision really was another account's row and the
    /// original error stands rather than a second account's session being returned. The caller is told
    /// nothing about who else holds that handle either way.
    ///
    /// `agent_id` / `model` are set on create and fill still-empty fields on an existing row; a stored
    /// value is never overwritten. The fill is a guarded write, not a bare update.
    pub async fn find_or_create(
        &self,
        external_id: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<Session, StorageError> {
        let session = match self.find_by_external_id(external_id).await? {
            Some(session) => session,
            None => match self.create(external_id, agent_id, model).await {
                Ok(session) => return Ok(session),
                // A lost authority is the one failure that must not fall through to the recovery
                // read: re-resolving the handle after a refused create could only adopt a row this
                // context did not write, which is the shape Task 6's review called `Stale`-as-success.
                Err(stale @ StorageError::Stale) => return Err(stale),
                // A unique violation on `(account, external_id)` is the raced second store, and the
                // recovery read is account-bound, so it can only find this account's own row.
                Err(conflict) => self
                    .find_by_external_id(external_id)
                    .await?
                    .ok_or(conflict)?,
            },
        };

        let fill_agent = session.agent_id.is_none().then_some(agent_id).flatten();
        let fill_model = session.model.is_none().then_some(model).flatten();
        if fill_agent.is_some() || fill_model.is_some() {
            let mut sets = Vec::new();
            let mut binds = Vec::new();
            if let Some(agent_id) = fill_agent {
                sets.push("agent_id = agent_id ?? $agent_id");
                binds.push(("agent_id", Bind::Text(agent_id.to_string())));
            }
            if let Some(model) = fill_model {
                sets.push("model = model ?? $model");
                binds.push(("model", Bind::Text(model.to_string())));
            }
            let rows = self
                .guard_then_write(
                    &format!(
                        "UPDATE `session` SET {} WHERE id = $id AND account = $account RETURN AFTER;",
                        sets.join(", ")
                    ),
                    vec![("id", session_id(&session)?)],
                    binds,
                )
                .await?;
            match rows {
                Guard::Refused => return Err(StorageError::Stale),
                // The row this repo just resolved stopped matching its own guard mid-write; either way
                // the fill is best-effort metadata and the session itself is the answer.
                Guard::Unmatched | Guard::Written(_) => {}
            }
        }
        Ok(session)
    }

    /// Record activity on the bound account's session by refreshing `ended_at`.
    ///
    /// Aimed at the *typed id* of the row this repo resolved under the bound account — the form the
    /// brief allows as equivalent to an account-qualified handle update — with `account = $account`
    /// still in the `WHERE`, so an id derived anywhere else cannot land.
    pub async fn touch(&self, external_id: &str) -> Result<Session, StorageError> {
        let session = self.bound_session(external_id).await?;
        let rows = self
            .guard_then_write(
                "UPDATE `session` SET ended_at = time::now() WHERE id = $id AND account = $account \
                 RETURN AFTER;",
                vec![("id", session_id(&session)?)],
                vec![],
            )
            .await?;
        match rows {
            Guard::Refused => Err(StorageError::Stale),
            Guard::Unmatched => Err(StorageError::NotFound),
            Guard::Written(rows) => rows.into_iter().next().ok_or(StorageError::NotFound),
        }
    }

    /// Finalize the bound account's session: `ended_at`, plus the summary and tags when supplied.
    ///
    /// A foreign handle is [`StorageError::NotFound`], the same outward answer as a missing one. A
    /// handle that is the account's own but whose authorization died mid-flight is
    /// [`StorageError::Stale`] — different conclusion, and neither names another account.
    pub async fn finalize(
        &self,
        external_id: &str,
        summary: Option<&str>,
        tags: Option<&[String]>,
    ) -> Result<Option<Session>, StorageError> {
        let session = self.bound_session(external_id).await?;
        let mut sets = vec!["ended_at = time::now()".to_string()];
        let mut binds = Vec::new();
        // The value is moved out of the `Option` rather than re-`expect`ed after an `is_some` test, so
        // the guard and the bind cannot drift apart.
        if let Some(summary) = summary {
            sets.push("summary = $summary".to_string());
            binds.push(("summary", Bind::Text(summary.to_string())));
        }
        if let Some(tags) = tags {
            sets.push("tags = $tags".to_string());
            binds.push(("tags", Bind::TextList(tags.to_vec())));
        }
        let rows = self
            .guard_then_write(
                &format!(
                    "UPDATE `session` SET {} WHERE id = $id AND account = $account RETURN AFTER;",
                    sets.join(", ")
                ),
                vec![("id", session_id(&session)?)],
                binds,
            )
            .await?;
        match rows {
            // A refusal is `Stale`, never `Ok(None)`: "nothing changed" reads as success, and a revoked
            // bearer must not receive one.
            Guard::Refused => Err(StorageError::Stale),
            Guard::Unmatched => Ok(None),
            Guard::Written(rows) => Ok(rows.into_iter().next()),
        }
    }

    /// Link one of the bound account's live memories to one of its sessions, **guarded**.
    ///
    /// **Both ends are validated under this context before the edge is written**, inside the same
    /// transaction as the authority claim. An edge is a membership claim; a claim from a session this
    /// account owns to a fact it does not is exactly the cross-owner structure the scoped readers cannot
    /// see and the graph traversals will happily fold. Either end failing its own ownership read is
    /// `NotFound`, and nothing is related. The claim is in the transaction for the same reason every
    /// other write here has one: a revocation landing after `admit` has to cost the *edge*, not be
    /// committed over.
    pub async fn add_memory(&self, external_id: &str, fact_id: &str) -> Result<(), StorageError> {
        let session = self.bound_session(external_id).await?;
        let fact = self
            .bound_fact(fact_id)
            .await?
            .ok_or(StorageError::NotFound)?;
        let write = GuardedWrite::new("RELATE $sess->contains_session_memory->$fact;")
            .id("sess", session_id(&session)?)
            .id("fact", fact.id.ok_or_else(|| missing("fact"))?);
        guarded_run::<serde_json::Value>(self.db, &self.owner, &write, Utc::now())
            .await?
            .ok_or(StorageError::Stale)?;
        Ok(())
    }

    /// The bound account's live memories for a handle, oldest first.
    ///
    /// Anchored on the *resolved row id* rather than on the handle, so it cannot follow a foreign
    /// session's edges, and the fact side carries the live predicate and the bound workspace inside the
    /// traversal target's parentheses — the only spelling this engine parses for a filtered traversal.
    pub async fn get_memories(&self, external_id: &str) -> Result<Vec<Fact>, StorageError> {
        let session = match self.find_by_external_id(external_id).await? {
            Some(session) => session,
            // A handle this account does not own has no memories to return, and the answer is the same
            // as an empty own-session: never a foreign row's contents.
            None => return Ok(vec![]),
        };
        let mut response = self
            .db
            .query(format!(
                "SELECT * FROM $sess->contains_session_memory->fact WHERE {LIVE_FACT} AND \
                 workspace = $workspace ORDER BY created_at ASC;"
            ))
            .bind(("sess", session_id(&session)?))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The bound account's live memories for a handle, **restricted to one resolved read population**.
    ///
    /// This is the scoped session view, and the reason [`Self::get_memories`] is not enough: that read is
    /// account- and workspace-bound, so a request whose caller scope is Personal would still see the
    /// session's project rows. Task 8's `view: "scoped"` needs the intersection the design promises —
    /// "recall constrained by a session still intersects the selected memory scope" — and the only honest
    /// way to express it is the same [`ScopeFilter`] the fact reads use, spliced into the traversal
    /// target's own `WHERE` so the filter is applied *before* any ordering, not after a page is cut.
    ///
    /// An empty population selects nothing rather than everything, exactly as in [`ScopeFilter::new`]: a
    /// caller resolved to no partitions reads no memories, not the whole session.
    pub async fn memories_in(
        &self,
        external_id: &str,
        context: &ScopedContext,
    ) -> Result<Vec<Fact>, StorageError> {
        let filter = ScopeFilter::new(context);
        if filter.is_empty() {
            return Ok(vec![]);
        }
        let session = match self.find_by_external_id(external_id).await? {
            Some(session) => session,
            None => return Ok(vec![]),
        };
        let query = format!(
            "SELECT * FROM $sess->contains_session_memory->(fact WHERE {LIVE_FACT} AND {}) ORDER \
             BY created_at ASC;",
            filter.conjunct()
        );
        let mut response = filter
            .apply(
                self.db
                    .query(query)
                    .bind(("sess", session_id(&session)?))
                    .bind(("workspace", self.workspace())),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The caller's own session list, newest activity first, each row counted **inside the resolved
    /// read population** and projected with or without the account-private prose.
    ///
    /// Two things the unscoped shape could not express and Task 8 needs:
    ///
    /// * **The count is a scoped count.** It used to be `workspace = $workspace`, which counted every
    ///   live row of the caller's workspace — including Unassigned rows that no recall path may reach.
    ///   `membership` is the same [`ScopeFilter`] the fact reads use, so a session's count is exactly
    ///   the memories that caller could read under the scope they asked for, and an Unassigned row is
    ///   invisible until the caller names the review partition.
    /// * **The prose is a separate decision from the row.** `summary`/`tags` are the owner's historical
    ///   prose about a session, not project content, so a project-context enumeration omits them
    ///   ([`SessionProjection::Scoped`]) and the deliberate whole-session read includes them
    ///   ([`SessionProjection::Archive`]). They are *not* re-derived from a partial row: the columns are
    ///   not projected at all in the scoped shape, and cleared here as well, so a stale value cannot
    ///   survive a reused struct.
    ///
    /// As before, the account predicate sits in the `WHERE` before `LIMIT`/`START`, so a page is this
    /// owner's page rather than a global page filtered afterwards, and `ORDER BY ended_at DESC,
    /// external_id ASC` stays a total order because `(account, external_id)` is unique (v010).
    pub async fn list_scoped(
        &self,
        page: &SessionPageQuery,
    ) -> Result<Vec<SessionListItem>, StorageError> {
        let SessionPageQuery {
            agent_id,
            tag,
            finalized,
            membership,
            projection,
            limit,
            offset,
        } = page;
        let agent_id = agent_id.as_deref();
        let tag = tag.as_deref();
        let scoped = projection == &SessionProjection::Scoped;
        // Every filter is ANDed with the bound account, and the membership predicate is the resolved
        // population's own conjunct — so the count and the row set agree with each other and with what
        // the same caller's fact reads return.
        let mut filters = vec!["account = $account".to_string()];
        if agent_id.is_some() {
            filters.push("agent_id = $agent_id".to_string());
        }
        if tag.is_some() {
            filters.push("$tag IN tags".to_string());
        }
        // `!= NONE` is the tested spelling for "has a summary": on a SCHEMAFULL table an absent field
        // reads back as null, and `IS NOT NULL` reports every session as finalized.
        if finalized.is_some() {
            filters.push("(summary != NONE) = $finalized".to_string());
        }
        // `finalized` is a projected column, computed by the statement before anything is cleared. The
        // page's badge needs the discriminator and nothing else: `summary.is_some()` is the only honest
        // test for "a caller summarized this" — `touch()` moves `ended_at` on every attached memory — and
        // withholding the prose must not also withhold the fact.
        //
        // The rest of the reason is a real constraint rather than a style choice: `SessionListItem` derives
        // `SurrealValue`, and a row
        // projected without `tags` fails to decode at all — so an omitted column here would silently
        // empty the page rather than withhold the field. Withholding therefore happens on the decoded
        // value, immediately after the read and before anything can look at it.
        let mut q = self.db.query(format!(
            "SELECT *,              (->contains_session_memory->(fact WHERE {LIVE_FACT} AND {membership})).len() AS              memory_count,              (summary != NONE) AS finalized              FROM `session` WHERE {}              ORDER BY ended_at DESC, external_id ASC LIMIT $limit START $offset;",
            filters.join(" AND "),
            membership = membership.conjunct()
        ));
        q = membership.apply(
            q.bind(("account", self.account()))
                .bind(("agent_id", agent_id.map(str::to_string)))
                .bind(("tag", tag.map(str::to_string)))
                .bind(("finalized", *finalized))
                .bind(("limit", *limit as i64))
                .bind(("offset", *offset as i64)),
        );
        let mut response = q.await.map_err(internal)?.check().map_err(internal)?;
        let mut rows: Vec<SessionListItem> = response.take(0).map_err(internal)?;
        if scoped {
            for row in &mut rows {
                row.summary = None;
                row.tags.clear();
            }
        }
        Ok(rows)
    }

    /// The owner's archive projection: sessions with a stored summary, including ones whose memories
    /// have since been soft-deleted or quarantined.
    ///
    /// A different method rather than a flag on [`Self::list_scoped`], because the two answer different
    /// questions and only one of them is allowed to carry historical prose. The archive preserves that
    /// prose (`summary`, `tags`) and exposes **no memory body at all** — the one number it reports is
    /// how many memories are *still live*, so an archived row cannot leak a body the live path hides.
    /// Migration-pending (unowned) rows are unreachable here for the same reason they are unreachable
    /// everywhere: `account = $account` cannot match `NONE`.
    pub async fn archive(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionArchiveEntry>, StorageError> {
        let mut response = self
            .db
            .query(format!(
                "SELECT external_id, agent_id, model, started_at, ended_at, summary, tags, \
                 (->contains_session_memory->(fact WHERE {LIVE_FACT} AND workspace = $workspace)) \
                 .len() AS live_memory_count, \
                 (->contains_session_memory->(fact WHERE {LIVE_FACT} AND workspace = $workspace)) \
                 .scope AS scopes \
                 FROM `session` WHERE account = $account AND summary != NONE \
                 ORDER BY ended_at DESC, external_id ASC LIMIT $limit START $offset;"
            ))
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let mut rows: Vec<SessionArchiveEntry> = response.take(0).map_err(internal)?;
        // Deduplicated in Rust rather than in the statement: `array::distinct` over a traversal that can
        // return nothing is a second shape to test, and a handful of one owner's own scope labels is not
        // worth an engine-specific projection. Sorted, so two renders of one session agree.
        for row in &mut rows {
            row.scopes.sort();
            row.scopes.dedup();
        }
        Ok(rows)
    }

    /// How many of the caller's sessions carry a summary — the dashboard's finalized/idle split.
    ///
    /// `summary != NONE` is the same discriminator `list_scoped`'s `finalized` filter uses and for the
    /// same reason: on this engine an absent field reads `NONE`, and `IS NOT NULL` / `!= NULL` are both
    /// satisfied by it, so a "has a summary" test written either of those ways reports every session.
    /// `ended_at` is not the discriminator and never will be: `touch()` moves it on every attached
    /// memory, so a live idle session has one too.
    pub async fn count_finalized(&self) -> Result<usize, StorageError> {
        #[derive(SurrealValue)]
        struct CountRow {
            count: i64,
        }
        let mut response = self
            .db
            .query(
                "SELECT count() FROM `session` WHERE account = $account AND summary != NONE GROUP                  ALL;",
            )
            .bind(("account", self.account()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<CountRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .first()
            .map(|row| row.count.max(0) as usize)
            .unwrap_or(0))
    }

    /// One owner's session count. Never a table-wide count.
    pub async fn count(&self) -> Result<usize, StorageError> {
        #[derive(SurrealValue)]
        struct CountRow {
            count: i64,
        }
        let mut response = self
            .db
            .query("SELECT count() FROM `session` WHERE account = $account GROUP ALL;")
            .bind(("account", self.account()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<CountRow> = response.take(0).map_err(internal)?;
        Ok(rows.first().map(|row| row.count as usize).unwrap_or(0))
    }

    /// The bound account's session for a handle, refusing instead of returning `None`.
    ///
    /// Used by every write path so the id a write aims at is always one this context resolved.
    async fn bound_session(&self, external_id: &str) -> Result<Session, StorageError> {
        self.find_by_external_id(external_id)
            .await?
            .ok_or(StorageError::NotFound)
    }

    /// Whether `fact_id` is a live fact inside the bound account's own workspace.
    async fn bound_fact(&self, fact_id: &str) -> Result<Option<Fact>, StorageError> {
        let id = parse_id(fact_id, "fact")?;
        let mut response = self
            .db
            .query(format!(
                "SELECT * FROM type::record('fact', $key) WHERE account = $account AND workspace = \
                 $workspace AND {LIVE_FACT} LIMIT 1;"
            ))
            .bind(("key", record_key(&id)?))
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Fact> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// One guarded write: claim the authorization row, write the data row, commit.
    ///
    /// `data_sql` must carry `AND account = $account` in its `WHERE` — the bound account is bound by
    /// [`guarded_run`], always, so no call site can drop it — and may name the other binds in `binds`.
    /// `$id` is bound from `targets`, which is how a write aimed at a specific record stays aimed at it.
    ///
    /// The retry lives in [`guarded_run`] and covers the case a sequential pre-check cannot see — a
    /// concurrent writer changed the authorization row between our claim and our commit — which the
    /// engine reports as a write conflict on commit rather than as a lost update. Retrying re-issues the
    /// claim, so the second attempt sees the dead authority and reports [`Guard::Refused`]. Classification
    /// is by `QueryError::TransactionConflict`, never by message matching.
    async fn guard_then_write(
        &self,
        data_sql: &str,
        targets: Vec<(&str, RecordId)>,
        binds: Vec<(&str, Bind)>,
    ) -> Result<Guard, StorageError> {
        let mut write = GuardedWrite::new(data_sql);
        for (name, id) in targets {
            write = write.id(name, id);
        }
        for (name, bind) in binds {
            write = write.value(name, bind);
        }
        let rows = guarded_run(self.db, &self.owner, &write, Utc::now()).await?;
        Ok(match rows {
            // The authorization predicate matched no row: the caller's authority is gone, and nothing
            // about anybody else's rows is the honest answer.
            None => Guard::Refused,
            Some(rows) if rows.is_empty() => Guard::Unmatched,
            Some(rows) => Guard::Written(rows),
        })
    }
}

/// The id a session write aims at, from a row this repo resolved.
fn session_id(session: &Session) -> Result<RecordId, StorageError> {
    session
        .id
        .clone()
        .ok_or_else(|| StorageError::Internal(anyhow::anyhow!("session row has no id")))
}

/// Parses a caller-supplied `table:key` handle. A malformed handle is `InvalidInput`, not `NotFound`:
/// the request was wrong, which is a different conclusion from "no such row".
fn parse_id(text: &str, table: &str) -> Result<RecordId, StorageError> {
    let id = crate::parse_record_id(text)
        .map_err(|_| StorageError::InvalidInput(format!("malformed {table} id")))?;
    if id.table.as_str() != table {
        return Err(StorageError::InvalidInput(format!(
            "expected a record of table `{table}`"
        )));
    }
    Ok(id)
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

fn missing(what: &str) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("{what} row has no id"))
}
