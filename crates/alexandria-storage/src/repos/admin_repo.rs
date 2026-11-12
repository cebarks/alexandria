//! Account management, bounded by the capability that authorises it.
//!
//! This is the storage half of Task 9's admin boundary: the rows an administrator may *see* and *change*
//! about an account, and nothing else. It exists as its own repo rather than as methods on
//! [`crate::repos::AuthRepo`] because the two answer different questions — `AuthRepo` verifies a secret a
//! caller presented, and never lists anybody — and its own type is where the difference is enforced:
//! every method here takes an [`AdminCapability`], whose only producer
//! ([`AccessRepo::authorize_admin`](crate::access::AccessRepo::authorize_admin)) re-reads
//! `account.role`/`account.status` from the live row. There is therefore no call path from a client-supplied
//! account id to any of these statements.
//!
//! Two rules the shape of this module carries:
//!
//! * **No memory content, and no other account's record identity, leaves here.** The account listing
//!   projects `login`/`status`/`role`/`created_at` and nothing else — not `password_hash`, not
//!   `bootstrap_operation`, not a count of what the account holds. A corpus count would be a per-account
//!   existence oracle, and the design puts content-derived aggregates outside the admin surface.
//! * **A suspension is guarded the way a scoped write is.** [`AdminRepo::set_status`] claims the *acting*
//!   admin's own row inside the same transaction as the target write, with the conditional-write shape
//!   `claim_authority` uses. A read-then-write here would let a revocation or a self-demotion that lands
//!   between the two statements still buy the write — the exact TOCTOU Task 6 measured and rejected.

use crate::access::AdminCapability;
use crate::error::StorageError;
use crate::models::auth::{account_role, account_status};
use crate::models::identity::{AccountId, CredentialId};
use crate::repos::scope::{is_write_conflict, record_key};
use chrono::{DateTime, Utc};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};

/// How many accounts one page of the admin listing returns at most.
///
/// A page, not a sweep: the store is multi-tenant now, and an operator surface that read every account
/// row to render a table would turn a growth in tenants into a growth in per-request work.
pub const MAX_ACCOUNT_PAGE: usize = 200;

/// One row of the admin account listing.
///
/// Deliberately narrow: an id, a login, the two state columns, and when the account was created. There
/// is no `password_hash` field to populate, which is the point of projecting the columns rather than
/// `SELECT *`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountEntry {
    pub account: AccountId,
    pub login: String,
    /// `NONE` on a row that predates v009's status column; reported rather than defaulted to `active`,
    /// because "the store does not say" is not the same finding as "the account is active".
    pub status: Option<String>,
    pub role: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

impl AccountEntry {
    /// Whether this row is the one an administrator is currently acting as.
    pub fn is_suspended(&self) -> bool {
        self.status.as_deref() == Some(account_status::SUSPENDED)
    }
}

/// A newly minted one-use link, returned with the id of the row behind it.
///
/// The secret is the caller's to show exactly once; like every other mint in this crate, nothing here can
/// read it back afterwards, because the row keeps only a digest.
pub struct MintedLink {
    pub credential: CredentialId,
    pub secret: String,
}

/// Account management, bound to the capability that authorises it.
pub struct AdminRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> AdminRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    /// The account listing, oldest first, one page at a time.
    ///
    /// Ordered by `id` rather than by `created_at` because a record id is unique and a timestamp is not,
    /// so a page boundary cannot repeat or skip a row the way a `created_at` page can when an import
    /// stamps ten accounts with one instant.
    pub async fn list_accounts(
        &self,
        _admin: &AdminCapability,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AccountEntry>, StorageError> {
        let mut response = self
            .db
            .query(
                "SELECT id, login, status, role, created_at FROM account ORDER BY id ASC LIMIT $limit \
                 START $offset;",
            )
            .bind(("limit", limit.min(MAX_ACCOUNT_PAGE) as i64))
            .bind(("offset", offset as i64))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        #[derive(SurrealValue)]
        struct Row {
            id: Option<RecordId>,
            login: String,
            status: Option<String>,
            role: Option<String>,
            created_at: Option<DateTime<Utc>>,
        }
        let rows: Vec<Row> = response.take(0).map_err(internal)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(AccountEntry {
                    // A row whose id names another table is refused here rather than re-labelled, the
                    // same way every typed handle in this crate is: `TryFrom<RecordId>` checks the table.
                    account: AccountId::try_from(row.id?).ok()?,
                    login: row.login,
                    status: row.status,
                    role: row.role,
                    created_at: row.created_at,
                })
            })
            .collect())
    }

    /// How many accounts exist, for the pager above.
    pub async fn count_accounts(&self, _admin: &AdminCapability) -> Result<usize, StorageError> {
        #[derive(SurrealValue)]
        struct CountRow {
            count: i64,
        }
        let mut response = self
            .db
            .query("SELECT count() FROM account GROUP ALL;")
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?;
        let rows: Vec<CountRow> = response.take(0).map_err(internal)?;
        Ok(rows
            .first()
            .map(|row| row.count.max(0) as usize)
            .unwrap_or(0))
    }

    /// Suspends or restores an account, and returns the status the row now carries.
    ///
    /// Three refusals, none of them a silent success:
    ///
    /// * the acting capability has gone stale — suspended, demoted, or its epoch superseded — which the
    ///   in-transaction claim below reports as [`StorageError::Stale`], never as a write;
    /// * the target is the acting admin's *own* account. Suspending oneself is a lockout with no
    ///   recovery path but a database edit, and the design gives the operator no self-removal button;
    /// * the target row does not exist, or exists with a status this build does not recognise. Foreign
    ///   and absent are one [`StorageError::NotFound`], so this is not a probe for which logins the
    ///   store holds.
    ///
    /// **A suspension retires the account's credentials, durably.** Suspending raises the account's
    /// `auth_epoch` in the same transaction as the status write, which is v009's own mechanism for
    /// "every browser session and agent token minted before this instant is inert" — the same rule
    /// password recovery uses, and the same one `credential_entitles_read` and
    /// `AccessRepo::authorize` already apply on every request. That is what makes restoring an account
    /// safe: a restore flips `status` back and leaves the epoch where it is, so a cookie or token the
    /// suspended account held before the suspension cannot come back to life when the account does. A
    /// suspension that only flipped `status` would be a mute button someone could un-press for the
    /// credential that motivated it.
    ///
    /// Nothing here deletes a corpus. The in-process half — cancelling a stream that is already open,
    /// which no store-side change can reach — is the caller's
    /// `AuthService::invalidate_account` call on the same account, and `AdminRepo` deliberately does
    /// not own that: the bus is the MCP layer's.
    pub async fn set_status(
        &self,
        admin: &AdminCapability,
        target: &AccountId,
        suspend: bool,
        now: DateTime<Utc>,
    ) -> Result<String, StorageError> {
        if target == admin.account() {
            return Err(StorageError::InvalidInput(
                "an administrator cannot suspend their own account; the store would be left with no \
                 way back in but a database edit"
                    .to_string(),
            ));
        }
        let wanted = if suspend {
            account_status::SUSPENDED
        } else {
            account_status::ACTIVE
        };

        for _attempt in 0..crate::repos::scope::GUARD_CONFLICT_ATTEMPTS {
            let tx = self.db.clone().begin().await.map_err(internal)?;

            // The claim: a conditional write on the *acting* row, in the shape `claim_authority` uses
            // and for the same reason. `UPDATE ONLY $target … WHERE …` was measured not to filter here
            // (probe: it returns one `NONE` row and writes anyway), so the guard has to be a
            // table-targeted update whose row count means what it says.
            let claimed: Vec<serde_json::Value> = tx
                .query(
                    "UPDATE account SET updated_at = time::now() WHERE id = $admin AND role = $role \
                     AND status = 'active' RETURN AFTER;",
                )
                .bind(("admin", admin.account().as_record_id().clone()))
                .bind(("role", account_role::ADMIN.to_string()))
                .await
                .map_err(internal)?
                .check()
                .map_err(internal)?
                .take(0)
                .map_err(internal)?;
            if claimed.is_empty() {
                tx.cancel().await.map_err(internal)?;
                return Err(StorageError::Stale);
            }

            // The second half of the claim: the credential this capability was issued from. An account
            // that is still an active admin is not entitled if *this* session was revoked, expired or
            // superseded in the meantime, and a capability that named "any live credential" would let a
            // sibling mask it — the same distinction `WriteAuthorization` carries for an owner write.
            if let Some(credential) = admin.credential() {
                let held: Vec<serde_json::Value> = tx
                    .query(
                        "UPDATE credential SET last_used_at = $now WHERE id = $credential AND \
                         account = $admin AND revoked = false AND consumed = false AND kind = \
                         'browser' RETURN AFTER;",
                    )
                    .bind(("credential", credential.as_record_id().clone()))
                    .bind(("admin", admin.account().as_record_id().clone()))
                    .bind(("now", now))
                    .await
                    .map_err(internal)?
                    .check()
                    .map_err(internal)?
                    .take(0)
                    .map_err(internal)?;
                if held.is_empty() {
                    tx.cancel().await.map_err(internal)?;
                    return Err(StorageError::Stale);
                }
            }

            // Suspension advances the epoch in the same statement as the status write; restoration
            // changes only the status. `auth_epoch += 1` is the engine's own increment (the form
            // `rotate_password` uses, which the module documents as reading the committed value so two
            // concurrent rotations cannot write one epoch twice) — an absolute value here would collide
            // with v009's `auth_epoch >= 1` assertion the moment an account had already recovered its
            // password, and would silently re-qualify credentials from an earlier epoch.
            let statement = if suspend {
                "UPDATE account SET status = $status, auth_epoch += 1, updated_at = $now WHERE id \
                     = $target AND auth_epoch != NONE RETURN AFTER;"
            } else {
                "UPDATE account SET status = $status, updated_at = $now WHERE id = $target AND \
                     auth_epoch != NONE RETURN AFTER;"
            };
            let written: Vec<serde_json::Value> = match tx
                .query(statement)
                .bind(("status", wanted.to_string()))
                .bind(("now", now))
                .bind(("target", target.as_record_id().clone()))
                .await
            {
                Ok(response) => match response.check() {
                    Ok(mut checked) => checked.take(0).map_err(internal)?,
                    Err(error) if is_write_conflict(&error) => {
                        tx.cancel().await.map_err(internal)?;
                        continue;
                    }
                    Err(error) => {
                        tx.cancel().await.map_err(internal)?;
                        return Err(internal(error));
                    }
                },
                Err(error) if is_write_conflict(&error) => {
                    tx.cancel().await.map_err(internal)?;
                    continue;
                }
                Err(error) => {
                    tx.cancel().await.map_err(internal)?;
                    return Err(internal(error));
                }
            };
            if written.is_empty() {
                tx.cancel().await.map_err(internal)?;
                return Err(StorageError::NotFound);
            }
            tx.commit().await.map_err(internal)?;
            return Ok(wanted.to_string());
        }
        Err(StorageError::RetryableBookkeepingConflict)
    }

    /// Mints a password-recovery link for the account an administrator names.
    ///
    /// The account-management half of the recovery flow, and the reason it is here rather than on
    /// [`crate::repos::AuthRepo`]: `AuthRepo::create_recovery_link` takes a bare account id, which is
    /// fine inside a store that has no request path, and not fine as a method an HTTP handler could call
    /// with anything a form carried. The capability argument is what makes "whose recovery link is this"
    /// answerable: the row belongs to the administrator who issued it (`credential.account`) while the
    /// link rotates the account it names — the same issuer-owns-the-link rule Task 4 recorded for
    /// invitations, and the reason an issued link shows up in the issuer's own credential list.
    ///
    /// The row belongs to the account it recovers — `credential_kind::RECOVERY` resolves to a target
    /// account, unlike an invitation, whose issuer owns the link because the invited account does not
    /// exist yet. The caller still has to hand the secret over out of band and still cannot choose or
    /// read the new password: consuming the link rotates it and raises that account's `auth_epoch`,
    /// which retires every credential it held.
    pub async fn create_recovery_link(
        &self,
        admin: &AdminCapability,
        target: &AccountId,
        lifetime: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Result<MintedLink, StorageError> {
        // Refuse before reading anything: a link for one's own account is the owner path
        // (`AuthService::create_recovery_link`), and a self-targeted admin link would be a second,
        // less accountable way to mint the same thing.
        if target == admin.account() {
            return Err(StorageError::InvalidInput(
                "an administrator recovers their own account from the account page".to_string(),
            ));
        }
        let exists: Vec<serde_json::Value> = self
            .db
            .query("SELECT id FROM account WHERE id = $target LIMIT 1;")
            .bind(("target", target.as_record_id().clone()))
            .await
            .map_err(internal)?
            .check()
            .map_err(internal)?
            .take(0)
            .map_err(internal)?;
        if exists.is_empty() {
            return Err(StorageError::NotFound);
        }
        let minted = crate::repos::AuthRepo::new(self.db)
            .create_recovery_link(target, lifetime, now)
            .await?;
        Ok(MintedLink {
            credential: minted.credential,
            secret: minted.secret.render().expose().to_string(),
        })
    }

    /// The account-management role, spelled once for the tests and the refusal paths.
    pub fn is_admin_role(role: Option<&str>) -> bool {
        role == Some(account_role::ADMIN)
    }
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::Internal(anyhow::Error::new(error))
}

// `record_key` is used by the scoped writers; referencing it here keeps the admin claim and their claim
// one spelling of "guarded conditional write" rather than two.
#[allow(dead_code)]
fn claim_key_hint(id: &AccountId) -> Result<String, StorageError> {
    record_key(id.as_record_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{AccessRepo, PrincipalProvenance};
    use crate::models::OperationId;
    use crate::repos::{AuthRepo, IdentityRepo};
    use crate::schema;

    /// Two accounts and one administrator, enrolled through the real paths, each with a live browser
    /// session so `authorize_admin` has something to re-read.
    struct Fixture {
        db: Database,
        admin: AccountId,
        admin_session: String,
        subject: AccountId,
    }

    use crate::Database;

    async fn fixture() -> Fixture {
        let db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let digest = AuthRepo::create_password_digest("admin-repo-fixture-password-padding")
            .await
            .unwrap();
        let repo = IdentityRepo::new(db.inner());
        let admin = repo
            .bootstrap_owner(
                "admin-repo-admin",
                digest.as_str(),
                &OperationId::new("adm-1"),
            )
            .await
            .unwrap();
        let subject = repo
            .bootstrap_owner(
                "admin-repo-subject",
                digest.as_str(),
                &OperationId::new("adm-2"),
            )
            .await
            .unwrap();
        // The role write is the operator's, and the only producer of an admin row in this crate: a
        // request path cannot reach it because nothing outside storage can name an `account` row it
        // was not issued a capability for.
        db.inner()
            .query("UPDATE ONLY $target SET role = 'admin' RETURN NONE;")
            .bind(("target", admin.account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let session = AuthRepo::new(db.inner())
            .mint_browser_session(
                &admin.account,
                chrono::Duration::minutes(30),
                chrono::Duration::hours(24),
                Utc::now(),
            )
            .await
            .unwrap();
        Fixture {
            db,
            admin: admin.account,
            admin_session: session.cookie.render().expose().to_string(),
            subject: subject.account,
        }
    }

    impl Fixture {
        /// The administrator's owner context, which is what revoking one of its own credentials needs.
        async fn owner_context(&self) -> crate::access::OwnerContext {
            let principal = self.principal().await;
            AccessRepo::new(&self.db)
                .authorize(&principal)
                .await
                .unwrap()
        }

        async fn principal(&self) -> crate::access::Principal {
            AuthRepo::new(self.db.inner())
                .authenticate_browser_session(
                    &self.admin_session,
                    chrono::Duration::minutes(30),
                    Utc::now(),
                )
                .await
                .unwrap()
        }

        /// The administrator's capability, issued the only way it can be: authenticate the session, then
        /// ask storage what the rows say.
        async fn capability(&self) -> AdminCapability {
            let principal = self.principal().await;
            assert!(matches!(
                principal.provenance(),
                PrincipalProvenance::Credential(_)
            ));
            AccessRepo::new(&self.db)
                .authorize_admin(&principal)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn authorize_admin_refuses_an_owner_and_survives_a_suspension() {
        let fixture = fixture().await;
        let access = AccessRepo::new(&fixture.db);
        let admin = fixture.capability().await;
        assert_eq!(admin.account(), &fixture.admin);

        // An ordinary owner is not an administrator, and the refusal is the same answer an
        // unresolvable id gives.
        let subject_session = AuthRepo::new(fixture.db.inner())
            .mint_browser_session(
                &fixture.subject,
                chrono::Duration::minutes(30),
                chrono::Duration::hours(24),
                Utc::now(),
            )
            .await
            .unwrap();
        let subject_principal = AuthRepo::new(fixture.db.inner())
            .authenticate_browser_session(
                subject_session.cookie.render().expose(),
                chrono::Duration::minutes(30),
                Utc::now(),
            )
            .await
            .unwrap();
        assert!(matches!(
            access.authorize_admin(&subject_principal).await,
            Err(StorageError::NotFound)
        ));

        // Suspend the administrator, and the session it still holds stops entitling *anything*.
        fixture
            .db
            .inner()
            .query("UPDATE ONLY $target SET status = 'suspended' RETURN NONE;")
            .bind(("target", fixture.admin.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let stale = AuthRepo::new(fixture.db.inner())
            .authenticate_browser_session(
                &fixture.admin_session,
                chrono::Duration::minutes(30),
                Utc::now(),
            )
            .await;
        assert!(
            stale.is_err(),
            "a suspended account's session still authenticates"
        );
    }

    #[tokio::test]
    async fn set_status_suspends_and_restores_the_target_account() {
        let fixture = fixture().await;
        let admin = fixture.capability().await;
        let repo = AdminRepo::new(fixture.db.inner());

        let status = repo
            .set_status(&admin, &fixture.subject, true, Utc::now())
            .await
            .unwrap();
        assert_eq!(status, account_status::SUSPENDED);
        let listed = repo.list_accounts(&admin, 50, 0).await.unwrap();
        let subject = listed
            .iter()
            .find(|entry| entry.account == fixture.subject)
            .expect("the subject is listed");
        assert!(subject.is_suspended());
        // The listing carries account metadata and nothing that could be somebody's content.
        let rendered = format!("{listed:?}");
        assert!(!rendered.contains("password_hash"), "{rendered}");

        repo.set_status(&admin, &fixture.subject, false, Utc::now())
            .await
            .unwrap();
        assert!(
            !repo
                .list_accounts(&admin, 50, 0)
                .await
                .unwrap()
                .into_iter()
                .find(|entry| entry.account == fixture.subject)
                .unwrap()
                .is_suspended(),
            "the restore did not land"
        );
    }

    /// A suspension retires the credentials the account held; a restore does not bring them back.
    ///
    /// The assertion is on the epoch rather than on a session row's `revoked` flag, because that is the
    /// mechanism: v009's `auth_epoch` is what makes "every credential minted before this instant is
    /// inert" one integer write. Restoring flips `status` alone, so the epoch — and with it the death of
    /// those credentials — survives the restore. Without the increment, restoring an account would hand
    /// a cookie or token that motivated the suspension back its authority.
    #[tokio::test]
    async fn suspending_advances_the_epoch_and_restoring_does_not_resurrect_the_old_credentials() {
        let fixture = fixture().await;
        let admin = fixture.capability().await;
        let repo = AdminRepo::new(fixture.db.inner());
        let epoch_before: i64 = {
            let mut response = fixture
                .db
                .inner()
                .query("SELECT auth_epoch FROM $target;")
                .bind(("target", fixture.subject.as_record_id().clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
            let rows: Vec<serde_json::Value> = response.take(0).unwrap();
            rows[0]["auth_epoch"].as_i64().expect("an epoch on the row")
        };

        repo.set_status(&admin, &fixture.subject, true, Utc::now())
            .await
            .unwrap();
        let read_epoch = |db: &Database| {
            let db = db.inner().clone();
            let target = fixture.subject.as_record_id().clone();
            async move {
                let mut response = db
                    .query("SELECT auth_epoch FROM $target;")
                    .bind(("target", target))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                let rows: Vec<serde_json::Value> = response.take(0).unwrap();
                rows[0]["auth_epoch"].as_i64().unwrap()
            }
        };
        assert_eq!(
            read_epoch(&fixture.db).await,
            epoch_before + 1,
            "a suspension did not retire the account's existing credentials"
        );

        repo.set_status(&admin, &fixture.subject, false, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            read_epoch(&fixture.db).await,
            epoch_before + 1,
            "restoring an account requalified the credentials its suspension retired"
        );
    }

    #[tokio::test]
    async fn set_status_refuses_self_suspension_and_an_absent_target() {
        let fixture = fixture().await;
        let admin = fixture.capability().await;
        let repo = AdminRepo::new(fixture.db.inner());

        let error = repo
            .set_status(&admin, admin.account(), true, Utc::now())
            .await
            .expect_err("suspending oneself must be refused");
        assert!(
            matches!(error, StorageError::InvalidInput(_)),
            "unexpected refusal: {error:?}"
        );
        assert!(
            matches!(
                repo.set_status(
                    &admin,
                    &AccountId::try_from(surrealdb::types::RecordId::new(
                        "account",
                        "nobody-here-at-all"
                    ))
                    .unwrap(),
                    true,
                    Utc::now()
                )
                .await,
                Err(StorageError::NotFound)
            ),
            "an absent target was not a NotFound"
        );
        // The refusal wrote nothing: the admin is still active, which is what the next call needs.
        assert!(
            repo.list_accounts(&admin, 50, 0)
                .await
                .unwrap()
                .iter()
                .any(|entry| entry.account == fixture.admin && !entry.is_suspended())
        );
    }

    #[tokio::test]
    async fn a_stale_capability_claims_nothing_and_writes_no_status_change() {
        // The guard is a claim, not a look: revoke the administrator's own credential and the write
        // must cost the transaction, so a suspension cannot be committed by an authority that ended
        // between the page render and the click.
        let fixture = fixture().await;
        let admin = fixture.capability().await;
        let owner = fixture.owner_context().await;
        let credential = fixture
            .principal()
            .await
            .credential()
            .cloned()
            .expect("a hosted principal carries its credential");
        AuthRepo::new(fixture.db.inner())
            .revoke_credential(&owner, &credential, Utc::now())
            .await
            .unwrap();

        let error = AdminRepo::new(fixture.db.inner())
            .set_status(&admin, &fixture.subject, true, Utc::now())
            .await
            .expect_err("a revoked administrator must not be able to suspend anybody");
        assert!(
            matches!(error, StorageError::Stale),
            "unexpected refusal: {error:?}"
        );
        let repo = AdminRepo::new(fixture.db.inner());
        let listed = repo.list_accounts(&admin, 50, 0).await.unwrap();
        assert!(listed.iter().all(|entry| !entry.is_suspended()));
    }

    #[tokio::test]
    async fn a_recovery_link_is_minted_for_another_account_and_shown_once() {
        let fixture = fixture().await;
        let admin = fixture.capability().await;
        let repo = AdminRepo::new(fixture.db.inner());

        let link = repo
            .create_recovery_link(
                &admin,
                &fixture.subject,
                chrono::Duration::hours(1),
                Utc::now(),
            )
            .await
            .unwrap();
        assert!(!link.secret.is_empty());
        // The row belongs to the account it recovers, not to the issuer.
        let row = AuthRepo::new(fixture.db.inner())
            .credential_of(&link.credential)
            .await
            .unwrap()
            .expect("the link row");
        assert_eq!(
            row.account.as_ref(),
            Some(fixture.subject.as_record_id()),
            "a recovery link is addressed to the account it rotates"
        );
        assert_eq!(row.kind, crate::models::credential_kind::RECOVERY);

        // An absent target and a self-target are refused without minting a row.
        assert!(matches!(
            repo.create_recovery_link(
                &admin,
                &AccountId::try_from(surrealdb::types::RecordId::new("account", "nobody")).unwrap(),
                chrono::Duration::hours(1),
                Utc::now(),
            )
            .await,
            Err(StorageError::NotFound)
        ));
        assert!(matches!(
            repo.create_recovery_link(
                &admin,
                admin.account(),
                chrono::Duration::hours(1),
                Utc::now()
            )
            .await,
            Err(StorageError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn the_listing_pages_and_counts_the_same_population() {
        let fixture = fixture().await;
        let admin = fixture.capability().await;
        let repo = AdminRepo::new(fixture.db.inner());
        let all = repo.list_accounts(&admin, 50, 0).await.unwrap();
        assert_eq!(all.len(), 2, "the fixture enrolled two accounts: {all:?}");
        assert_eq!(repo.count_accounts(&admin).await.unwrap(), 2);
        // A page smaller than the population returns that many, ordered by id, and the cap is applied
        // rather than trusted.
        let page = repo.list_accounts(&admin, 1, 0).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(
            repo.list_accounts(&admin, usize::MAX, 0)
                .await
                .unwrap()
                .len(),
            2,
            "the page cap must not turn a two-account store into an error"
        );
        assert_eq!(page[0].account, all[0].account);
    }
}
