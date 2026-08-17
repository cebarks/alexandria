use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use anyhow::Result;

pub async fn bootstrap(db: &Surreal<Any>) -> Result<()> {
    db.query(include_str!("schema.surql")).await?.check()?;
    Ok(())
}
