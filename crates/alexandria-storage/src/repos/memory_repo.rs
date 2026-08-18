use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::{SurrealValue, ToSql};
use surrealdb::Surreal;

use crate::models::Fact;

pub struct MemoryRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> MemoryRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    pub async fn create_fact(
        &self,
        content: &str,
        confidence: f64,
        embedding: &[f32],
        tags: &[String],
    ) -> Result<String> {
        let mut response = self
            .db
            .query(
                "CREATE fact SET \
                 content = $content, \
                 confidence = $confidence, \
                 embedding = $embedding, \
                 tags = $tags, \
                 deleted = false",
            )
            .bind(("content", content.to_string()))
            .bind(("confidence", confidence))
            .bind(("embedding", embedding.to_vec()))
            .bind(("tags", tags.to_vec()))
            .await?;

        let created: Option<Fact> = response.take(0)?;
        let fact = created.ok_or_else(|| anyhow::anyhow!("Failed to create fact"))?;
        let id = fact.id.ok_or_else(|| anyhow::anyhow!("Created fact has no id"))?;
        Ok(id.to_sql())
    }

    pub async fn get_fact(&self, id: &str) -> Result<Option<Fact>> {
        let mut response = self
            .db
            .query("SELECT * FROM type::record($id)")
            .bind(("id", id.to_string()))
            .await?;
        let fact: Option<Fact> = response.take(0)?;
        Ok(fact)
    }

    pub async fn soft_delete_fact(&self, id: &str) -> Result<()> {
        self.db
            .query("UPDATE type::record($id) SET deleted = true")
            .bind(("id", id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Update a fact's content and/or tags. Returns the updated fact.
    pub async fn update_fact(
        &self,
        id: &str,
        content: Option<&str>,
        tags: Option<&[String]>,
        confidence: Option<f64>,
        embedding: Option<&[f32]>,
    ) -> Result<Option<Fact>> {
        let mut parts = Vec::new();
        if content.is_some() {
            parts.push("content = $content");
        }
        if tags.is_some() {
            parts.push("tags = $tags");
        }
        if confidence.is_some() {
            parts.push("confidence = $confidence");
        }
        if embedding.is_some() {
            parts.push("embedding = $embedding");
        }

        if parts.is_empty() {
            return self.get_fact(id).await;
        }

        let set_clause = parts.join(", ");
        let query = format!("UPDATE type::record($id) SET {set_clause}");

        let mut q = self.db.query(&query).bind(("id", id.to_string()));
        if let Some(c) = content {
            q = q.bind(("content", c.to_string()));
        }
        if let Some(t) = tags {
            q = q.bind(("tags", t.to_vec()));
        }
        if let Some(conf) = confidence {
            q = q.bind(("confidence", conf));
        }
        if let Some(emb) = embedding {
            q = q.bind(("embedding", emb.to_vec()));
        }

        let mut response = q.await?;
        let updated: Option<Fact> = response.take(0)?;
        Ok(updated)
    }

    /// List facts with optional content search, tag filter, and deleted-inclusion.
    /// `search` does a case-insensitive substring match against content.
    pub async fn list(
        &self,
        search: Option<&str>,
        tag: Option<&str>,
        include_deleted: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Fact>> {
        let mut conditions = Vec::new();
        if !include_deleted {
            conditions.push("deleted = false".to_string());
        }
        if search.is_some() {
            conditions.push("string::lowercase(content) CONTAINS string::lowercase($search)".to_string());
        }
        if tag.is_some() {
            conditions.push("$tag IN tags".to_string());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let query = format!(
            "SELECT * FROM fact {where_clause} ORDER BY created_at DESC LIMIT $limit START $offset"
        );

        let mut q = self
            .db
            .query(&query)
            .bind(("limit", limit as i64))
            .bind(("offset", offset as i64));
        if let Some(s) = search {
            q = q.bind(("search", s.to_string()));
        }
        if let Some(t) = tag {
            q = q.bind(("tag", t.to_string()));
        }

        let mut response = q.await?;
        let facts: Vec<Fact> = response.take(0)?;
        Ok(facts)
    }

    /// Count facts matching the same filters as `list` (ignoring limit/offset).
    pub async fn count(
        &self,
        search: Option<&str>,
        tag: Option<&str>,
        include_deleted: bool,
    ) -> Result<usize> {
        let mut conditions = Vec::new();
        if !include_deleted {
            conditions.push("deleted = false".to_string());
        }
        if search.is_some() {
            conditions.push("string::lowercase(content) CONTAINS string::lowercase($search)".to_string());
        }
        if tag.is_some() {
            conditions.push("$tag IN tags".to_string());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let query = format!("SELECT count() FROM fact {where_clause} GROUP ALL");

        let mut q = self.db.query(&query);
        if let Some(s) = search {
            q = q.bind(("search", s.to_string()));
        }
        if let Some(t) = tag {
            q = q.bind(("tag", t.to_string()));
        }

        #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
        struct CountRow {
            count: i64,
        }

        let mut response = q.await?;
        let rows: Vec<CountRow> = response.take(0)?;
        Ok(rows.first().map(|r| r.count as usize).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_list_and_count_facts() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = MemoryRepo::new(db.inner());

        repo.create_fact("alpha content", 0.5, &[0.1, 0.2], &["tag1".to_string()]).await.unwrap();
        repo.create_fact("beta content", 0.5, &[0.3, 0.4], &["tag2".to_string()]).await.unwrap();
        let deleted_id = repo.create_fact("gamma content", 0.5, &[0.5, 0.6], &[]).await.unwrap();
        repo.soft_delete_fact(&deleted_id).await.unwrap();

        // Default: excludes deleted
        let all = repo.list(None, None, false, 10, 0).await.unwrap();
        assert_eq!(all.len(), 2);

        // include_deleted = true picks up all 3
        let with_deleted = repo.list(None, None, true, 10, 0).await.unwrap();
        assert_eq!(with_deleted.len(), 3);

        // search filters by content substring
        let searched = repo.list(Some("alpha"), None, false, 10, 0).await.unwrap();
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].content, "alpha content");

        // tag filters
        let tagged = repo.list(None, Some("tag2"), false, 10, 0).await.unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].content, "beta content");

        // count matches list length for same filters
        let count = repo.count(None, None, false).await.unwrap();
        assert_eq!(count, 2);

        // limit/offset paginate
        let page1 = repo.list(None, None, false, 1, 0).await.unwrap();
        let page2 = repo.list(None, None, false, 1, 1).await.unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page2.len(), 1);
        assert_ne!(page1[0].content, page2[0].content);
    }
}
