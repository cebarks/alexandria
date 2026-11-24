//! The account-bound project resolver: the one place a project name becomes a project handle.
//!
//! Callers arrive with *text* — the canonical alias Task 8's engine produced from a client's sanitized
//! remote, or an alias somebody typed — and a [`ProjectId`] is what every partition-bearing write needs.
//! That step is a lookup and, on one path, a creation, so it belongs in storage: it is the only place
//! allowed to touch a row, and the only place where "does this name resolve" can be answered against the
//! workspace rather than against the caller's claim about it.
//!
//! ## Two properties this type exists to hold
//!
//! **Every statement carries the bound workspace next to the name.** `project` and `project_alias` are
//! both unique on `(workspace, alias)` — deliberately *not* on `alias` alone, because two accounts
//! working on the same repository own two projects with one identity string. A lookup that dropped the
//! workspace predicate would not merely widen the read, it would answer with somebody else's row on a
//! name the caller is entitled to hold. So `resolve_*` cannot be built without an [`OwnerContext`] and
//! no method takes an account or workspace argument: the context is the authority, the alias is data.
//!
//! **Creation is one path, and it is idempotent.** [`ScopedProjectRepo::establish`] is the only
//! resolve-or-create entry point in this crate, and it is reached only through the MCP `ScopeService` on
//! the explicit context-establishment tool. A concurrent pair of establishes for one alias in one
//! workspace collides on `idx_project_alias`, and the loser re-reads **under its own workspace** — so it
//! returns the winner's handle rather than a foreign row or a spurious error, and a collision with
//! another workspace's row stays invisible exactly like an absent name.
//!
//! ## What is deliberately not here
//!
//! No fuzzy matching, no rename, no alias merge, and no writer for `project_alias`: an explicit alias
//! association is an owner-facing act Task 8's public surface does not offer, and inventing the write
//! path here would put an unowned operation behind a tool that looks like a read. Resolution already
//! honours that table, and `scoped_project_test` pins it with a storage-owned fixture.

use crate::access::OwnerContext;
use crate::error::StorageError;
use crate::models::auth::Project;
use crate::models::identity::{ProjectId, WorkspaceId};
use crate::repos::scope::{Bind, GuardedWrite, guarded_run};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// One `project` row, with its handles already validated.
///
/// `id` is a [`ProjectId`] rather than an `Option<RecordId>` because a row this repo returns has been
/// read back by handle or created in this transaction: a project without an id is not a state a caller
/// may act on, so it is an error here instead of a field every caller must re-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub id: ProjectId,
    pub workspace: WorkspaceId,
    pub normalized_alias: String,
    pub display_name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

impl ResolvedProject {
    fn from_row(row: Project) -> Result<Self, StorageError> {
        Ok(Self {
            id: row.project_id()?,
            workspace: row.workspace_id()?,
            normalized_alias: row.normalized_alias,
            display_name: row.display_name,
            created_at: row.created_at,
        })
    }
}

/// One entry of a listing page, with the live-memory count of its own partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub project: ResolvedProject,
    /// Live memories in `(workspace, project = this)`. Soft-deleted and quarantined rows are excluded
    /// because a count that absorbed them would disagree with every read the same caller can do.
    pub live_memory_count: i64,
}

/// One page of the caller's projects, and the cursor that continues it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPage {
    pub entries: Vec<ProjectEntry>,
    /// The alias to resume after, or `None` when this was the last page. Storage hands back the raw
    /// keyset value and the MCP boundary renders it as the opaque token a client echoes: the position
    /// is in *this* caller's index order, so there is nothing global about it to leak.
    pub next_cursor: Option<String>,
}

/// The outcome of [`ScopedProjectRepo::establish`]: which project, and whether this call created it.
///
/// `created` is not decoration. A client that asked the server to *establish* a project has to tell a
/// replay from a first-time association — that is the difference between telling a user "linked" and
/// "created" — and it is the only observable evidence that the alias did or did not already resolve
/// here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Established {
    pub project: ResolvedProject,
    pub created: bool,
}

pub struct ScopedProjectRepo<'a> {
    db: &'a Surreal<Any>,
    owner: &'a OwnerContext,
}

/// The grouped live-member count over exactly the page's project handles, named so the plan test pins the
/// statement the shipped read issues. The claim it has to keep true: this counts per page, on the
/// workspace+scope qualified index, and never by scanning the store and filtering in Rust.
const LIVE_COUNTS_SQL: &str = "SELECT project, count() AS c FROM fact WHERE workspace = $workspace \
AND scope = 'project' AND project INSIDE $handles AND deleted = false AND quarantined_at = NONE \
GROUP BY project;";

/// The resolver's two reads, named so the plan tests below `EXPLAIN` the statements the shipped method
/// actually issues. A test holding its own copy of the SQL could stay green while the query degraded into
/// a store-wide filter, which is the exact failure the v009 qualified indexes exist to prevent.
const RESOLVE_PRIMARY_SQL: &str = "SELECT * FROM project WHERE workspace = $workspace AND \
normalized_alias = $alias LIMIT 1;";

/// The second resolver step: the caller's explicit alias table.
const RESOLVE_ALIAS_SQL: &str = "SELECT project FROM project_alias WHERE workspace = $workspace AND \
alias = $alias LIMIT 1;";

/// The first page of the caller's own project listing.
const LIST_FIRST_PAGE_SQL: &str = "SELECT * FROM project WHERE workspace = $workspace ORDER BY \
normalized_alias ASC LIMIT $beyond;";

/// A resumed page. `>` on the qualified index rather than an offset, so a page costs what it returns.
const LIST_RESUME_PAGE_SQL: &str = "SELECT * FROM project WHERE workspace = $workspace AND \
normalized_alias > $after ORDER BY normalized_alias ASC LIMIT $beyond;";

impl<'a> ScopedProjectRepo<'a> {
    /// Binds the resolver to one account's workspace for its whole life.
    ///
    /// `OwnerContext` is sealed and its only producer is `AccessRepo::authorize`, so there is no way to
    /// build a resolver that reads a workspace it was not issued for: no `Default`, no
    /// `from_account_id`, no test-only constructor.
    pub fn new(db: &'a Surreal<Any>, owner: &'a OwnerContext) -> Self {
        Self { db, owner }
    }

    pub fn owner(&self) -> &OwnerContext {
        self.owner
    }

    fn workspace(&self) -> RecordId {
        self.owner.workspace().as_record_id().clone()
    }

    /// The caller's own project for a canonical alias, or `None`.
    ///
    /// Resolves through **both** identity doors, in this order, and both reads are index-qualified:
    ///
    /// 1. `project.normalized_alias` — the primary identity a project was created under.
    /// 2. `project_alias.alias` — an explicit secondary association (a repository's second remote, a
    ///    renamed one) pointing at a project of the same workspace.
    ///
    /// The primary is tried first because it is the identity a row was *created* under; an explicit
    /// alias is a pointer to a project, not a rival name for one. A foreign row behind either door is
    /// `None`, the same answer as an absent one, so a selector cannot be probed for existence in
    /// another workspace.
    pub async fn resolve_alias(
        &self,
        alias: &str,
    ) -> Result<Option<ResolvedProject>, StorageError> {
        let mut response = self
            .db
            .query(RESOLVE_PRIMARY_SQL)
            .bind(("workspace", self.workspace()))
            .bind(("alias", alias.to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Project> = response.take(0).map_err(internal)?;
        if let Some(row) = rows.into_iter().next() {
            return ResolvedProject::from_row(row).map(Some);
        }

        let mut response = self
            .db
            .query(RESOLVE_ALIAS_SQL)
            .bind(("workspace", self.workspace()))
            .bind(("alias", alias.to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let links: Vec<AliasLink> = response.take(0).map_err(internal)?;
        let Some(link) = links.into_iter().filter_map(|link| link.project).next() else {
            return Ok(None);
        };
        // The link names a row by handle, and the row is then read back under the same workspace: an
        // alias pointing outside this workspace (which no lawful write can create, and corruption can)
        // resolves to nothing rather than to somebody else's project.
        self.read_by_id(&link).await.map(Some)
    }

    /// The caller's own project for a validated handle.
    ///
    /// [`StorageError::NotFound`] is one refusal for a project of another workspace and for one that was
    /// never minted, so a handle cannot be probed for existence either.
    pub async fn resolve_id(&self, project: &ProjectId) -> Result<ResolvedProject, StorageError> {
        self.read_by_id(project.as_record_id()).await
    }

    async fn read_by_id(&self, id: &RecordId) -> Result<ResolvedProject, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM $id WHERE workspace = $workspace LIMIT 1;")
            .bind(("id", id.clone()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Project> = response.take(0).map_err(internal)?;
        ResolvedProject::from_row(rows.into_iter().next().ok_or(StorageError::NotFound)?)
    }

    /// Resolve-or-create the caller's project for `alias`, the create inside the transaction that
    /// re-asserts the caller's authority.
    ///
    /// Read first, then create guarded: a concurrent establish of the same alias is refused by the
    /// unique index, and the recovery read is workspace-bound, so it either finds the row this caller
    /// just raced or finds nothing. In the first case the call is a replay (`created: false`) and in the
    /// second the original error stands — which is what keeps a foreign collision from being adoptable
    /// by construction rather than by a check.
    ///
    /// `display_name` is written on create only: a replay's caller is a client re-establishing context,
    /// not an owner renaming anything.
    pub async fn establish(
        &self,
        alias: &str,
        display_name: Option<&str>,
    ) -> Result<Established, StorageError> {
        if let Some(project) = self.resolve_alias(alias).await? {
            return Ok(Established {
                project,
                created: false,
            });
        }

        let write = GuardedWrite::new(
            "CREATE project SET workspace = $workspace, normalized_alias = $alias, display_name = \
             $display_name RETURN AFTER;",
        )
        .value("alias", Bind::Text(alias.to_string()))
        .value(
            "display_name",
            Bind::OptText(display_name.map(str::to_string)),
        );
        let created = guarded_run::<Project>(self.db, self.owner, &write, Utc::now()).await;

        // A refused authority (`Ok(None)`, surfaced as `Stale`) must not fall into the recovery read: a
        // caller that is no longer anybody is not entitled to a row whoever else did create.
        let (project, created) = match created {
            Ok(Some(rows)) => {
                let row = rows.into_iter().next().ok_or_else(|| {
                    StorageError::Internal(anyhow::anyhow!("created project has no id"))
                })?;
                (ResolvedProject::from_row(row)?, true)
            }
            Ok(None) => return Err(StorageError::Stale),
            Err(stale @ StorageError::Stale) => return Err(stale),
            // Anything else is the statement itself refusing us, and the only refusal this path can
            // legitimately meet is the unique alias this caller is allowed to adopt — so the recovery
            // read decides between "somebody raced me here" and "the store is refusing me".
            Err(failure) => match self.resolve_alias(alias).await? {
                Some(project) => (project, false),
                None => return Err(failure),
            },
        };
        Ok(Established { project, created })
    }

    /// One page of the caller's projects, each with its live memory count.
    ///
    /// The page is a **keyset read on the workspace-qualified alias index** — `workspace = $workspace
    /// AND normalized_alias > $after ORDER BY normalized_alias` plans an `IndexScan` on
    /// `idx_project_alias`, and the first page plans the same — so an owner's page cannot be shifted by
    /// how many projects other accounts hold, which is the failure mode of a global page filtered
    /// afterwards. The alias is unique inside a workspace, so `>` is a total order with no tie group to
    /// repeat or skip between pages, and no secondary key is needed.
    ///
    /// `next_cursor` is set only when the store proved another page exists: the read asks for one row
    /// past the page and drops it, so a full last page does not claim a continuation.
    ///
    /// The counts arrive as one grouped read over the same partition the page names
    /// (`idx_fact_partition`, `scope = 'project'`, live predicate, narrowed to the page's handles) —
    /// never one query per project, and never a store-wide tally filtered in Rust.
    pub async fn list(
        &self,
        after_alias: Option<&str>,
        limit: usize,
    ) -> Result<ProjectPage, StorageError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX).max(1);
        let beyond = limit + 1;
        let (sql, first_page) = match after_alias {
            None => (LIST_FIRST_PAGE_SQL, true),
            Some(_) => (LIST_RESUME_PAGE_SQL, false),
        };
        let mut query = self
            .db
            .query(sql.to_string())
            .bind(("workspace", self.workspace()))
            .bind(("beyond", beyond));
        if !first_page {
            query = query.bind(("after", after_alias.unwrap_or_default().to_string()));
        }
        let rows: Vec<Project> = query
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;

        let has_more = rows.len() as i64 > limit;
        let mut projects = rows;
        if has_more {
            projects.pop();
        }
        let next_cursor = has_more
            .then(|| projects.last().map(|row| row.normalized_alias.clone()))
            .flatten();
        let handles: Vec<RecordId> = projects.iter().filter_map(|row| row.id.clone()).collect();
        let counts = self.live_counts(&handles).await?;
        let mut entries = Vec::with_capacity(projects.len());
        for row in projects {
            let count = row
                .id
                .as_ref()
                .and_then(|id| counts.get(&key_of(id)).copied())
                .unwrap_or(0);
            entries.push(ProjectEntry {
                live_memory_count: count,
                project: ResolvedProject::from_row(row)?,
            });
        }
        Ok(ProjectPage {
            entries,
            next_cursor,
        })
    }

    /// Live memories per project, for exactly the handles a page names.
    ///
    /// One read, partition-qualified by `workspace` and `scope = 'project'`, narrowed to the page's
    /// handles, grouped by project in SQL. A project absent from the result has zero live memories — a
    /// *counted* absence, because the grouped read covers the whole partition and the page's handles say
    /// which rows to report on.
    async fn live_counts(
        &self,
        handles: &[RecordId],
    ) -> Result<HashMap<String, i64>, StorageError> {
        if handles.is_empty() {
            return Ok(HashMap::new());
        }
        let mut response = self
            .db
            .query(LIVE_COUNTS_SQL)
            .bind(("workspace", self.workspace()))
            .bind(("handles", handles.to_vec()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<CountRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.project.map(|project| (key_of(&project), row.c)))
            .collect())
    }
}

/// One row of `project_alias`, read for the `project` handle it names.
#[derive(SurrealValue)]
struct AliasLink {
    project: Option<RecordId>,
}

/// One grouped `SELECT project, count()` row.
#[derive(SurrealValue)]
struct CountRow {
    project: Option<RecordId>,
    c: i64,
}

/// The map key for a record id.
///
/// `ToSql`, not `record_id_to_string`: AGENTS.md records the display form as **not** a parser-safe
/// round trip, and while this value is only ever a hash key that is compared to another key built the
/// same way, `to_sql` is the rendering that cannot collide across a string, numeric and UUID key. It is
/// never re-parsed and never interpolated into a statement.
fn key_of(id: &RecordId) -> String {
    use surrealdb::types::ToSql;
    id.to_sql()
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use surrealdb::types::ToSql;

    /// `EXPLAIN FORMAT JSON` of one shipped statement, rendered the way the repo's other plan tests
    /// render it — as SQL text, because the plan is a SurrealQL value and `to_sql` is what makes its
    /// node names readable.
    async fn plan(sql: &str, workspace: RecordId, alias: Option<&str>) -> String {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let mut query = db
            .inner()
            .query(format!("EXPLAIN FORMAT JSON {sql}"))
            .bind(("workspace", workspace))
            .bind(("alias", alias.unwrap_or("github.com/anten/app").to_string()))
            .bind(("beyond", 3i64));
        if alias.is_some() {
            query = query.bind(("after", alias.unwrap_or_default().to_string()));
        }
        let mut response = query.await.unwrap().check().unwrap();
        let plan: surrealdb::types::Value = response.take(0).unwrap();
        plan.to_sql()
    }

    /// A workspace handle to bind. `EXPLAIN` plans the statement without executing it, so no `workspace`
    /// row has to exist — and creating one would need a full `account` chain, which is a fixture for the
    /// *behavior* tests, not for a plan.
    fn workspace_handle() -> RecordId {
        RecordId::new("workspace", "plan_fixture")
    }

    /// The grouped live-count read over a page of project handles: `idx_fact_partition`, never a
    /// store-wide filter.
    ///
    /// A page of projects with counts must not cost a scan of every fact the workspace holds, and the
    /// claim is this lane's to prove rather than to assert in prose.
    #[tokio::test]
    async fn counting_a_pages_live_rows_plans_the_qualified_partition_index() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let mut response = db
            .inner()
            .query(format!("EXPLAIN FORMAT JSON {LIVE_COUNTS_SQL}"))
            .bind(("workspace", workspace_handle()))
            .bind(("handles", vec![RecordId::new("project", "plan_fixture")]))
            .await
            .unwrap()
            .check()
            .unwrap();
        let plan: surrealdb::types::Value = response.take(0).unwrap();
        let plan = plan.to_sql();
        assert!(
            plan.contains("idx_fact_partition") && plan.contains("IndexScan"),
            "the grouped live-count read does not reach the qualified partition index: {plan}"
        );
        assert!(
            !plan.contains("TableScan"),
            "the grouped live-count read is a scan: {plan}"
        );

        // Positive control, same shape as the resolver's: a grouped count over the same table with no
        // qualified index to reach *does* plan a scan, so the assertion above discriminates rather than
        // printing whatever the planner emits.
        let mut control = db
            .inner()
            .query("EXPLAIN FORMAT JSON SELECT label, count() AS c FROM fact GROUP BY label;")
            .await
            .unwrap()
            .check()
            .unwrap();
        let rendered: surrealdb::types::Value = control.take(0).unwrap();
        let control = rendered.to_sql();
        assert!(
            control.contains("TableScan") && !control.contains("idx_fact_partition"),
            "the unqualified control was expected to scan and name no index: {control}"
        );
    }

    /// Requirement: "resolver/list operations must use v009 qualified indexes, not full-store
    /// filtering." Both resolver reads plan onto `idx_project_alias` / `idx_project_alias_lookup`, and
    /// neither reaches a `TableScan`. Results cannot tell an index scan from a scan of the whole table
    /// that happens to return one row — which is why this asserts the plan, exactly as the HNSW tests do.
    #[tokio::test]
    async fn resolving_an_alias_plans_onto_the_qualified_project_index() {
        let workspace = workspace_handle();
        let primary = plan(RESOLVE_PRIMARY_SQL, workspace.clone(), None).await;
        assert!(
            primary.contains("idx_project_alias"),
            "the primary-identity read does not name the qualified index: {primary}"
        );
        assert!(!primary.contains("TableScan"), "plan is a scan: {primary}");

        let alias = plan(RESOLVE_ALIAS_SQL, workspace, None).await;
        assert!(
            alias.contains("idx_project_alias_lookup"),
            "the alias-table read does not name the qualified index: {alias}"
        );
        assert!(!alias.contains("TableScan"), "plan is a scan: {alias}");

        // Positive control, and the reason it is here: an assertion of the form "this plan names the
        // index" is worth nothing if it would also pass for a query that cannot use it. The same table
        // filtered on a field with no qualified index does fall back to a scan, which proves the reader
        // above is discriminating between the two rather than printing whatever the planner emits.
        let unqualified = plan(
            "SELECT * FROM project WHERE label = $alias LIMIT 1;",
            workspace_handle(),
            None,
        )
        .await;
        assert!(
            unqualified.contains("TableScan") && !unqualified.contains("idx_project_alias"),
            "the control query was expected to scan and name no index: {unqualified}"
        );
    }

    /// The listing plans onto the same qualified index on both the first page and a resumed page, so
    /// paging through a large corpus costs per page rather than per store.
    #[tokio::test]
    async fn listing_projects_plans_onto_the_qualified_index_on_both_pages() {
        let workspace = workspace_handle();
        let first = plan(LIST_FIRST_PAGE_SQL, workspace.clone(), None).await;
        let resumed = plan(
            LIST_RESUME_PAGE_SQL,
            workspace,
            Some("github.com/anten/app"),
        )
        .await;
        for (label, plan) in [("first page", first), ("resumed page", resumed)] {
            assert!(
                plan.contains("idx_project_alias"),
                "the {label} does not name the qualified index: {plan}"
            );
            assert!(!plan.contains("TableScan"), "the {label} is a scan: {plan}");
        }
    }
}
