//! Enrollment: the one operation that creates an owner, and the private workspace that comes with
//! it.
//!
//! The administrator bootstrap path is an operator command, never anonymous signup, and it has to
//! survive being run twice — the terminal scrolled, the receipt was lost, the caller wants the ids
//! again. A bootstrap writes two rows (the account and its workspace) inside **one** engine
//! transaction, so this producer cannot leave half of an owner behind. The [`OperationId`] the caller
//! supplies is stamped on the account together with a fingerprint of the request that used it, and
//! that pair — not the key alone — decides whether a second call is a retry or a different request
//! claiming somebody else's anchor.
//!
//! The login is normalised here rather than trusted from the caller, because `account.login`'s own
//! assertion rejects an unnormalised value, and a login stored as-typed is a second owner that no
//! later authentication can reach.

use crate::error::StorageError;
use crate::models::auth::{
    Account, PrincipalIds, Workspace, bootstrap_request_fingerprint, normalize_enrollable_login,
};
use crate::models::identity::{AccountId, OperationId};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// The `id` column of a row a write just produced.
#[derive(SurrealValue)]
struct Created {
    id: RecordId,
}

pub struct IdentityRepo<'a> {
    db: &'a Surreal<Any>,
}

/// Turns an engine failure into the classified storage error: the message stays for the log and
/// never reaches a client.
fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

fn missing_id(what: &str) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("{what} row has no id"))
}

impl<'a> IdentityRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    /// Creates the owner `login` names, with its private workspace, and returns both ids.
    ///
    /// Idempotent per ([`OperationId`], request): re-running the same operation with the same
    /// normalised login and the same prepared `password_hash` returns the same [`PrincipalIds`] and
    /// writes nothing. Three other cases are refused, each with a different meaning:
    ///
    /// * the login exists under a **different** operation key — [`StorageError::Stale`], because the
    ///   caller meant to create an owner and one already exists;
    /// * the key matches but the **request** does not — [`StorageError::InvalidInput`], because
    ///   answering a different request with the first one's ids is the misleading-success failure the
    ///   journal rules forbid;
    /// * the key matches and the row predates fingerprinting — also `InvalidInput`: unknown history
    ///   is not permission to adopt whatever the caller is now presenting.
    ///
    /// A legitimate later password rotation (Task 4) changes `password_hash`, not the recorded
    /// request, so the original retry keeps working after a rotation and the rotated value under the
    /// old key is a different request.
    ///
    /// `password_hash` is stored verbatim: Task 4 owns Argon2id and every write that follows. A blank
    /// hash is refused here because an owner nobody can authenticate as is not a usable outcome.
    pub async fn bootstrap_owner(
        &self,
        login: &str,
        password_hash: &str,
        operation: &OperationId,
    ) -> Result<PrincipalIds, StorageError> {
        let login = normalize_enrollable_login(login)?;
        if password_hash.trim().is_empty() {
            return Err(StorageError::InvalidInput(
                "password hash must not be blank".to_string(),
            ));
        }
        let key = operation.as_str().trim();
        if key.is_empty() {
            return Err(StorageError::InvalidInput(
                "bootstrap operation key must not be blank".to_string(),
            ));
        }
        let fingerprint = bootstrap_request_fingerprint(&login, password_hash);

        if let Some(account) = self.account_by_login(&login).await? {
            return self.replay(&account, key, &fingerprint).await;
        }

        match self.create(&login, password_hash, key, &fingerprint).await {
            Ok(ids) => Ok(ids),
            // Two bootstraps of one login race; the loser's create is refused by the unique index.
            // The recovery read answers through *exactly* the rules above — the same key and the
            // same request succeed, anything else is refused — so the fallback can never turn a
            // changed request into a success, and a real engine failure is not swallowed either.
            Err(conflict) => match self.account_by_login(&login).await? {
                Some(account) => self.replay(&account, key, &fingerprint).await,
                None => Err(conflict),
            },
        }
    }

    async fn create(
        &self,
        login: &str,
        password_hash: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<PrincipalIds, StorageError> {
        // One transaction for both rows: an account without its workspace resolves to no readable
        // population, which is why nothing here may create them separately.
        let tx = self.db.clone().begin().await.map_err(internal)?;
        let account_id = {
            let mut created = tx
                .query(
                    "CREATE account SET login = $login, password_hash = $hash, bootstrap_operation \
                     = $key, bootstrap_fingerprint = $fingerprint;",
                )
                .bind(("login", login.to_string()))
                .bind(("hash", password_hash.to_string()))
                .bind(("key", key.to_string()))
                .bind(("fingerprint", fingerprint.to_string()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
            let rows: Vec<Created> = created.take(0).map_err(internal)?;
            rows.into_iter()
                .next()
                .ok_or_else(|| missing_id("account"))?
                .id
        };
        {
            let mut created = tx
                .query("CREATE workspace SET owner = $account;")
                .bind(("account", account_id.clone()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?;
            let rows: Vec<Created> = created.take(0).map_err(internal)?;
            if rows.is_empty() {
                return Err(missing_id("workspace"));
            }
        }
        tx.commit().await.map_err(internal)?;
        self.principal_ids(account_id).await
    }

    /// Answers a call whose login already exists, from that row alone.
    ///
    /// Deliberately performs no write: because `create` commits both rows atomically, a missing
    /// workspace here is a *later* deletion or corruption, and minting a replacement workspace id
    /// would hand back a `PrincipalIds` that contradicts the receipt the first run produced. The
    /// refusal is the fail-closed answer; repairing an owned corpus is a human, backup-requiring
    /// decision (Task 14's migration machinery), not something an enrollment retry may invent.
    async fn replay(
        &self,
        account: &Account,
        key: &str,
        fingerprint: &str,
    ) -> Result<PrincipalIds, StorageError> {
        if account.bootstrap_operation.as_deref() != Some(key) {
            return Err(StorageError::Stale);
        }
        let Some(recorded) = account.bootstrap_fingerprint.as_deref() else {
            return Err(StorageError::InvalidInput(
                "that bootstrap operation key owns an enrollment with no recorded request \
                 fingerprint; refusing to treat a new request as the original one"
                    .to_string(),
            ));
        };
        if recorded != fingerprint {
            return Err(StorageError::InvalidInput(
                "that bootstrap operation key already recorded a different request; retry with the \
                 prepared hash the operation used, or enroll under a new key"
                    .to_string(),
            ));
        }
        let id = account.id.clone().ok_or_else(|| missing_id("account"))?;
        // A missing workspace is fail-closed, and it is the *typed* internal case (owner decision F6):
        // the caller did nothing wrong, the answer must not identify whose state is gone, and an operator
        // reading it has to be able to tell "an enrolled owner's private state is missing" from a driver
        // hiccup without any error string being matched. Repairing it is a human, backup-requiring decision
        // (Task 14's migration machinery), not something an enrollment retry may invent.
        let workspace = self
            .workspace_of(&id)
            .await?
            .ok_or_else(StorageError::owner_workspace_lost)?;
        Ok(PrincipalIds {
            account: account.account_id()?,
            workspace: workspace.workspace_id()?,
        })
    }

    async fn principal_ids(&self, account: RecordId) -> Result<PrincipalIds, StorageError> {
        let workspace = self
            .workspace_of(&account)
            .await?
            .ok_or_else(StorageError::owner_workspace_lost)?;
        Ok(PrincipalIds {
            account: AccountId::try_from(account)?,
            workspace: workspace.workspace_id()?,
        })
    }

    async fn account_by_login(&self, login: &str) -> Result<Option<Account>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM account WHERE login = $login LIMIT 1;")
            .bind(("login", login.to_string()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Account> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    async fn workspace_of(&self, account: &RecordId) -> Result<Option<Workspace>, StorageError> {
        let mut response = self
            .db
            .query("SELECT * FROM workspace WHERE owner = $account LIMIT 1;")
            .bind(("account", account.clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<Workspace> = response.take(0).map_err(internal)?;
        Ok(rows.into_iter().next())
    }
}
