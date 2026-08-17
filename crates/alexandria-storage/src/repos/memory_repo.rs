use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::types::ToSql;
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
}
