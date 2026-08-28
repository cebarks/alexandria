use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};
use surrealdb::Surreal;

use crate::models::Session;

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
                 memory_count = 0, \
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

    /// Increment memory_count and refresh ended_at on a session.
    pub async fn touch(&self, external_id: &str) -> Result<()> {
        self.db
            .query(
                "UPDATE `session` SET \
                 memory_count += 1, \
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

    /// Get all facts belonging to a session, ordered by creation time.
    pub async fn get_memories(&self, external_id: &str) -> Result<Vec<crate::models::Fact>> {
        // First get the fact IDs from the edges
        let mut response = self
            .db
            .query(
                "SELECT out AS fact_id FROM contains_session_memory \
                 WHERE in = (SELECT VALUE id FROM `session` WHERE external_id = $external_id LIMIT 1)[0]",
            )
            .bind(("external_id", external_id.to_string()))
            .await?;

        #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
        struct EdgeRow {
            fact_id: Option<RecordId>,
        }

        let rows: Vec<EdgeRow> = response.take(0)?;
        let fact_ids: Vec<String> = rows
            .into_iter()
            .filter_map(|r| r.fact_id.map(|id| crate::record_id_to_string(&id)))
            .collect();

        if fact_ids.is_empty() {
            return Ok(vec![]);
        }

        // Fetch the actual facts
        let mut facts = Vec::with_capacity(fact_ids.len());
        let repo = crate::repos::MemoryRepo::new(self.db);
        for fid in &fact_ids {
            if let Some(fact) = repo.get_fact(fid).await? {
                facts.push(fact);
            }
        }

        // Sort by created_at ascending
        facts.sort_by_key(|a| a.created_at);
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
        assert_eq!(session.memory_count, 0);

        // Add a memory
        let fact_id = memory_repo
            .create_fact("test fact", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        repo.add_memory(&session_id, &fact_id).await.unwrap();
        repo.touch("sess-001").await.unwrap();

        // Verify memory_count incremented
        let updated = repo.find_by_external_id("sess-001").await.unwrap().unwrap();
        assert_eq!(updated.memory_count, 1);

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
    async fn test_find_nonexistent_session() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = SessionRepo::new(db.inner());

        let found = repo.find_by_external_id("nonexistent").await.unwrap();
        assert!(found.is_none());
    }
}
