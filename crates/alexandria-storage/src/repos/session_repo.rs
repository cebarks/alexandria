use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::RecordId;

use crate::models::{Session, SessionListItem};

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

    /// Find a session by external_id, creating it if missing. Returns the record ID string.
    /// `agent_id` / `model` are set on create and fill still-empty fields on an existing
    /// session; a value already stored is never overwritten.
    pub async fn find_or_create(
        &self,
        external_id: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String> {
        let Some(session) = self.find_by_external_id(external_id).await? else {
            return self.create(external_id, agent_id, model).await;
        };
        let id = session
            .id
            .map(|id| crate::record_id_to_string(&id))
            .ok_or_else(|| anyhow::anyhow!("Session has no id"))?;

        let fill_agent = session.agent_id.is_none().then_some(agent_id).flatten();
        let fill_model = session.model.is_none().then_some(model).flatten();
        if fill_agent.is_some() || fill_model.is_some() {
            let mut q = self
                .db
                .query(
                    "UPDATE `session` SET \
                     agent_id = $agent_id ?? agent_id, \
                     model = $model ?? model \
                     WHERE external_id = $external_id",
                )
                .bind(("external_id", external_id.to_string()));
            if let Some(a) = fill_agent {
                q = q.bind(("agent_id", a.to_string()));
            }
            if let Some(m) = fill_model {
                q = q.bind(("model", m.to_string()));
            }
            q.await?.check()?;
        }
        Ok(id)
    }

    /// Refresh ended_at on a session.
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

    /// List sessions newest-first with a live non-deleted memory count. Each filter is
    /// applied only when `Some`; `finalized` selects sessions with (`true`) or without
    /// (`false`) a summary.
    pub async fn list(
        &self,
        agent_id: Option<&str>,
        tag: Option<&str>,
        finalized: Option<bool>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SessionListItem>> {
        let mut response = self
            .db
            .query(
                "SELECT *,                  count(->contains_session_memory->(fact WHERE deleted = false)) AS memory_count                  FROM `session`                  WHERE ($agent_id IS NONE OR agent_id = $agent_id)                  AND ($tag IS NONE OR $tag IN tags)                  AND ($finalized IS NONE OR (summary IS NOT NONE) = $finalized)                  ORDER BY started_at DESC LIMIT $limit START $offset",
            )
            .bind(("agent_id", agent_id.map(str::to_string)))
            .bind(("tag", tag.map(str::to_string)))
            .bind(("finalized", finalized))
            .bind(("limit", limit))
            .bind(("offset", offset))
            .await?;
        let sessions: Vec<SessionListItem> = response.take(0)?;
        Ok(sessions)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

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
    async fn test_find_or_create_is_idempotent() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        let first = repo.find_or_create("sess-004", None, None).await.unwrap();
        let second = repo.find_or_create("sess-004", None, None).await.unwrap();
        assert_eq!(first, second);

        let mut response = db
            .inner()
            .query("SELECT * FROM `session` WHERE external_id = 'sess-004'")
            .await
            .unwrap();
        let rows: Vec<Session> = response.take(0).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn test_find_or_create_fills_agent_and_model_if_empty() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        // First seen without metadata.
        repo.find_or_create("sess-005", None, None).await.unwrap();
        let s = repo.find_by_external_id("sess-005").await.unwrap().unwrap();
        assert_eq!(s.agent_id, None);
        assert_eq!(s.model, None);

        // Acquires it later.
        repo.find_or_create("sess-005", Some("claude-code"), None)
            .await
            .unwrap();
        let s = repo.find_by_external_id("sess-005").await.unwrap().unwrap();
        assert_eq!(s.agent_id.as_deref(), Some("claude-code"));
        assert_eq!(s.model, None);

        // Set values are never overwritten; still-empty ones are filled.
        repo.find_or_create("sess-005", Some("other"), Some("haiku"))
            .await
            .unwrap();
        let s = repo.find_by_external_id("sess-005").await.unwrap().unwrap();
        assert_eq!(s.agent_id.as_deref(), Some("claude-code"));
        assert_eq!(s.model.as_deref(), Some("haiku"));

        // Set on create when given.
        repo.find_or_create("sess-006", Some("pi"), Some("sonnet"))
            .await
            .unwrap();
        let s = repo.find_by_external_id("sess-006").await.unwrap().unwrap();
        assert_eq!(s.agent_id.as_deref(), Some("pi"));
        assert_eq!(s.model.as_deref(), Some("sonnet"));
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
    async fn test_list_filters_orders_and_counts() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());
        let memory_repo = crate::repos::MemoryRepo::new(db.inner());

        let a = repo.create("sess-a", Some("pi"), None).await.unwrap();
        let keep = memory_repo
            .create_fact("kept", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let gone = memory_repo
            .create_fact("gone", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.add_memory(&a, &keep).await.unwrap();
        repo.add_memory(&a, &gone).await.unwrap();
        memory_repo.soft_delete_fact(&gone).await.unwrap();

        repo.create("sess-b", Some("claude-code"), None)
            .await
            .unwrap();
        repo.finalize("sess-b", Some("done"), Some(&["review".to_string()]))
            .await
            .unwrap();

        repo.create("sess-c", Some("claude-code"), None)
            .await
            .unwrap();

        // Newest first, live count excludes soft-deleted facts.
        let all = repo.list(None, None, None, 20, 0).await.unwrap();
        let ids: Vec<&str> = all.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(ids, ["sess-c", "sess-b", "sess-a"]);
        assert_eq!(all[2].memory_count, 1);
        assert_eq!(all[0].memory_count, 0);

        let by_agent = repo
            .list(Some("claude-code"), None, None, 20, 0)
            .await
            .unwrap();
        let ids: Vec<&str> = by_agent.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(ids, ["sess-c", "sess-b"]);

        let by_tag = repo.list(None, Some("review"), None, 20, 0).await.unwrap();
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].external_id, "sess-b");

        let open = repo.list(None, None, Some(false), 20, 0).await.unwrap();
        let ids: Vec<&str> = open.iter().map(|s| s.external_id.as_str()).collect();
        assert_eq!(ids, ["sess-c", "sess-a"]);

        let closed = repo.list(None, None, Some(true), 20, 0).await.unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].external_id, "sess-b");

        let page = repo.list(None, None, None, 1, 1).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].external_id, "sess-b");
    }
}
