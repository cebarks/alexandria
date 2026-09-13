use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

use crate::models::Session;

/// A session row flattened for display.
///
/// `memory_count` is DERIVED from the `contains_session_memory` graph traversal, not read from
/// a column: migration v006 dropped `session.memory_count` because a stored counter could never
/// be decremented by `delete_memory`. The derivation applies the same `deleted = false` filter
/// as [`SessionRepo::get_memories`], so a list can never claim a memory its detail view hides.
///
/// `ended_at` is *last activity*, not completion. `touch()` writes it every time a memory is
/// added, so an idle-but-live session also has a non-null `ended_at`; only a non-null `summary`
/// distinguishes a finalized session from an active one.
#[derive(Debug, Clone, Deserialize, SurrealValue)]
pub struct SessionSummary {
    pub external_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    /// Last activity, NOT completion — see the type-level docs.
    pub ended_at: Option<DateTime<Utc>>,
    /// Non-null implies the session was finalized.
    pub summary: Option<String>,
    pub tags: Vec<String>,
    /// Live memories, derived from the traversal.
    pub memory_count: usize,
}

pub struct SessionRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> SessionRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    /// Find a session by its external_id. Returns None if not found.
    pub async fn find_by_external_id(&self, external_id: &str) -> Result<Option<Session>> {
        let mut response = self
            .db
            .query("SELECT * FROM `session` WHERE external_id = $external_id LIMIT 1")
            .bind(("external_id", external_id.to_string()))
            .await?;
        let sessions: Vec<Session> = response.take(0)?;
        Ok(sessions.into_iter().next())
    }

    /// Create a new session. Returns the session's record ID string.
    pub async fn create(
        &self,
        external_id: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String> {
        let mut q = self
            .db
            .query(
                "CREATE `session` SET \
                 external_id = $external_id, \
                 agent_id = $agent_id, \
                 model = $model, \
                 tags = []",
            )
            .bind(("external_id", external_id.to_string()));

        if let Some(aid) = agent_id {
            q = q.bind(("agent_id", aid.to_string()));
        }
        if let Some(m) = model {
            q = q.bind(("model", m.to_string()));
        }

        let mut response = q.await?;
        let created: Option<Session> = response.take(0)?;
        let session = created.ok_or_else(|| anyhow::anyhow!("Failed to create session"))?;
        let id = session
            .id
            .ok_or_else(|| anyhow::anyhow!("Created session has no id"))?;
        Ok(crate::record_id_to_string(&id))
    }

    /// Record activity on a session by refreshing `ended_at`.
    ///
    /// This is why `ended_at` means *last activity* rather than completion: `do_store_memory`
    /// and `do_import_document` both call it after attaching a memory, so a live session has a
    /// non-null `ended_at` too. Only `finalize` writes the `summary` that marks one finished.
    pub async fn touch(&self, external_id: &str) -> Result<()> {
        self.db
            .query(
                "UPDATE `session` SET \
                 ended_at = time::now() \
                 WHERE external_id = $external_id",
            )
            .bind(("external_id", external_id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Create a contains_session_memory edge from session to fact.
    pub async fn add_memory(&self, session_id: &str, fact_id: &str) -> Result<()> {
        let session_rid = RecordId::parse_simple(session_id)?;
        let fact_rid = RecordId::parse_simple(fact_id)?;
        self.db
            .query("RELATE $sess->contains_session_memory->$fact")
            .bind(("sess", session_rid))
            .bind(("fact", fact_rid))
            .await?
            .check()?;
        Ok(())
    }

    /// Get all non-deleted facts belonging to a session, ordered by creation time.
    pub async fn get_memories(&self, external_id: &str) -> Result<Vec<crate::models::Fact>> {
        let Some(session) = self.find_by_external_id(external_id).await? else {
            return Ok(vec![]);
        };
        let Some(sess) = session.id else {
            return Ok(vec![]);
        };
        let mut response = self
            .db
            .query(
                "SELECT * FROM $sess->contains_session_memory->fact \
                 WHERE deleted = false ORDER BY created_at ASC",
            )
            .bind(("sess", sess))
            .await?;
        let facts: Vec<crate::models::Fact> = response.take(0)?;
        Ok(facts)
    }

    /// Finalize a session: set ended_at, summary, and tags.
    pub async fn finalize(
        &self,
        external_id: &str,
        summary: Option<&str>,
        tags: Option<&[String]>,
    ) -> Result<Option<Session>> {
        let mut parts = vec!["ended_at = time::now()".to_string()];

        if summary.is_some() {
            parts.push("summary = $summary".to_string());
        }
        if tags.is_some() {
            parts.push("tags = $tags".to_string());
        }

        let query = format!(
            "UPDATE `session` SET {} WHERE external_id = $external_id",
            parts.join(", ")
        );

        let mut q = self
            .db
            .query(&query)
            .bind(("external_id", external_id.to_string()));
        if let Some(s) = summary {
            q = q.bind(("summary", s.to_string()));
        }
        if let Some(t) = tags {
            q = q.bind(("tags", t.to_vec()));
        }

        let mut response = q.await?;
        let updated: Option<Session> = response.take(0)?;
        Ok(updated)
    }

    /// List sessions, most recent activity first, with a total order so paging is safe.
    ///
    /// The guaranteed ordering is `ended_at DESC, external_id ASC`:
    /// 1. Newest last-activity first. A session that has never been touched has a null
    ///    `ended_at` and sorts last, because SurrealDB orders nulls as smaller than any value.
    /// 2. Ties — which includes the entire never-touched group — break on `external_id`
    ///    ascending. `external_id` is UNIQUE-indexed, so the pair is a total order and
    ///    `LIMIT`/`START` cannot repeat or skip a row between pages.
    ///
    /// The secondary key is load-bearing, not cosmetic. `ORDER BY ended_at DESC` alone leaves
    /// the never-touched group in an unspecified relative order — every row on a fresh install —
    /// so page 2 could repeat a session page 1 showed and silently drop another.
    ///
    /// Note that `ended_at` is *last activity*, not completion; see [`SessionSummary`].
    pub async fn list(&self, limit: usize, offset: usize) -> Result<Vec<SessionSummary>> {
        // The `deleted = false` filter must sit *inside* the traversal target's parentheses.
        // `(->contains_session_memory->fact WHERE deleted = false).len()`,
        // `array::len(... WHERE ...)`, `count(... WHERE ...)` and the same with bound
        // parameters are all parse errors on SurrealDB 3.2:
        //   "Unexpected token `WHERE` expected delimiter `)`"
        let mut response = self
            .db
            .query(
                "SELECT *, \
                 (->contains_session_memory->(fact WHERE deleted = false)).len() AS memory_count \
                 FROM `session` \
                 ORDER BY ended_at DESC, external_id ASC LIMIT $limit START $offset",
            )
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64))
            .await?;
        let sessions: Vec<SessionSummary> = response.take(0)?;
        Ok(sessions)
    }

    /// Total number of sessions, ignoring pagination.
    pub async fn count(&self) -> Result<usize> {
        #[derive(Deserialize, SurrealValue)]
        struct CountRow {
            count: i64,
        }

        let mut response = self
            .db
            .query("SELECT count() FROM `session` GROUP ALL")
            .await?;
        let rows: Vec<CountRow> = response.take(0)?;
        Ok(rows.first().map(|r| r.count as usize).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;
    use std::time::Duration;

    /// `ended_at` is written by SurrealDB's `time::now()`, so two writes issued back to
    /// back can land on the same timestamp and make the DESC ordering a tie. Real gaps
    /// keep the ordering assertions meaningful.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    fn find<'a>(sessions: &'a [SessionSummary], external_id: &str) -> &'a SessionSummary {
        sessions
            .iter()
            .find(|s| s.external_id == external_id)
            .unwrap_or_else(|| {
                let ids: Vec<&str> = sessions.iter().map(|s| s.external_id.as_str()).collect();
                panic!("`{external_id}` missing from {ids:?}")
            })
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        // Create session
        let session_id = repo
            .create("sess-001", Some("test-agent"), Some("stub-model"))
            .await
            .unwrap();
        assert!(!session_id.is_empty());

        // Find by external_id
        let found = repo.find_by_external_id("sess-001").await.unwrap();
        assert!(found.is_some());
        let session = found.unwrap();
        assert_eq!(session.external_id, "sess-001");

        // Add a memory
        let fact_id = memory_repo
            .create_fact("test fact", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.add_memory(&session_id, &fact_id).await.unwrap();
        repo.touch("sess-001").await.unwrap();

        // Get memories
        let memories = repo.get_memories("sess-001").await.unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "test fact");

        // Finalize
        let finalized = repo
            .finalize(
                "sess-001",
                Some("session summary"),
                Some(&["debug".to_string()]),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(finalized.ended_at.is_some());
        assert_eq!(finalized.summary.as_deref(), Some("session summary"));
        assert_eq!(finalized.tags, vec!["debug"]);
    }

    #[tokio::test]
    async fn test_get_memories_excludes_deleted() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let session_id = repo.create("sess-002", None, None).await.unwrap();
        let keep = memory_repo
            .create_fact("kept", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let gone = memory_repo
            .create_fact("deleted", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.add_memory(&session_id, &keep).await.unwrap();
        repo.add_memory(&session_id, &gone).await.unwrap();
        memory_repo.soft_delete_fact(&gone).await.unwrap();

        let memories = repo.get_memories("sess-002").await.unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "kept");
    }

    #[tokio::test]
    async fn test_get_memories_ordered_by_created_at() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let session_id = repo.create("sess-003", None, None).await.unwrap();
        let mut ids = Vec::new();
        for content in ["first", "second", "third"] {
            ids.push(
                memory_repo
                    .create_fact(content, 0.5, &[0.1, 0.2], &[])
                    .await
                    .unwrap(),
            );
        }
        // Link in reverse so edge order differs from creation order.
        for id in ids.iter().rev() {
            repo.add_memory(&session_id, id).await.unwrap();
        }

        let memories = repo.get_memories("sess-003").await.unwrap();
        let contents: Vec<&str> = memories.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn test_find_nonexistent_session() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        let found = repo.find_by_external_id("nonexistent").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_list_returns_sessions_newest_activity_first() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        // Activity order: a, b, c, then a again — so `a` is both the first created and
        // the most recently active, which is what separates this test from list order.
        for id in ["sess-a", "sess-b", "sess-c"] {
            repo.create(id, None, None).await.unwrap();
            settle().await;
            repo.touch(id).await.unwrap();
            settle().await;
        }
        repo.touch("sess-a").await.unwrap();
        settle().await;

        let sessions = repo.list(10, 0).await.unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(ids, ["sess-a", "sess-c", "sess-b"]);
    }

    #[tokio::test]
    async fn test_list_respects_limit_and_offset() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        for id in ["sess-1", "sess-2", "sess-3"] {
            repo.create(id, None, None).await.unwrap();
            settle().await;
        }
        // Touched newest-created-first, so the DESC list starts with sess-1 (the first
        // created) — `offset = 1` must therefore drop it.
        for id in ["sess-3", "sess-2", "sess-1"] {
            repo.touch(id).await.unwrap();
            settle().await;
        }

        let all = repo.list(10, 0).await.unwrap();
        let all_ids: Vec<&str> = all.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(all_ids, ["sess-1", "sess-2", "sess-3"]);

        let page = repo.list(2, 1).await.unwrap();
        let page_ids: Vec<&str> = page.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(page.len(), 2, "LIMIT must cap the page: {page_ids:?}");
        assert_eq!(page_ids, ["sess-2", "sess-3"]);
    }

    #[tokio::test]
    async fn test_list_derives_memory_count_excluding_deleted() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        // One live fact plus one soft-deleted: only the live one may be counted.
        let mixed = repo.create("sess-mixed", None, None).await.unwrap();
        let mut mixed_ids = Vec::new();
        for content in ["keep", "deleted"] {
            mixed_ids.push(
                memory_repo
                    .create_fact(content, 0.5, &[0.1, 0.2], &[])
                    .await
                    .unwrap(),
            );
        }
        for id in &mixed_ids {
            repo.add_memory(&mixed, id).await.unwrap();
        }
        memory_repo.soft_delete_fact(&mixed_ids[1]).await.unwrap();

        // A second session with two live facts of its own, to catch a count that leaks
        // across sessions, collapses to a global total, or reports existence rather than size.
        let other = repo.create("sess-other", None, None).await.unwrap();
        for content in ["other 1", "other 2"] {
            let id = memory_repo
                .create_fact(content, 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
            repo.add_memory(&other, &id).await.unwrap();
        }

        // And a third with no memories at all.
        repo.create("sess-empty", None, None).await.unwrap();

        let sessions = repo.list(10, 0).await.unwrap();
        assert_eq!(find(&sessions, "sess-mixed").memory_count, 1);
        assert_eq!(find(&sessions, "sess-other").memory_count, 2);
        assert_eq!(find(&sessions, "sess-empty").memory_count, 0);

        // The derived count is only correct if it agrees with the traversal the UI's
        // detail view will actually render.
        for id in ["sess-mixed", "sess-other", "sess-empty"] {
            let actual = repo.get_memories(id).await.unwrap().len();
            assert_eq!(
                find(&sessions, id).memory_count,
                actual,
                "derived memory_count disagrees with get_memories for {id}"
            );
        }
    }

    #[tokio::test]
    async fn test_list_distinguishes_finalized_from_active() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        repo.create("sess-active", Some("agent-1"), Some("model-x"))
            .await
            .unwrap();
        settle().await;
        // touch() also writes ended_at, so an idle-but-visited session looks "ended".
        repo.touch("sess-active").await.unwrap();
        settle().await;

        repo.create("sess-final", None, None).await.unwrap();
        settle().await;
        repo.finalize(
            "sess-final",
            Some("a summary"),
            Some(&["tagged".to_string()]),
        )
        .await
        .unwrap();
        settle().await;

        let sessions = repo.list(10, 0).await.unwrap();
        let active = find(&sessions, "sess-active");
        let finalized = find(&sessions, "sess-final");

        // Both have last-activity timestamps; that alone must not read as finalized.
        assert!(active.ended_at.is_some(), "touch() must set ended_at");
        assert!(finalized.ended_at.is_some());
        assert!(
            active.summary.is_none(),
            "active session must not look finalized"
        );
        assert_eq!(finalized.summary.as_deref(), Some("a summary"));

        // Flattened display fields.
        assert!(active.started_at.is_some(), "schema default must survive");
        assert_eq!(active.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(active.model.as_deref(), Some("model-x"));
        assert!(active.tags.is_empty());
        assert_eq!(finalized.tags, vec!["tagged".to_string()]);
    }

    /// Pins the NULL behaviour the `/debug/sessions` ordering promise depends on: a session
    /// created but never touched has `ended_at = NONE` and must not masquerade as the most
    /// recent activity. Relative order *within* the untouched tail is unspecified.
    #[tokio::test]
    async fn test_list_sorts_never_touched_sessions_last() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        repo.create("sess-untouched-1", None, None).await.unwrap();
        repo.create("sess-touched", None, None).await.unwrap();
        settle().await;
        repo.touch("sess-touched").await.unwrap();
        settle().await;
        repo.create("sess-untouched-2", None, None).await.unwrap();

        let sessions = repo.list(10, 0).await.unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(
            ids[0], "sess-touched",
            "null ended_at must sort after real ones"
        );
        let mut tail = ids[1..].to_vec();
        tail.sort();
        assert_eq!(tail, ["sess-untouched-1", "sess-untouched-2"]);

        // The same rows still expose their schema-default started_at.
        for s in sessions
            .iter()
            .filter(|s| s.external_id.starts_with("sess-untouched"))
        {
            assert!(s.started_at.is_some());
            assert!(s.ended_at.is_none());
        }
    }

    /// The property at risk is *across* pages, so this walks the whole set with a small LIMIT.
    ///
    /// Every row here is never-touched, so `ended_at` is NULL for all of them — the worst case,
    /// and what a fresh install actually looks like. Ordering by `ended_at DESC` alone then
    /// leaves the whole result set as one unspecified tie group.
    ///
    /// The no-duplicates/no-gaps check is the operator-visible property, but on its own it does
    /// NOT catch the missing tie-break: an in-memory store happens to scan the tie group in one
    /// stable order per process, so the pages come back disjoint even though a restart reshuffles
    /// them. The exact-sequence assertion is what actually pins the contract, and it is the
    /// reason this test exists alongside the set-equality check.
    #[tokio::test]
    async fn test_list_untouched_tail_pages_in_one_stable_total_order() {
        const PAGE: usize = 3;
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        // Seven rows, so the last page is a partial one — the boundary where a reshuffle shows
        // up as a gap. Creation order is deliberately neither alphabetical nor reversed, so an
        // accidental match with the expected order cannot come from insertion order.
        let ids = ["s-m", "s-a", "s-z", "s-c", "s-f", "s-b", "s-y"];
        for id in ids {
            repo.create(id, None, None).await.unwrap();
        }
        assert_eq!(repo.count().await.unwrap(), ids.len());

        let mut concatenated: Vec<String> = Vec::new();
        let mut offset = 0;
        loop {
            let page = repo.list(PAGE, offset).await.unwrap();
            if page.is_empty() {
                break;
            }
            assert!(
                page.len() <= PAGE,
                "LIMIT must cap every page, got {} at offset {offset}",
                page.len()
            );
            concatenated.extend(page.into_iter().map(|s| s.external_id));
            offset += PAGE;
            assert!(offset <= ids.len() * PAGE, "pagination did not terminate");
        }

        let mut seen = concatenated.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            concatenated.len(),
            "a session appeared on two pages: {concatenated:?}"
        );
        let mut universe = ids.to_vec();
        universe.sort();
        assert_eq!(
            seen, universe,
            "the pages must cover every session exactly once"
        );
        assert_eq!(
            concatenated, universe,
            "the tie group must be ordered by external_id, not by whatever the scan yielded"
        );
    }

    /// The tie-break must only break ties. A touched session still outranks every never-touched
    /// one regardless of how its external_id sorts, and touched rows keep `ended_at DESC` order.
    #[tokio::test]
    async fn test_list_secondary_key_does_not_override_activity_order() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        for id in ["z-active", "m-active", "a-quiet-1", "b-quiet-2"] {
            repo.create(id, None, None).await.unwrap();
        }
        // Newest activity first, and named so alphabetical order is the reverse of it — if the
        // secondary key leaked into the non-null group this cannot pass.
        for id in ["z-active", "m-active"] {
            settle().await;
            repo.touch(id).await.unwrap();
        }

        let sessions = repo.list(10, 0).await.unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(
            ids,
            ["m-active", "z-active", "a-quiet-1", "b-quiet-2"],
            "touched rows lead by ended_at DESC, then the null tail by external_id ASC"
        );
    }

    #[tokio::test]
    async fn test_count_matches_list_len() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        assert_eq!(repo.count().await.unwrap(), 0);
        // An empty page must deserialise to an empty vec rather than erroring — the sessions
        // list is the first thing a fresh install renders.
        assert!(repo.list(10, 0).await.unwrap().is_empty());
        assert!(repo.list(10, 5).await.unwrap().is_empty());

        for id in ["sess-1", "sess-2", "sess-3"] {
            repo.create(id, None, None).await.unwrap();
            settle().await;
        }

        assert_eq!(repo.count().await.unwrap(), 3);
        // A LIMIT beyond the row count must not change the total.
        assert_eq!(repo.list(1000, 0).await.unwrap().len(), 3);
        // count() is the total, not the page size.
        assert_eq!(repo.list(1, 0).await.unwrap().len(), 1);
        assert_eq!(repo.count().await.unwrap(), 3);
    }
}
