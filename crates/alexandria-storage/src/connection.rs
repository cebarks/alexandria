use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use anyhow::Result;

pub struct Database {
    db: Surreal<Any>,
}

impl Database {
    /// Connect to an in-memory embedded SurrealDB instance
    pub async fn connect_embedded() -> Result<Self> {
        let db = surrealdb::engine::any::connect("mem://").await?;
        db.use_ns("alexandria").use_db("default").await?;
        Ok(Self { db })
    }

    pub fn is_connected(&self) -> bool {
        true // embedded connection is always valid after construction
    }

    pub fn inner(&self) -> &Surreal<Any> {
        &self.db
    }
}
