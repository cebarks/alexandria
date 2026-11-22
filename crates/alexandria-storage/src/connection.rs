use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::access::SystemContext;
use crate::error::StorageError;

/// One store's right to hand out its one system capability.
///
/// `Arc`-identity *is* the binding: a `Database` and every handle reached through it share this object,
/// so "the same database instance through an `Arc` reference" and "a different instance" are decided by
/// pointer equality rather than by a caller-supplied path, account or workspace string. Nothing here is
/// process-global — two independent stores have independent state, so one test bootstrapping itself does
/// not change another's answer.
#[derive(Debug, Default)]
pub(crate) struct InstanceAuthority {
    issued: AtomicBool,
}

impl InstanceAuthority {
    /// Claims the single capability this instance will ever issue.
    ///
    /// The second call is refused rather than ignored: an owner that silently handed out a second
    /// capability would turn "startup issued this" back into "anybody holding a handle can assert this".
    fn claim(self: &Arc<Self>) -> Result<SystemContext, StorageError> {
        if self.issued.swap(true, Ordering::AcqRel) {
            return Err(StorageError::Stale);
        }
        Ok(SystemContext::issued_by(Arc::clone(self)))
    }

    /// Whether a capability was issued by this instance. Pointer equality on the allocation is the
    /// whole cross-instance refusal: clones and `Arc` references of one `Database` share it, and a
    /// second store never does.
    pub(crate) fn issued_to(self: &Arc<Self>, system: &SystemContext) -> bool {
        Arc::ptr_eq(self, system.issued_by_instance())
    }
}

pub struct Database {
    db: Surreal<Any>,
    /// Per actual instance, created at connect and never re-created: `Arc<Database>` clones and every
    /// `Surreal` handle taken from this value see the same issuance state because they see this same
    /// allocation.
    authority: Arc<InstanceAuthority>,
}

impl Database {
    /// Connect to an in-memory embedded SurrealDB instance (ephemeral).
    /// Used for tests and when data_dir is ":memory:".
    pub async fn connect_embedded() -> Result<Self> {
        let db = surrealdb::engine::any::connect("mem://").await?;
        db.use_ns("alexandria").use_db("default").await?;
        Ok(Self::new(db))
    }

    /// Connect to a persistent SurrealKV-backed instance on disk.
    /// Creates the directory if it doesn't exist.
    pub async fn connect_persistent(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let url = format!("surrealkv://{}", path.display());
        let db = surrealdb::engine::any::connect(&url).await?;
        db.use_ns("alexandria").use_db("default").await?;
        Ok(Self::new(db))
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

    /// Claims this instance's single [`SystemContext`]: the offline/background authority that selects
    /// without an owner, for the startup path, the CLI and the dreaming jobs.
    ///
    /// It takes `&mut self` on purpose. Ordinary request-facing code is handed the database behind an
    /// `Arc` (that is exactly what `AlexandriaServer` holds), and a shared `Arc` cannot yield `&mut`, so
    /// this is only reachable where the process still owns the connection — startup. The first claim
    /// succeeds and every later one returns [`StorageError::Stale`], including from the same value, so
    /// there is no re-mint path after the claim is spent and no `SystemContext::offline()`, `Default`
    /// or callback that would hand the same authority to a request handler for free.
    ///
    /// **Position in boot, and why there is no one-call `Database::bootstrap(data_dir)`:** the order is
    /// `connect` → embedding admission (`system_config::check_boot_embedding_metadata` /
    /// `stored_embedding_space`) → `schema::migrate` → `claim_system_authority` → admit requests, which
    /// is what `main.rs` and `dream.rs` already do and what AGENTS.md states as an invariant: a
    /// pending, corrupt or model/budget-mismatched embedding target must be refused *before* any
    /// schema write. Admission needs the live handle, and a persistent SurrealKV store cannot be
    /// opened twice to pre-check it (single-writer `LOCK`, and reopening an engine as a lock probe is
    /// forbidden), so an entry point that ran the migrations on the way to issuing the capability
    /// would foreclose that order for every later caller. Composing the four steps is Tasks 5/22; this
    /// crate offers the pieces and refuses to offer the shortcut. A startup that admits requests
    /// without claiming cannot be detected from inside storage, and cannot be recovered either — the
    /// one-shot refusal is the guarantee.
    pub fn claim_system_authority(&mut self) -> Result<SystemContext, StorageError> {
        self.authority.claim()
    }

    /// The `:memory:` convenience the crate's own authorization tests start from: connect, migrate,
    /// claim. Crate-private and test-only by construction.
    ///
    /// It is `#[cfg(test)]` for the same reason there is no persistent equivalent above: this order
    /// (connect → migrate → claim) is only safe for an ephemeral store with no embedding target to be
    /// corrupt, and a `pub` helper named `bootstrap` is exactly what a later wiring task would reach
    /// for on a real data directory. Production startup composes the four steps in the order
    /// [`Self::claim_system_authority`] documents.
    #[cfg(test)]
    pub(crate) async fn bootstrap_embedded_for_tests() -> Result<(Self, SystemContext)> {
        let mut db = Self::connect_embedded().await?;
        crate::schema::migrate(db.inner()).await?;
        let system = db.claim_system_authority().map_err(anyhow::Error::from)?;
        Ok((db, system))
    }

    /// Whether `system` is the capability *this* instance issued.
    ///
    /// The check is pointer equality on the per-instance state, so a capability obtained against one
    /// store is refused by another even though both are `mem://` stores in the same process with the same
    /// namespace and database names.
    pub(crate) fn holds_authority(&self, system: &SystemContext) -> bool {
        self.authority.issued_to(system)
    }

    /// Verify both the capability and the exact handle a repository is about to use.
    ///
    /// A `Surreal` clone has its own SDK session and may select a different namespace/database.
    /// Require the borrowed handle from this instance; a clone of an unrelated (or reconfigured)
    /// session is not proof of identity.
    pub(crate) fn verified_system_handle(
        &self,
        db: &Surreal<Any>,
        system: &SystemContext,
    ) -> Result<&Surreal<Any>, StorageError> {
        if !std::ptr::eq(self.inner(), db) || !self.holds_authority(system) {
            return Err(StorageError::Stale);
        }
        Ok(self.inner())
    }

    fn new(db: Surreal<Any>) -> Self {
        Self {
            db,
            authority: Arc::new(InstanceAuthority::default()),
        }
    }
}
