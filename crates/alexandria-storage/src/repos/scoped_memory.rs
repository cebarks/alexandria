//! The account-bound fact and raw-document writes a request path needs.
//!
//! This module exists because v010 cannot ship apart from its readers, and a session re-key cannot ship
//! apart from the *fact* ownership that makes membership edges meaningful:
//!
//! * [`ScopedSessionRepo::add_memory`](super::ScopedSessionRepo::add_memory) validates both ends of a
//!   membership edge, and the fact end is "a live fact of this account's workspace". A fact written
//!   without its `account`/`workspace`/`scope` columns therefore fails that check — which is correct,
//!   and which is exactly why the store path that creates the fact has to stamp them in the same
//!   checkpoint. An unowned fact is migration-pending, and migration-pending data is invisible to every
//!   scoped reader.
//! * `scope` is not decoration. It is the partition activation derives from (
//!   [`super::scoped_activation::focal_partition_of`]), so a fact created without a scope can neither be
//!   recalled by a scoped reader nor seed a warm.
//!
//! Writes are guarded with [`super::scope::claim_authority`] — the single spelling of the in-transaction
//! commit predicate — and reads splice [`super::scope::ScopeFilter`]'s conjunct into the `WHERE` clause,
//! before any `LIMIT`, so a page is a page *of this population* rather than a global page trimmed
//! afterwards.

use crate::access::{OwnerContext, ScopedContext};
use crate::error::StorageError;
use crate::models::{Cluster, Fact, HeatState, MemoryEdge, MemoryScope, RawRecord};
use crate::repos::memory_repo::FactListQuery;
use crate::repos::scope::{Bind, GuardedWrite, ScopeFilter, guarded_run};
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue, ToSql};

/// The live predicate, spelled once: `quarantined_at = NONE`, never `IS NOT NULL`/`!= NULL`, which on
/// this engine are both satisfied by `NONE` and so filter nothing.
const LIVE: &str = "deleted = false AND quarantined_at = NONE";

/// The ceiling on the `k` a scoped kNN read will interpolate, and the reason one exists.
///
/// `k` is the one number in the statement that a caller influences, so it cannot be left to reach the
/// parser as delivered: the MCP request path clamps its own `limit`, but `nearest` is storage's public
/// seam and a caller that forgot to would otherwise put a client's number straight into statement text.
/// Measured against the pinned 3.3 engine, `nearest(q, usize::MAX, false)` fails to parse at all
/// (`number too large to fit in target type`), which is the milder half of the problem — an unbounded
/// `k` that *did* parse would ask the brute-force scan for the whole population.
///
/// The value is the HNSW search breadth rather than a fresh constant: a `k` above `ef` cannot be
/// answered from the index anyway, and the request path's largest ask
/// (`MAX_RETRIEVE_LIMIT + KNN_SLACK`, 110) sits under it with room to spare — which
/// `the_k_ceiling_sits_above_what_the_request_path_can_ask` pins, so the two ceilings cannot drift
/// apart without a red test.
pub const MAX_KNN_K: usize = crate::repos::memory_repo::HNSW_EF;

/// How a scoped kNN read obtained its candidates, and the only label the request path may report.
///
/// Two values because there are two statements and the choice between them belongs to the server:
/// [`RetrievalStrategy::Indexed`] is the numeric-ef form, valid only once
/// [`ensure_vector_index`](crate::schema::ensure_vector_index) defined the index; [`RetrievalStrategy::Exact`]
/// is the filtered brute-force cosine scan that answers when it was not defined, and is also the honest
/// word for a session-narrowed read, where no vector index participates at all and the ranking is exact
/// cosine over the session's own eligible rows.
///
/// The label is *derived*, never parsed and never supplied by a caller: [`RetrievalStrategy::for_index`]
/// is the one function that maps "the index is there" onto both the operator argument inside
/// [`ScopedMemoryRepo::nearest_statement`] and the word in a response, so the two cannot disagree about
/// what ran. `AGENTS.md` records why that matters on 3.3: the numeric-ef form with no index behind it
/// matches **no rows**, so a wrong `Indexed` is indistinguishable from an empty population to everyone
/// but the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalStrategy {
    Indexed,
    Exact,
}

impl RetrievalStrategy {
    /// The strategy a store with (or without) a defined HNSW index runs.
    pub const fn for_index(index_defined: bool) -> Self {
        if index_defined {
            Self::Indexed
        } else {
            Self::Exact
        }
    }

    /// The owner-facing label. `as_str` rather than `Display` so a `format!("{strategy}")` cannot
    /// quietly turn a machine-readable label into prose a client has to parse.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::Exact => "exact",
        }
    }

    /// The KNN operator's second argument: the fixed search breadth for the index, the distance name
    /// for the scan that never consults it. Same source as `MemoryRepo`'s forms, by way of
    /// [`crate::repos::memory_repo::HNSW_EF`], so the two paths cannot drift.
    fn knn_argument(self) -> String {
        match self {
            Self::Indexed => crate::repos::memory_repo::HNSW_EF.to_string(),
            Self::Exact => "COSINE".to_string(),
        }
    }
}

/// Which partition a stored memory is written into.
///
/// A request-side enum rather than a `MemoryScope` because the caller speaks in *aliases*: `store_memory`
/// carries a project string, and resolving it to a `ProjectId` is this crate's job — a caller that could
/// hand a `MemoryScope` directly could hand one belonging to a workspace it has no claim on.
#[derive(Debug, Clone)]
pub enum WriteTarget {
    /// The owner's Personal partition.
    Personal,
    /// One of the owner's projects, named by its validated handle.
    Project(ProjectId),
    /// The owner's Unassigned partition: stored, visible only to an explicit
    /// [`ReadSelection::UnassignedReview`](crate::models::ReadSelection), and never a recall or
    /// maintenance population.
    Unassigned,
}

use crate::models::ProjectId;

/// One of the caller's rows as an owner-facing view needs it: identity, the two visibility states, and the
/// three columns [`Fact`] does not carry because the browse surface never asked for them.
///
/// `scope`/`project` are the raw stored pair rather than a resolved [`crate::models::MemoryScope`] because
/// resolving one needs the project's own row, and a listing that resolved per row is an N+1 read of the
/// owner's own projects for a label the page can spell from the handle it already has.
#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct OwnedRow {
    pub id: Option<RecordId>,
    pub content: String,
    pub confidence: f64,
    pub tags: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub quarantined_at: Option<DateTime<Utc>>,
    pub scope: Option<String>,
    pub project: Option<RecordId>,
    /// v009's guarded semantic revision, `option<int>` because a row written before the column existed has
    /// none — which the view renders as `0`, the same reading the journal's own CAS applies.
    pub semantic_revision: Option<i64>,
}

pub struct ScopedMemoryRepo<'a> {
    db: &'a Surreal<Any>,
    owner: &'a OwnerContext,
    read: ScopeFilter,
    /// The scopes the resolved population carries, kept so a write can be checked against the same
    /// partitions the reads use rather than only against the account.
    granted: Vec<MemoryScope>,
}

/// The owner-review page over the caller's resolved partitions, built from its population conjunct and
/// its resume clause (` AND id > $cursor`, empty on the first page).
///
/// A function rather than a format-string constant because `format!` takes only a literal template: this
/// way the shipped statement and `review_page_*` plan tests read exactly one source of truth, and a change
/// to the statement is a change the plan tests see.
///
/// The shape is the one `AGENTS.md` flags as a trap — `WHERE … AND id > $cursor ORDER BY id LIMIT n` —
/// and `review_page_plans_the_qualified_partition_index_on_both_pages` is the measurement that shows
/// this read does **not** fall into it here: both pages plan an `IndexScan` on `idx_fact_partition`,
/// because the workspace+scope equality that precedes the cursor is what selects the index. A cursor over
/// a *range-typed* `FROM $records` is what needs the bound-key-range spelling; a filter on the table keeps
/// the index.
pub fn review_page_sql(conjunct: &str, cursor_clause: &str) -> String {
    format!(
        "SELECT * FROM fact WHERE {conjunct}{cursor_clause} AND deleted = false ORDER BY id ASC \
             LIMIT $limit;"
    )
}

impl<'a> ScopedMemoryRepo<'a> {
    /// Binds one writer to an owner *and* to the population a read of theirs resolved to.
    ///
    /// Two values on purpose: writes stamp the owner's account and workspace, reads select the resolved
    /// partitions, and conflating them would either let a read see rows it was never granted or let a
    /// write claim an account the caller does not hold.
    pub fn new(db: &'a Surreal<Any>, owner: &'a OwnerContext, scope: &'a ScopedContext) -> Self {
        Self {
            db,
            owner,
            read: ScopeFilter::new(scope),
            granted: scope
                .partitions()
                .iter()
                .map(|partition| partition.scope.clone())
                .collect(),
        }
    }

    /// Creates a memory inside `target`'s partition, stamped with the bound account and workspace.
    ///
    /// The project is validated as belonging to this workspace before the row is written, so a caller
    /// cannot create a fact in a project it does not own — and a foreign or absent project id and a
    /// malformed one all refuse identically.
    pub async fn create_fact(
        &self,
        content: &str,
        confidence: f64,
        embedding: &[f32],
        tags: &[String],
        target: WriteTarget,
        now: DateTime<Utc>,
    ) -> Result<Fact, StorageError> {
        let (scope_text, project) = self.resolve_write_partition(&target).await?;
        let sets = format!(
            "CREATE fact SET content = $content, confidence = $confidence, embedding = $embedding, \
             tags = $tags, deleted = false, account = $account, workspace = $workspace, scope = \
             $scope{project_clause}",
            project_clause = if project.is_some() {
                ", project = $project"
            } else {
                ""
            }
        );
        let rows: Vec<Fact> = self
            .guarded_create(
                &sets,
                vec![
                    ("content", Bind::Text(content.to_string())),
                    ("confidence", Bind::Float(confidence)),
                    ("embedding", Bind::Floats(embedding.to_vec())),
                    ("tags", Bind::TextList(tags.to_vec())),
                    ("scope", Bind::Text(scope_text.to_string())),
                ],
                project
                    .into_iter()
                    .map(|id| ("project", id))
                    .collect::<Vec<_>>(),
                now,
            )
            .await?;
        rows.into_iter().next().ok_or_else(|| missing("fact"))
    }

    /// Creates the `raw` record a document import links its chunks back to.
    ///
    /// `raw` carries the same ownership columns as `fact` (v009), and the only path to a chunk's owner is
    /// its `extracted_from` edge — so an unowned `raw` row is a document nobody's scoped reader can find.
    pub async fn create_raw(
        &self,
        content: &str,
        now: DateTime<Utc>,
    ) -> Result<RawRecord, StorageError> {
        let rows: Vec<RawRecord> = self
            .guarded_create(
                "CREATE raw SET content = $content, deleted = false, account = $account, workspace \
                 = $workspace;",
                vec![("content", Bind::Text(content.to_string()))],
                vec![],
                now,
            )
            .await?;
        rows.into_iter().next().ok_or_else(|| missing("raw"))
    }

    /// One of the bound population's facts by handle.
    ///
    /// A foreign id and an absent id give the same `None`, so this is not a probe for whether the row
    /// exists somewhere.
    pub async fn get_fact(&self, id: &RecordId) -> Result<Option<Fact>, StorageError> {
        let sql = format!(
            "SELECT * FROM type::record('fact', $key) WHERE id = $id AND {} LIMIT 1;",
            self.read.conjunct()
        );
        let query = self.read.apply(
            self.db
                .query(sql)
                .bind(("key", crate::repos::scope::record_key(id)?))
                .bind(("id", id.clone())),
        );
        let mut response = query.await.map_err(internal)?.check().map_err(internal)?;
        let rows: Vec<Fact> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// The `k` nearest live facts **of the resolved population**.
    ///
    /// The scope conjunct is inside the KNN statement, not applied to its output: the engine returns the
    /// `k` nearest *eligible* rows — exact for [`RetrievalStrategy::Exact`], and up to the index's
    /// approximation in the `Indexed` state, where `ef = 150` behind a selective filter can answer with
    /// fewer than `k` eligible rows on a large corpus. Truncating a global neighbour list and filtering
    /// afterwards is the failure this seam exists to prevent, and it is invisible in the shape of an
    /// answer — the caller simply gets fewer rows, by an amount that depends on how much somebody else
    /// stored. Both states are pinned against closer out-of-scope vectors in
    /// `tests/scoped_retrieval_test.rs`, by rows *and* by plan, because neither proof implies the other.
    ///
    /// `strategy` is the server's choice, made once from whether the HNSW index was defined — never
    /// from caller input — and it is also the label the request path reports, which is the only way that
    /// label can be *about* the statement that ran.
    ///
    /// `k` is clamped to [`MAX_KNN_K`] before it reaches statement text; see that constant for why the
    /// bound lives here rather than only at the tool boundary. A `k` of zero asks for nothing and gets
    /// nothing, which is the answer, not an error.
    pub async fn nearest(
        &self,
        query_vector: &[f32],
        k: usize,
        strategy: RetrievalStrategy,
    ) -> Result<Vec<Fact>, StorageError> {
        let query = self.read.apply(
            self.db
                .query(self.nearest_statement(k, strategy))
                .bind(("q", query_vector.to_vec())),
        );
        let mut response = query.await.map_err(internal)?.check().map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The kNN statement [`Self::nearest`] issues, as text.
    ///
    /// Named rather than inlined, and public rather than crate-private, for one reason each: the query
    /// plan is the only thing that can tell an index scan from a brute-force scan (the rows cannot, and
    /// on 3.3 the numeric-ef form with no index behind it returns *no rows at all*, which a row count
    /// reads as a correctly emptied population), and the durable gate that checks it lives in
    /// `tests/scoped_retrieval_test.rs`, outside this crate. Either way the gate runs the text
    /// production runs, never a string a test assembled to look like it.
    ///
    /// The same two forms `MemoryRepo` uses, reached through the same [`RetrievalStrategy`] mapping, so
    /// the label a response carries and the operator that ran cannot be made to disagree.
    pub fn nearest_statement(&self, k: usize, strategy: RetrievalStrategy) -> String {
        let k = k.min(MAX_KNN_K);
        format!(
            "SELECT * FROM fact WHERE {LIVE} AND {} AND embedding <|{k},{}|> $q;",
            self.read.conjunct(),
            strategy.knn_argument()
        )
    }

    /// `EXPLAIN FORMAT JSON` of the statement [`Self::nearest`] issues, with this context's own binds.
    ///
    /// Storage owns the text, so storage runs the attestation too: a caller that had to rebuild the
    /// statement to plan it would be planning its own copy.
    pub async fn explain_nearest(
        &self,
        query_vector: &[f32],
        k: usize,
        strategy: RetrievalStrategy,
    ) -> Result<String, StorageError> {
        let query = self.read.apply(
            self.db
                .query(format!(
                    "EXPLAIN FORMAT JSON {}",
                    self.nearest_statement(k, strategy)
                ))
                .bind(("q", query_vector.to_vec())),
        );
        let plan: surrealdb::types::Value = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        Ok(plan.to_sql().replace(' ', ""))
    }

    /// One keyset page of the bound population, oldest handle first.
    ///
    /// This is the owner-review read, and the population is whatever the caller's resolved
    /// [`ReadSelection`] named — in Task 8, `UnassignedReview`, because Unassigned is reachable through
    /// no other selection. The predicate is [`ScopeFilter`]'s, so the partition equality that makes it an
    /// `IndexScan` on `idx_fact_partition` is in the statement and not a filter applied afterwards: a
    /// review page that truncated globally and then filtered would let one owner's page depend on how
    /// much every other account holds, which is the failure the whole scoped pass exists to remove.
    ///
    /// **Quarantined rows are included**, unlike every live read in this crate, and that is the point:
    /// quarantine is the middle rung of the dreaming ladder — hidden from recall, still inspectable —
    /// and the review population is exactly where an owner goes to inspect it. Soft-deleted rows are
    /// still excluded: a tombstone is not review state, it is a deletion.
    ///
    /// The cursor is the last record id the caller was given, and `None` starts the walk. Ordering by
    /// `id` is total because a record id is unique, so pages cannot repeat or skip a row the way a
    /// `created_at` page can when a batch import stamps ten rows with one timestamp.
    pub async fn review_page(
        &self,
        after: Option<&RecordId>,
        limit: usize,
    ) -> Result<Vec<Fact>, StorageError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX).max(1);
        let cursor_clause = if after.is_some() {
            " AND id > $cursor"
        } else {
            ""
        };
        let sql = review_page_sql(self.read.conjunct(), cursor_clause);
        let mut query = self.read.apply(self.db.query(sql).bind(("limit", limit)));
        if let Some(cursor) = after {
            query = query.bind(("cursor", cursor.clone()));
        }
        let mut response = query.await.map_err(internal)?.check().map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// Creates the `provenance` row describing one of the bound population's facts and links it to that
    /// fact with `has_provenance`, in one guarded transaction.
    ///
    /// This replaces the request path's bare `CREATE provenance SET kind = 'user'`, which left the row
    /// unrelated to the fact it described and unstamped even though v009 gave `provenance` its own
    /// `account`/`workspace`. The create and the `RELATE` are one script and therefore one transaction: a
    /// stamped row with no link, or a link pointing at a row that never appeared, are the two half-states
    /// this shape cannot produce. `check()` covers every statement of the script, so `Ok(())` means both
    /// landed, and a transaction that never commits leaves neither.
    ///
    /// The link is a plain `RELATE` on a table v001 already defines, which is why none of Task 10's journal
    /// machinery is required to write it.
    pub async fn create_provenance(
        &self,
        fact: &RecordId,
        kind: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let write = GuardedWrite::new(
            "LET $prov = CREATE provenance SET kind = $kind, timestamp = $now, account = $account, \
             workspace = $workspace RETURN AFTER; RELATE $fact->has_provenance->$prov;",
        )
        .id("fact", fact.clone())
        .value("kind", Bind::Text(kind.to_string()))
        .value("now", Bind::Time(now));
        guarded_run::<serde_json::Value>(self.db, self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)?;
        Ok(())
    }

    /// Soft-deletes one of the bound population's facts.
    ///
    /// Aiming at a row outside the population is `NotFound`, and the write itself is guarded, so a dead
    /// credential cannot delete a row it resolved moments earlier.
    pub async fn soft_delete(
        &self,
        id: &RecordId,
        now: DateTime<Utc>,
    ) -> Result<Fact, StorageError> {
        let sql = format!(
            "UPDATE fact SET deleted = true WHERE id = $id AND {} RETURN AFTER;",
            self.read.conjunct()
        );
        let rows = self
            .guarded_write(&sql, vec![("id", id.clone())], vec![], now)
            .await?;
        rows.into_iter().next().ok_or(StorageError::NotFound)
    }

    /// Updates one of the bound population's facts, returning the new row.
    ///
    /// Content, tags and confidence move under the same population predicate; `embedding` is written
    /// here because a content change re-embeds in the same operation and a partial write would leave a
    /// row whose vector describes its previous text.
    pub async fn update_fact(
        &self,
        id: &RecordId,
        content: Option<&str>,
        tags: Option<&[String]>,
        confidence: Option<f64>,
        embedding: Option<&[f32]>,
        now: DateTime<Utc>,
    ) -> Result<Option<Fact>, StorageError> {
        let mut sets: Vec<&str> = Vec::new();
        let mut binds: Vec<(&str, Bind)> = Vec::new();
        // Each arm binds the value by moving it out of the `Option`, rather than testing `is_some()` and
        // `expect()`ing it later: the shape clippy flags, and the shape that lets a future edit desync the
        // test from the unwrap.
        if let Some(content) = content {
            sets.push("content = $content");
            binds.push(("content", Bind::Text(content.to_string())));
        }
        if let Some(tags) = tags {
            sets.push("tags = $tags");
            binds.push(("tags", Bind::TextList(tags.to_vec())));
        }
        if let Some(confidence) = confidence {
            sets.push("confidence = $confidence");
            binds.push(("confidence", Bind::Float(confidence)));
        }
        if let Some(embedding) = embedding {
            sets.push("embedding = $embedding");
            binds.push(("embedding", Bind::Floats(embedding.to_vec())));
        }
        if sets.is_empty() {
            return self.get_fact(id).await;
        }
        let sql = format!(
            "UPDATE fact SET {} WHERE id = $id AND {} RETURN AFTER;",
            sets.join(", "),
            self.read.conjunct()
        );
        let rows = self
            .guarded_write(&sql, vec![("id", id.clone())], binds, now)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// One page of the caller's own facts, under the filters the browse surface offers.
    ///
    /// The population conjunct is in the `WHERE` before `LIMIT`/`START`, which is the whole point of
    /// the method existing rather than a `MemoryRepo::list` plus a filter afterwards: a page trimmed
    /// after a global `LIMIT` is a page whose contents depend on how much every *other* account holds,
    /// and its total is a store count wearing an owner's label. [`crate::repos::scope`] is the one
    /// spelling of that conjunct, and the live/quarantine predicate is `MemoryRepo`'s own
    /// [`FactListQuery`] shape reused rather than restated, so the two pages cannot drift.
    ///
    /// `include_quarantined` is the one deliberate widening, and it is the same opt-in the unscoped
    /// page has: quarantine hides a row from recall, and a read that exists to inspect quarantine must
    /// be able to see it. It never widens *across* accounts — the conjunct is always in the statement.
    pub async fn page(&self, page: &FactListQuery<'_>) -> Result<Vec<Fact>, StorageError> {
        let sql = self.list_statement(page);
        let mut query = self
            .read
            .apply(self.db.query(sql).bind(("limit", page.limit as i64)))
            .bind(("offset", page.offset as i64));
        if let Some(search) = page.search {
            query = query.bind(("search", search.to_string()));
        }
        if let Some(tag) = page.tag {
            query = query.bind(("tag", tag.to_string()));
        }
        let mut response = query.await.map_err(internal)?.check().map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The total behind [`Self::page`], from the same filter value — see `MemoryRepo::count` for the
    /// drift this pairing exists to prevent.
    pub async fn count(&self, page: &FactListQuery<'_>) -> Result<usize, StorageError> {
        #[derive(SurrealValue)]
        struct CountRow {
            count: i64,
        }
        // The count needs no projection and no page clause, so only the predicate half is reused.
        let count_sql = format!(
            "SELECT count() FROM fact WHERE {} AND {} GROUP ALL;",
            self.read.conjunct(),
            crate::repos::MemoryRepo::filter_tail(page)
        );
        let mut query = self.read.apply(self.db.query(count_sql));
        if let Some(search) = page.search {
            query = query.bind(("search", search.to_string()));
        }
        if let Some(tag) = page.tag {
            query = query.bind(("tag", tag.to_string()));
        }
        let mut response = query.await.map_err(internal)?.check().map_err(internal)?;
        let rows: Vec<CountRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .first()
            .map(|row| row.count.max(0) as usize)
            .unwrap_or(0))
    }

    /// The statement [`Self::page`] runs, built from the population conjunct plus the page's own
    /// filters. Split out so a plan test can be shown to run the shipped text.
    fn list_statement(&self, page: &FactListQuery<'_>) -> String {
        format!(
            "SELECT *{} FROM fact WHERE {} AND {} ORDER BY {} LIMIT $limit START $offset;",
            page.sort.projection(),
            self.read.conjunct(),
            // The filter half is `MemoryRepo`'s own builder, reached rather than restated: a fourth
            // live-predicate spelling is how `quarantined_at = NONE` gets forgotten by one page.
            crate::repos::MemoryRepo::filter_tail(page),
            page.sort.order_clause(page.dir),
        )
    }

    /// One page of the owner's own rows **with the columns a partition view has to show**.
    ///
    /// [`Self::page`] returns [`Fact`], which carries no `scope`, no `project` and no
    /// `semantic_revision` — the browse surface never needed them. The collections view needs all three, and
    /// for reasons that are not cosmetic: a move control that cannot say which partition a row is in today
    /// cannot label a mixed population, and a revision the page did not render is a revision it cannot bind
    /// to its own form, which is what makes "reload if it changed" an advertisement instead of a guard.
    ///
    /// The projection is explicit rather than `SELECT *` because the vector is the widest column on the row
    /// and no owner listing renders it. The population conjunct and the filter tail are the same ones
    /// [`Self::page`] uses — `MemoryRepo::filter_tail` reached, not restated — so the two pages cannot drift
    /// about what "this caller's live/quarantined/hidden rows" means, and a row that `page` can see is a row
    /// this can see, with three more columns attached.
    pub async fn owner_page(
        &self,
        page: &FactListQuery<'_>,
    ) -> Result<Vec<OwnedRow>, StorageError> {
        let sql = format!(
            "SELECT id, content, confidence, tags, created_at, deleted, quarantined_at, scope, \
             project, semantic_revision FROM fact WHERE {} AND {} ORDER BY {} LIMIT $limit \
             START $offset;",
            self.read.conjunct(),
            crate::repos::MemoryRepo::filter_tail(page),
            page.sort.order_clause(page.dir),
        );
        let mut response = self
            .read
            .apply(self.db.query(sql).bind(("limit", page.limit as i64)))
            .bind(("offset", page.offset as i64));
        if let Some(search) = page.search {
            response = response.bind(("search", search.to_string()));
        }
        if let Some(tag) = page.tag {
            response = response.bind(("tag", tag.to_string()));
        }
        let mut response = response
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// Tag frequencies across the caller's live facts, highest first, capped.
    ///
    /// Same flatten-in-SQL, tally-in-Rust shape as `MemoryRepo::top_tags` (that workaround is recorded
    /// there), with the population conjunct added — a tag bar chart is a content-derived aggregate and
    /// is therefore the caller's own or nothing.
    pub async fn top_tags(&self, limit: usize) -> Result<Vec<(String, usize)>, StorageError> {
        #[derive(SurrealValue)]
        struct TagRow {
            tags: Vec<String>,
        }
        let mut response = self
            .read
            .apply(self.db.query(format!(
                "SELECT array::flatten(array::group(tags)) AS tags FROM fact WHERE {} AND \
                 deleted = false AND quarantined_at = NONE GROUP ALL;",
                self.read.conjunct()
            )))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let row: Option<TagRow> = response.take(0).map_err(internal)?;
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for tag in row.map(|row| row.tags).unwrap_or_default() {
            *counts.entry(tag).or_default() += 1;
        }
        let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        pairs.truncate(limit);
        Ok(pairs)
    }

    /// The heat row of one of the caller's facts, or `None` when the fact is not the caller's to read
    /// or has no heat row at all.
    ///
    /// `heat_state` carries no owner column (v009 leaves ownership to the row's `memory` link), so the
    /// population test is a subquery over `fact` rather than a predicate on `heat_state` — and it is
    /// in the statement, so a foreign fact's heat cannot come back even when its id is known.
    pub async fn heat_of(&self, id: &RecordId) -> Result<Option<HeatState>, StorageError> {
        let mut response = self
            .read
            .apply(
                self.db
                    .query(format!(
                        "SELECT * FROM heat_state WHERE memory = $id AND memory INSIDE (SELECT \
                         VALUE id FROM fact WHERE {}) LIMIT 1;",
                        self.read.conjunct()
                    ))
                    .bind(("id", id.clone())),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<HeatState> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// The caller's own heat distribution, in [`super::heat_repo::HEAT_BANDS`] order.
    ///
    /// Four counted aggregates in one round trip, each narrowed to the population the same way
    /// [`Self::heat_of`] is. A store-wide histogram is another account's ranking information.
    pub async fn heat_bands(&self) -> Result<Vec<usize>, StorageError> {
        #[derive(SurrealValue)]
        struct CountRow {
            count: i64,
        }
        let bounds: [(Option<f64>, Option<f64>); 4] = [
            (None, Some(1.0)),
            (Some(1.0), Some(2.0)),
            (Some(2.0), Some(3.0)),
            (Some(3.0), None),
        ];
        let mut sql = String::new();
        for (lower, upper) in bounds {
            let mut arms = Vec::new();
            if let Some(lower) = lower {
                arms.push(format!("heat >= {lower}"));
            }
            if let Some(upper) = upper {
                arms.push(format!("heat < {upper}"));
            }
            sql.push_str(&format!(
                "SELECT count() FROM heat_state WHERE {} AND memory INSIDE (SELECT VALUE id FROM \
                 fact WHERE {}) GROUP ALL;",
                arms.join(" AND "),
                self.read.conjunct()
            ));
        }
        let mut response = self
            .read
            .apply(self.db.query(sql))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let mut counts = Vec::with_capacity(bounds.len());
        for index in 0..bounds.len() {
            let rows: Vec<CountRow> = response.take(index).map_err(internal)?;
            counts.push(
                rows.first()
                    .map(|row| row.count.max(0) as usize)
                    .unwrap_or(0),
            );
        }
        Ok(counts)
    }

    /// The cluster a fact of the caller's belongs to, if any.
    ///
    /// The membership is read through the caller's own cluster rows, so this answers "which of *my*
    /// clusters holds this fact" and nothing else: `cluster.workspace` is the population test, and a
    /// fact outside the caller's partitions is `None` rather than a cluster label.
    pub async fn cluster_of(&self, id: &RecordId) -> Result<Option<Cluster>, StorageError> {
        // The fact must be the caller's first, or the traversal would answer a question about a row
        // this context cannot read at all.
        if self.get_fact(id).await?.is_none() {
            return Ok(None);
        }
        // The ownership test is inside the traversal target's parentheses, which is the only spelling
        // this engine parses for a filtered graph match, and the match is made *from the fact* so the
        // result set is the cluster rows themselves. Both halves were measured on the pinned 3.3.0
        // engine: the alternative shape — `SELECT (<-contains_memory<-(cluster WHERE …)) AS c FROM
        // $focal` — parses, but yields an array of **record ids**, so deserialising it into `Cluster`
        // produced nothing and the memory page silently lost its cluster link. `Cluster` does not
        // project `workspace`, so filtering the rows afterwards in Rust is not an option either.
        let mut response = self
            .read
            .apply(
                self.db
                    .query(
                        "SELECT * FROM $focal <-contains_memory<-(cluster WHERE workspace = \
                         $scope_workspace);",
                    )
                    .bind(("focal", id.clone())),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Cluster> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// The edges incident to one record, keeping only those whose **other end** is inside the caller's
    /// population.
    ///
    /// A line to a row this caller cannot open would name it, which is the leak this filter exists for:
    /// the memory detail page and the graph page both draw what they are handed and cannot themselves
    /// tell a shared cluster's member from a stranger's.
    pub async fn edges_of(&self, id: &RecordId) -> Result<Vec<MemoryEdge>, StorageError> {
        if self.get_fact(id).await?.is_none() {
            return Ok(Vec::new());
        }
        let mut response = self
            .read
            .apply(
                self.db
                    .query(format!(
                        "SELECT * FROM memory_edge WHERE (in = $id OR out = $id) AND ((in INSIDE \
                         (SELECT VALUE id FROM fact WHERE {pop} AND deleted = false AND \
                         quarantined_at = NONE) AND (out = $id OR (out INSIDE (SELECT VALUE id \
                         FROM fact WHERE {pop} AND deleted = false AND quarantined_at = NONE)))));",
                        pop = self.read.conjunct()
                    ))
                    .bind(("id", id.clone())),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// Edges among a set of records, with both ends required to be readable by this context.
    ///
    /// The graph page's edge collector: it already holds a bounded node set and must not pay one query
    /// per node, and "both ends inside" is what keeps a shared cluster or a lineage edge from drawing a
    /// line to a memory this caller cannot open.
    pub async fn edges_within(&self, ids: &[RecordId]) -> Result<Vec<MemoryEdge>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut response = self
            .read
            .apply(
                self.db
                    .query(format!(
                        "SELECT * FROM memory_edge WHERE (in INSIDE $ids OR out INSIDE $ids) AND \
                         in INSIDE (SELECT VALUE id FROM fact WHERE {}) AND out INSIDE (SELECT \
                         VALUE id FROM fact WHERE {}) LIMIT $limit;",
                        self.read.conjunct(),
                        self.read.conjunct()
                    ))
                    .bind(("ids", ids.to_vec()))
                    .bind(("limit", GRAPH_EDGE_CAP as i64)),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        response.take(0).map_err(internal)
    }

    /// The caller's own ego-graph: every fact reachable from `focal` within `hops` steps, staying
    /// inside the resolved population, plus the edges among them.
    ///
    /// Three rules, all of them about *where* the filter lives:
    ///
    /// * the focal record must itself be readable, or the answer is empty — a foreign id does not get
    ///   to be a window onto whoever owns it;
    /// * a neighbour is admitted by its own row, in SQL, before it is ever named: the candidate ids come
    ///   from `memory_edge`, and only those that are live facts of this population survive into the
    ///   result. A one-hop-per-query walk is what `EdgeRepo::get_neighbors_capped` already costs, so
    ///   the scoping adds no round trips;
    /// * the visit bound is honoured the same way — the walk *stops* rather than skipping a node, so a
    ///   hub cannot turn a page into thousands of queries.
    ///
    /// `truncated` says the bound was hit; `hop_of` gives the shortest distance the walk found, which is
    /// what the renderer draws.
    pub async fn ego(
        &self,
        focal: &RecordId,
        hops: u32,
        visit_cap: usize,
    ) -> Result<ScopedEgoGraph, StorageError> {
        let hops = hops.min(MAX_GRAPH_HOPS);
        if self.get_fact(focal).await?.is_none() {
            return Ok(ScopedEgoGraph::default());
        }
        let mut hop_of: std::collections::HashMap<String, u32> =
            std::collections::HashMap::from([(key_of(focal), 0)]);
        let mut nodes: Vec<RecordId> = vec![focal.clone()];
        let mut edges: Vec<MemoryEdge> = Vec::new();
        let mut seen_edges: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut frontier = vec![focal.clone()];
        let mut truncated = false;

        for hop in 1..=hops {
            if frontier.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for node in &frontier {
                let candidates = self.adjacent(node).await?;
                if candidates.is_empty() {
                    continue;
                }
                for (neighbor, edge) in candidates {
                    let key = key_of(&neighbor);
                    if hop_of.contains_key(&key) {
                        // Already drawn; still record the edge if it is new, because a cycle is part of
                        // the picture the operator is reading.
                        if let Some(edge) = edge
                            && let Some(id) = edge.id.as_ref()
                            && seen_edges.insert(key_of(id))
                        {
                            edges.push(edge);
                        }
                        continue;
                    }
                    if hop_of.len() >= visit_cap {
                        truncated = true;
                        break;
                    }
                    hop_of.insert(key, hop);
                    nodes.push(neighbor.clone());
                    next.push(neighbor);
                    if let Some(edge) = edge
                        && let Some(id) = edge.id.as_ref()
                        && seen_edges.insert(key_of(id))
                    {
                        edges.push(edge);
                    }
                }
                if truncated {
                    break;
                }
            }
            if truncated {
                break;
            }
            frontier = next;
        }

        // One more read, so an edge *between* two admitted nodes is drawn even when it was not the edge
        // that discovered one of them — the same "both ends must be in the set" rule the unscoped page
        // applied in Rust, now applied by the statement.
        edges.extend(self.edges_within(&nodes).await?);
        // The two reads can each return the same edge, and the merged page must name it once. Sorted by
        // the row's own key so the order is stable across renders.
        edges.sort_by(|a, b| {
            (
                a.in_node.as_ref().map(key_of),
                a.out_node.as_ref().map(key_of),
            )
                .cmp(&(
                    b.in_node.as_ref().map(key_of),
                    b.out_node.as_ref().map(key_of),
                ))
        });
        edges.dedup_by(|a, b| {
            a.in_node == b.in_node && a.out_node == b.out_node && a.edge_type == b.edge_type
        });
        Ok(ScopedEgoGraph {
            nodes,
            hops: hop_of,
            edges,
            truncated,
        })
    }

    /// The facts adjacent to one record, each with the edge that reaches it, admitting only neighbours
    /// this population may read.
    async fn adjacent(
        &self,
        node: &RecordId,
    ) -> Result<Vec<(RecordId, Option<MemoryEdge>)>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT in, out, edge_type, strength FROM memory_edge WHERE in = $node OR out = $node;",
            )
            .bind(("node", node.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        // `in`/`out` are reserved words in projection position, so they arrive aliased — the same
        // rename `EdgeRepo` makes.
        #[derive(SurrealValue)]
        struct RawEdgeRow {
            #[surreal(rename = "in")]
            in_node: Option<RecordId>,
            #[surreal(rename = "out")]
            out_node: Option<RecordId>,
            edge_type: String,
            strength: f64,
        }
        let rows: Vec<RawEdgeRow> = response.take(0).map_err(internal)?;
        let mut candidates: Vec<(RecordId, MemoryEdge)> = Vec::new();
        for row in rows {
            let other = match (&row.in_node, &row.out_node) {
                (Some(in_node), _) if in_node == node => row.out_node.clone(),
                (_, Some(out_node)) if out_node == node => row.in_node.clone(),
                _ => None,
            };
            let (Some(other), Some(in_node), Some(out_node)) =
                (other, row.in_node.clone(), row.out_node.clone())
            else {
                continue;
            };
            candidates.push((
                other,
                MemoryEdge {
                    id: None,
                    in_node: Some(in_node),
                    out_node: Some(out_node),
                    edge_type: row.edge_type,
                    strength: row.strength,
                    created_at: None,
                },
            ));
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<RecordId> = candidates.iter().map(|(id, _)| id.clone()).collect();
        let mut response = self
            .read
            .apply(
                self.db
                    .query(format!(
                        "SELECT id FROM fact WHERE id INSIDE $ids AND {} AND deleted = false AND \
                         quarantined_at = NONE;",
                        self.read.conjunct()
                    ))
                    .bind(("ids", ids)),
            )
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        #[derive(SurrealValue)]
        struct IdRow {
            id: Option<RecordId>,
        }
        let admitted: std::collections::HashSet<String> = response
            .take::<Vec<IdRow>>(0)
            .map_err(internal)?
            .into_iter()
            .filter_map(|row| row.id)
            .map(|id| key_of(&id))
            .collect();
        Ok(candidates
            .into_iter()
            .filter(|(id, _)| admitted.contains(&key_of(id)))
            .map(|(id, edge)| (id, Some(edge)))
            .collect())
    }

    /// Counts of the caller's own corpus: live and soft-deleted facts, clusters, sessions, raw
    /// documents, and the edges that join them.
    ///
    /// Replaces the store-wide `stats::gather` on every browser path. Clusters, raws and sessions are
    /// counted by `workspace`/`account` because those tables carry an owner stamp; edges and heat are
    /// counted *through* the caller's facts because `memory_edge` and `heat_state` deliberately carry
    /// no owner column of their own (v009), so their ownership is the endpoint path.
    pub async fn counts(&self) -> Result<CorpusCounts, StorageError> {
        let mut response = self
            .read
            .apply(self.db.query(format!(
                "SELECT count() FROM fact WHERE {} AND deleted = false AND quarantined_at \
                 = NONE GROUP ALL;
                 SELECT count() FROM fact WHERE {} AND deleted = true GROUP ALL;
                 SELECT count() FROM fact WHERE {} AND quarantined_at != NONE GROUP \
                 ALL;
                 SELECT count() FROM cluster WHERE workspace = $scope_workspace GROUP ALL;
                 SELECT count() FROM raw WHERE workspace = $scope_workspace GROUP ALL;
                 SELECT count() FROM memory_edge WHERE in INSIDE (SELECT VALUE id FROM \
                 fact WHERE {}) GROUP ALL;",
                self.read.conjunct(),
                self.read.conjunct(),
                self.read.conjunct(),
                self.read.conjunct()
            )))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        #[derive(SurrealValue)]
        struct CountRow {
            count: i64,
        }
        // Each statement's rows are at its own index: a multi-statement query's `take(0)` is the
        // *first* statement's result and nothing else (measured, and recorded in the Task 9 probe
        // findings). Reading index 0 six times is what produced an "internal storage error" on the
        // dashboard here — the later statements' rows were never looked at.
        let mut number = |index: usize| -> Result<usize, StorageError> {
            let rows: Vec<CountRow> = response.take(index).map_err(internal)?;
            Ok(rows
                .first()
                .map(|row| row.count.max(0) as usize)
                .unwrap_or(0))
        };
        Ok(CorpusCounts {
            live_facts: number(0)?,
            deleted_facts: number(1)?,
            quarantined_facts: number(2)?,
            clusters: number(3)?,
            raw_documents: number(4)?,
            edges: number(5)?,
        })
    }

    /// Resolves a caller's partition choice to the columns a write stores.
    ///
    /// The project is checked against the bound workspace by reading the row, so `WriteTarget::Project`
    /// is a claim this method either confirms or refuses.
    async fn resolve_target(
        &self,
        target: &WriteTarget,
    ) -> Result<(&'static str, Option<RecordId>), StorageError> {
        match target {
            WriteTarget::Personal => Ok(("personal", None)),
            WriteTarget::Unassigned => Ok(("unassigned", None)),
            WriteTarget::Project(project) => {
                let mut response = self
                    .db
                    .query(
                        "SELECT id FROM project WHERE id = $id AND workspace = $workspace LIMIT 1;",
                    )
                    .bind(("id", project.as_record_id().clone()))
                    .bind(("workspace", self.owner.workspace().as_record_id().clone()))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?;
                let rows: Vec<serde_json::Value> = response.take(0).map_err(internal)?;
                if rows.is_empty() {
                    // Foreign and absent are one answer, so this is not an oracle for "does that project
                    // exist somewhere else".
                    return Err(StorageError::NotFound);
                }
                Ok(("project", Some(project.as_record_id().clone())))
            }
        }
    }

    /// The scope text and `project` a write stamps, checked against the partitions this context resolved.
    ///
    /// Writing to a partition the caller was not granted is refused even though the account is correct:
    /// `select` is what decided the population, and a write that could bypass it would let a Personal
    /// context create project rows that its own later reads cannot see.
    async fn resolve_write_partition(
        &self,
        target: &WriteTarget,
    ) -> Result<(&'static str, Option<RecordId>), StorageError> {
        let (scope, project) = self.resolve_target(target).await?;
        let expected = match target {
            WriteTarget::Personal => MemoryScope::Personal,
            WriteTarget::Unassigned => MemoryScope::Unassigned,
            WriteTarget::Project(project) => MemoryScope::Project(project.clone()),
        };
        let granted = self.granted.contains(&expected);
        if !granted {
            return Err(StorageError::NotFound);
        }
        Ok((scope, project))
    }

    /// Runs one **create** inside the transaction that asserts this context's authority.
    ///
    /// A `CREATE` has no `WHERE` to hang a guard on, so the atomicity comes from the other direction:
    /// the conditional write on the authorization row and the `CREATE` share one transaction, and a
    /// concurrent revoke or epoch bump costs the commit a write conflict rather than letting an
    /// unauthorized row appear. The machinery is [`guarded_run`], the single spelling every scoped writer
    /// in this crate shares.
    async fn guarded_create<T>(
        &self,
        sql: &str,
        binds: Vec<(&str, Bind)>,
        extra: Vec<(&str, RecordId)>,
        now: DateTime<Utc>,
    ) -> Result<Vec<T>, StorageError>
    where
        T: SurrealValue + Send,
    {
        let mut write = GuardedWrite::new(sql);
        for (name, id) in extra {
            write = write.id(name, id);
        }
        for (name, bind) in binds {
            write = write.value(name, bind);
        }
        guarded_run(self.db, self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)
    }

    /// A guarded update against the resolved population.
    async fn guarded_write(
        &self,
        sql: &str,
        targets: Vec<(&str, RecordId)>,
        binds: Vec<(&str, Bind)>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Fact>, StorageError> {
        let mut write = GuardedWrite::new(sql).with_scope(&self.read);
        for (name, id) in targets {
            write = write.id(name, id);
        }
        for (name, bind) in binds {
            write = write.value(name, bind);
        }
        guarded_run(self.db, self.owner, &write, now)
            .await?
            .ok_or(StorageError::Stale)
    }
}

/// The ceiling on edges one graph read will return.
///
/// The node set is already bounded by the caller's page, so this is a belt-and-braces cap on the edge
/// statement itself rather than the shape of a real ego-graph.
pub const GRAPH_EDGE_CAP: usize = 2_000;

/// How far a scoped ego-graph walk will reach, whatever the caller asked for.
///
/// The unscoped page had the same ceiling (`debug::graph::MAX_HOPS`); it lives here now because the
/// bound is what keeps the per-hop query count bounded, which is a storage property, not a view one.
pub const MAX_GRAPH_HOPS: u32 = 3;

/// The caller's own neighbourhood: the facts in it, how far each was from the root, the edges between
/// them, and whether a bound cut the walk short.
#[derive(Debug, Default, Clone)]
pub struct ScopedEgoGraph {
    pub nodes: Vec<RecordId>,
    /// Hop distance per record key, so a renderer can label a node without re-deriving the walk.
    pub hops: std::collections::HashMap<String, u32>,
    pub edges: Vec<MemoryEdge>,
    pub truncated: bool,
}

/// The map key of a record id: the engine's own `Debug` rendering of table and key, which is unique per
/// row and needs no parser. `record_id_to_string` is deliberately *not* used: it is a display form, not a
/// parser-safe round trip (`AGENTS.md`), and a map key that cannot be re-read is a worse bug than one
/// that is ugly.
fn key_of(id: &RecordId) -> String {
    format!("{id:?}")
}

/// One owner's corpus totals. Every field is a count of rows this context resolved, never a store total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CorpusCounts {
    pub live_facts: usize,
    pub deleted_facts: usize,
    pub quarantined_facts: usize,
    pub clusters: usize,
    pub raw_documents: usize,
    pub edges: usize,
}

fn missing(what: &str) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("{what} row was not returned"))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use crate::access::AccessRepo;
    use crate::models::{OperationId, ReadSelection};
    use crate::repos::IdentityRepo;

    /// One store, one trusted-local owner, and the pair of contexts a request of theirs would be handed.
    async fn fixture() -> (Database, OwnerContext, ScopedContext) {
        let mut db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let ids = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "knn-plan-owner",
                "$argon2id$fixture-prepared-hash-knn-plan-owner-padding",
                &OperationId::new("knn-plan"),
            )
            .await
            .unwrap();
        let access = AccessRepo::new(&db);
        let principal = access.local_principal(&system, &ids.account).await.unwrap();
        let owner = access.authorize(&principal).await.unwrap();
        let scope = access
            .select(&owner, ReadSelection::Auto(None))
            .await
            .unwrap();
        (db, owner, scope)
    }

    /// `EXPLAIN FORMAT JSON` of the statement [`ScopedMemoryRepo::nearest`] actually issues.
    ///
    /// Run through the repo's own `nearest_statement` and the repo's own binds, so the plan describes
    /// the query production runs rather than a string a test assembled to look like it.
    async fn plan_of(
        repo: &ScopedMemoryRepo<'_>,
        query: &[f32],
        strategy: RetrievalStrategy,
    ) -> String {
        repo.explain_nearest(query, 3, strategy).await.unwrap()
    }

    /// The scoped kNN statement keeps the HNSW index, and says so in its plan.
    ///
    /// Results cannot tell the two plans apart, and on 3.3 the failure is worse than slow: `<|k,ef|>`
    /// with no index behind it matches **no rows**, so a silent fall off `KnnScan` reads to an agent as
    /// "this account holds nothing near that query" and to a row-count test as a green empty result.
    /// `retrieve_memories` picks this form from `AlexandriaServer::vector_index`, and Task 7's
    /// eligible-first contract is built on this statement.
    ///
    /// The conjunct is the risk the gate exists for: a boolean-parameter disjunction plus
    /// `project INSIDE $scope_projects` *inside* the KNN statement is exactly the shape a planner quietly
    /// turns into a scan. Both spellings are pinned, because the two forms are chosen at boot depending on
    /// whether the HNSW define succeeded.
    #[tokio::test]
    async fn knn_plan_keeps_the_hnsw_index_under_its_own_conjunct() {
        let (db, owner, scope) = fixture().await;
        let repo = ScopedMemoryRepo::new(db.inner(), &owner, &scope);
        let query = vec![0.9f32, 0.1];

        // No index defined yet: the brute-force form is the one that answers, and it must not be planned
        // as an index scan.
        let brute = plan_of(&repo, &query, RetrievalStrategy::Exact).await;
        assert!(
            brute.contains("KnnTopK"),
            "the COSINE form is the brute-force scan and its plan must say so: {brute}"
        );
        assert!(
            !brute.contains("KnnScan"),
            "the brute-force form was planned as an index scan: {brute}"
        );

        crate::schema::ensure_vector_index(db.inner(), 2)
            .await
            .unwrap();

        let indexed = plan_of(&repo, &query, RetrievalStrategy::Indexed).await;
        assert!(
            indexed.contains("KnnScan"),
            "the scoped kNN dropped off the index: {indexed}"
        );
        assert!(
            indexed.contains("fact_embedding_hnsw"),
            "the scoped kNN's scan is not the HNSW index: {indexed}"
        );

        // The statement under the gate is the scoped one — live predicate, caller's partition conjunct.
        let sql = repo.nearest_statement(3, RetrievalStrategy::Indexed);
        assert!(
            sql.contains("quarantined_at = NONE") && sql.contains("project INSIDE $scope_projects"),
            "the plan gate is pinning a statement that is not the scoped one: {sql}"
        );

        // And the planned statement returns rows: if the engine's plan and its result disagreed, the
        // assertion above would be decorating a query that finds nothing.
        for i in 0..2 {
            repo.create_fact(
                &format!("plan target {i}"),
                0.5,
                &[0.9, 0.1],
                &[],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .unwrap();
        }
        let rows = repo
            .nearest(&query, 3, RetrievalStrategy::Indexed)
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "the indexed scoped kNN returned no rows while its plan claims an index scan"
        );
    }

    /// A dead authority writes neither the provenance row nor its link to the fact.
    ///
    /// The create and the `RELATE` are one script in one transaction, so the half-states a reader could
    /// not tell apart — a row describing nothing, or a memory with no row — are not reachable separately.
    /// A trusted-local context has no credential to revoke, so the authority dies by suspension, which is
    /// the change that stops it.
    #[tokio::test]
    async fn a_suspended_account_writes_neither_provenance_row_nor_link() {
        let (db, owner, scope) = fixture().await;
        let repo = ScopedMemoryRepo::new(db.inner(), &owner, &scope);
        let fact = repo
            .create_fact(
                "a stored memory",
                0.5,
                &[0.9, 0.1],
                &[],
                WriteTarget::Personal,
                Utc::now(),
            )
            .await
            .unwrap();
        let handle = fact.id.clone().expect("a created fact carries an id");

        db.inner()
            .query("UPDATE account SET status = 'suspended' WHERE id = $id;")
            .bind(("id", owner.account().as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let outcome = repo.create_provenance(&handle, "user", Utc::now()).await;
        assert!(
            matches!(outcome, Err(StorageError::Stale)),
            "a suspended account's context wrote provenance: {outcome:?}"
        );

        let rows: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT id FROM provenance; SELECT count() AS c FROM has_provenance GROUP ALL;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(1)
            .unwrap();
        assert_eq!(
            rows[0]["c"].as_i64(),
            Some(0),
            "the refused provenance write left a link behind"
        );
        let provenance: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT id FROM provenance;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert!(
            provenance.is_empty(),
            "the refused provenance write left a row behind: {provenance:?}"
        );

        // Positive control: the same call with a live authority writes both halves.
        db.inner()
            .query("UPDATE account SET status = 'active' WHERE id = $id;")
            .bind(("id", owner.account().as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        repo.create_provenance(&handle, "user", Utc::now())
            .await
            .expect("a live authority may record provenance");
        let links: Vec<serde_json::Value> = db
            .inner()
            .query("SELECT in, out FROM has_provenance;")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            links.len(),
            1,
            "the live provenance write left no link: {links:?}"
        );
    }
}
