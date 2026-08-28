use std::path::Path;

use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

pub struct Database {
    db: Surreal<Any>,
}

impl Database {
    /// Connect to an in-memory embedded SurrealDB instance (ephemeral).
    /// Used for tests and when data_dir is ":memory:".
    pub async fn connect_embedded() -> Result<Self> {
        let db = surrealdb::engine::any::connect("mem://").await?;
        db.use_ns("alexandria").use_db("default").await?;
        Ok(Self { db })
    }

    /// Connect to a persistent SurrealKV-backed instance on disk.
    /// Creates the directory if it doesn't exist.
    pub async fn connect_persistent(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let url = format!("surrealkv://{}", path.display());
        let db = surrealdb::engine::any::connect(&url).await?;
        db.use_ns("alexandria").use_db("default").await?;
        Ok(Self { db })
    }

    /// Connect based on a data_dir config value.
    /// ":memory:" → ephemeral, anything else → persistent SurrealKV.
    pub async fn connect(data_dir: &Path) -> Result<Self> {
        if data_dir.to_str() == Some(":memory:") {
            Self::connect_embedded().await
        } else {
            Self::connect_persistent(data_dir).await
        }
    }

    pub fn is_connected(&self) -> bool {
        true // embedded connection is always valid after construction
    }

    pub fn inner(&self) -> &Surreal<Any> {
        &self.db
    }
}
