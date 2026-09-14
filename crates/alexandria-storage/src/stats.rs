use anyhow::Result;
use serde::Serialize;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::SurrealValue;

/// Aggregate counts for the debug dashboard.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Stats {
    pub fact_count: usize,
    pub deleted_fact_count: usize,
    pub cluster_count: usize,
    pub edge_count: usize,
    pub raw_count: usize,
    pub session_count: usize,
}

async fn count_table(db: &Surreal<Any>, table: &str, where_clause: &str) -> Result<usize> {
    // `table` is interpolated verbatim, so reserved-word tables must arrive already
    // backticked by the caller (see the `session` entry in `gather`).
    #[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
    struct CountRow {
        count: i64,
    }
    let query = format!("SELECT count() FROM {table} {where_clause} GROUP ALL");
    let mut response = db.query(&query).await?;
    let rows: Vec<CountRow> = response.take(0)?;
    Ok(rows.first().map(|r| r.count as usize).unwrap_or(0))
}

/// Gather aggregate counts across all core tables.
pub async fn gather(db: &Surreal<Any>) -> Result<Stats> {
    Ok(Stats {
        fact_count: count_table(db, "fact", "WHERE deleted = false").await?,
        deleted_fact_count: count_table(db, "fact", "WHERE deleted = true").await?,
        cluster_count: count_table(db, "cluster", "").await?,
        edge_count: count_table(db, "memory_edge", "").await?,
        raw_count: count_table(db, "raw", "").await?,
        session_count: count_table(db, "`session`", "").await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;

    #[tokio::test]
    async fn test_gather_stats_empty_db() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let stats = gather(db.inner()).await.unwrap();
        assert_eq!(stats.fact_count, 0);
        assert_eq!(stats.cluster_count, 0);
        assert_eq!(stats.session_count, 0);
    }

    #[tokio::test]
    async fn test_gather_counts_sessions() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = crate::repos::SessionRepo::new(db.inner());
        repo.create("sess-1", None, None).await.unwrap();
        repo.create("sess-2", None, None).await.unwrap();

        // `session` is a SurrealDB reserved word, so this also proves count_table()'s
        // interpolation path passes the backticked name through unescaped.
        let stats = gather(db.inner()).await.unwrap();
        assert_eq!(stats.session_count, 2);
        assert_eq!(stats.fact_count, 0);
    }

    #[tokio::test]
    async fn test_gather_stats_with_data() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = crate::repos::MemoryRepo::new(db.inner());
        let id = repo.create_fact("x", 0.5, &[0.1], &[]).await.unwrap();
        repo.create_fact("y", 0.5, &[0.2], &[]).await.unwrap();
        repo.soft_delete_fact(&id).await.unwrap();

        let stats = gather(db.inner()).await.unwrap();
        assert_eq!(stats.fact_count, 1);
        assert_eq!(stats.deleted_fact_count, 1);
    }
}
