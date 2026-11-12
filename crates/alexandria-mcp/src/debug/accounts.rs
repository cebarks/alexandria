//! The account-facing browser pages: sign in, sign out, enrol from a link, recover a password, manage
//! one's own credentials, and — for an administrator — manage accounts.
//!
//! Every handler here is one of three kinds. The three auth pages are **public by design**: reachable
//! with no session, and their `POST`s carry a pre-auth token this process issued. Everything under
//! `/debug/account` is **owner-facing**: it reads the [`BrowserSession`] the guard inserted and acts
//! only on the caller's own account. The `/debug/admin` routes and the admin health page are the third
//! kind, and the rule they exist to enforce is in what they read rather than in where they are mounted:
//! an administrator sees **account rows and content-free scheduler health**, never another account's
//! corpus. That is why [`admin_accounts`] reads through `AdminRepo` and no scoped corpus reader, why the
//! maintenance rollup projects counts and verbs instead of record ids, and why the guard refuses a plain
//! owner before this module's admin code runs — with each admin handler re-checking the capability
//! itself, so a route that somehow escaped `admin_route` still has nothing to act with.
//!
//! ## Secrets are shown once, by construction
//!
//! An agent token, an invitation link and a recovery link each arrive as a value whose plaintext leaves
//! this process exactly once: here, in the response that minted it. Nothing reads a secret back —
//! storage keeps digests only — so the account page lists *labels and ids*, and the "copy this now"
//! sentence on a minting response is the UI matching a property the database already enforces rather
//! than a courtesy.
//!
//! ## Password work is not re-implemented here
//!
//! Login, invitation-claim and recovery-claim call the **same** `AuthMiddleware` order `/mcp` uses —
//! attempt budget, then login shape, then the Argon2 slot, then the path that reads a row — because
//! that ordering is what stops a held, unspent invitation link from buying one ~19 MiB digest per
//! attempt for the rest of its life. A route that called `AuthService::login` directly would be
//! cheaper to write and would drop the budget.

use askama::Template;
use axum::Form;
use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;

use super::html::{self, format_dt, page};
use super::web::{AuthPage, Boundary, BrowserSession, PeerAddress};
use crate::error::ServiceError;
use alexandria_storage::models::identity::{AccountId, CredentialId};
use alexandria_storage::models::{Credential, credential_kind};
use alexandria_storage::record_id_to_string;
use surrealdb::types::RecordId;

// -- templates ------------------------------------------------------------------------------------------

/// A public auth page. None of these extends `layout.html`: that layout's nav links into the private
/// surface, and a login page must not offer a signed-out visitor a route that answers 403. Each
/// therefore carries its own short inline `<style>` (the same idiom `graph.html` uses for page-local
/// rules), and no new vendored asset is introduced.
#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate<'a> {
    /// Which page this is, posted back so a token issued for one page cannot drive another.
    page: &'a str,
    csrf: String,
    /// Set only after a refusal, and never with anything about *which* account failed: this endpoint is
    /// reachable without a session and must not be an account-enumeration oracle.
    error: Option<String>,
    /// Whether the deployment runs without `Secure` cookies. Worth saying out loud: it is the one state
    /// where plain HTTP is allowed at all, and it should not look identical to a hosted login.
    insecure_development: bool,
}

#[derive(Template)]
#[template(path = "invite.html")]
struct InviteTemplate {
    page: &'static str,
    csrf: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "recover.html")]
struct RecoverTemplate {
    page: &'static str,
    csrf: String,
    error: Option<String>,
    /// Set once after a link was minted for *this* account by the owner-facing route, so the page can
    /// show the value it will never show again.
    issued: Option<String>,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountTemplate {
    nav: &'static str,
    login: String,
    /// The caller's own browser sessions and agent tokens, by label. Never a secret, never a digest.
    credentials: Vec<CredentialRow>,
    /// A freshly minted value, on the one response that produced it.
    issued: Option<IssuedRow>,
    error: Option<String>,
    csrf: String,
    /// Whether this account may also see the administration link. A capability, not a guess from the
    /// login name.
    is_admin: bool,
}

/// One row of the caller's credential list.
struct CredentialRow {
    id: String,
    kind: &'static str,
    label: String,
    created: String,
    last_used: String,
    expires: String,
    state: &'static str,
    /// Whether this row is the session the request is arriving on, so the page can say "this device"
    /// rather than listing an anonymous-looking second row for the same browser.
    current: bool,
}

struct IssuedRow {
    kind: &'static str,
    label: String,
    secret: String,
}

#[derive(Template)]
#[template(path = "admin_accounts.html")]
struct AdminTemplate {
    nav: &'static str,
    accounts: Vec<AdminAccountRow>,
    total: usize,
    /// Non-content scheduler health: what a run did, never what it touched.
    runs: Vec<AdminRunRow>,
    /// A link minted for one account, shown once.
    issued: Option<String>,
    error: Option<String>,
    csrf: String,
    acting_login: String,
}

struct AdminAccountRow {
    id: String,
    login: String,
    status: String,
    role: String,
    created: String,
    is_self: bool,
}

struct AdminRunRow {
    run: String,
    jobs: String,
    actions: String,
    entries: usize,
    members_moved: i64,
    first_at: String,
    last_at: String,
}

// -- public auth pages ----------------------------------------------------------------------------------

/// The `GET` half of an auth page: render it with a fresh pre-auth token.
async fn auth_page_form(boundary: Boundary, page: AuthPage) -> Response {
    // One token mint per page: the three forms share the mechanism and differ only in their fields, so
    // the value is issued once and moved into whichever template this page is.
    let csrf = boundary.issue_preauth(page);
    let insecure = !boundary.policy().secure_cookies();
    match page {
        AuthPage::Login => page_render(LoginTemplate {
            page: page.label(),
            csrf,
            error: None,
            insecure_development: insecure,
        }),
        AuthPage::Invite => page_render(InviteTemplate {
            page: page.label(),
            csrf,
            error: None,
        }),
        AuthPage::Recover => page_render(RecoverTemplate {
            page: page.label(),
            csrf,
            error: None,
            issued: None,
        }),
    }
}

/// Renders through the shared `html::page`, under a local name because every handler in this module
/// also has an `AuthPage` in scope.
fn page_render<T: askama::Template>(template: T) -> Response {
    page(template)
}

pub(crate) async fn login_form(boundary: Boundary) -> Response {
    auth_page_form(boundary, AuthPage::Login).await
}

pub(crate) async fn invite_form(boundary: Boundary) -> Response {
    auth_page_form(boundary, AuthPage::Invite).await
}

pub(crate) async fn recover_form(boundary: Boundary) -> Response {
    auth_page_form(boundary, AuthPage::Recover).await
}

/// The forms below deliberately do **not** model the `csrf` field their pages post. The guard reads that
/// key out of the body before any handler runs and refuses the request there, so a handler that also
/// carried the field would either re-check it (a second CSRF authority, which is the drift the design
/// refuses) or hold a value it never reads. `serde` ignores an unmodelled key rather than failing on it,
/// which is what lets the same body satisfy both the guard and the handler.
#[derive(Debug, Deserialize)]
pub(crate) struct LoginForm {
    pub login: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InviteForm {
    pub secret: String,
    pub login: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecoverForm {
    pub secret: String,
    pub password: String,
}

/// Signs a browser in.
///
/// The gate order is Task 5's, and `source` is the peer address the guard recorded — never a forwarded
/// header, because a limiter keyed on a value the client chooses is a limiter the client sets. A
/// success mints a **new** session (rotation is measurable: the value differs from anything previously
/// issued) and redirects; a failure re-renders the page with one generic sentence and the two refusals
/// an unauthenticated caller can distinguish nothing between.
pub(crate) async fn login(
    boundary: Boundary,
    peer: PeerAddress,
    Form(form): Form<LoginForm>,
) -> Response {
    let source = peer.as_str().to_string();
    match boundary
        .gate()
        .login(&source, &form.login, &form.password)
        .await
    {
        Ok(grant) => {
            let mut response = (
                StatusCode::SEE_OTHER,
                [(axum::http::header::LOCATION, "/debug")],
            )
                .into_response();
            super::web::push_cookies(&mut response, boundary.cookies_for(&grant));
            response
        }
        Err(error) => {
            let status = match error {
                // A budget was reached: the request was not malformed and nothing about the account is
                // being asserted. 429, because the caller's next action is to wait.
                ServiceError::Quota => StatusCode::TOO_MANY_REQUESTS,
                // Everything else — unknown login, wrong password, an unusable login shape, a store
                // that cannot answer — is one 401 with one sentence that does not say which.
                _ => StatusCode::UNAUTHORIZED,
            };
            (
                status,
                page_render(LoginTemplate {
                    page: AuthPage::Login.label(),
                    csrf: boundary.issue_preauth(AuthPage::Login),
                    error: Some(login_refusal(&error)),
                    insecure_development: !boundary.policy().secure_cookies(),
                }),
            )
                .into_response()
        }
    }
}

/// The sentence a failed login shows. One per error class, none of them about which account.
fn login_refusal(error: &ServiceError) -> String {
    match error {
        ServiceError::Quota => {
            "too many attempts from this address; try again in a minute".to_string()
        }
        ServiceError::InvalidInput(_) => {
            "that login or password is not usable (a login is 3-64 characters)".to_string()
        }
        _ => "no account matches that login and password".to_string(),
    }
}

/// Claims an invitation and enrols a new owner.
///
/// The new owner still has to log in: an invitation creates an account, it does not authenticates as
/// one — which is why this redirects to the login page and sets no cookie. The password is hashed
/// before the link is claimed (Task 4's order), so a request that cannot produce a policy digest never
/// burns a one-use link.
pub(crate) async fn invite(
    boundary: Boundary,
    peer: PeerAddress,
    Form(form): Form<InviteForm>,
) -> Response {
    let source = peer.as_str().to_string();
    match boundary
        .gate()
        .enroll(&source, &form.secret, &form.login, &form.password)
        .await
    {
        Ok(_) => redirect(StatusCode::SEE_OTHER, "/debug/login"),
        Err(error) => (
            StatusCode::UNAUTHORIZED,
            page(InviteTemplate {
                page: AuthPage::Invite.label(),
                csrf: boundary.issue_preauth(AuthPage::Invite),
                error: Some(claim_refusal(&error)),
            }),
        )
            .into_response(),
    }
}

/// Consumes a recovery link and rotates the password.
///
/// Nothing is handed back — no session, no token — because a recovery that also authenticated would
/// let whoever consumed the link keep using the account without ever proving the new password. Every
/// credential the account held is inert the moment its `auth_epoch` rises, and the *live* ones are
/// closed here rather than left to their next request: this is the caller that knows whose link it
/// was, which is precisely the loop Task 5's `recover` documentation left for this surface to close.
pub(crate) async fn recover(
    boundary: Boundary,
    peer: PeerAddress,
    Form(form): Form<RecoverForm>,
) -> Response {
    let source = peer.as_str().to_string();
    // Resolved *before* the claim, because `AuthRepo::recover` returns no account to name afterwards and
    // an epoch bump cannot reach a stream that is already open. A link that does not resolve here is
    // refused below by the same words as any other failure.
    let target = boundary.recovery_target(&form.secret).await;
    let outcome = boundary
        .gate()
        .recover(&source, &form.secret, &form.password)
        .await;
    if outcome.is_ok() {
        if let Some(account) = target.as_ref() {
            boundary.invalidate_account(account).await;
        }
        return redirect(StatusCode::SEE_OTHER, "/debug/login");
    }
    // The branch above returned on success, so this is the only remaining shape — matched rather than
    // `expect`ed, because `Result::err` on the happy path is the kind of accident that silently turns a
    // 200 into a panic.
    let error = match outcome {
        Ok(_) => unreachable!("the success arm redirected above"),
        Err(error) => error,
    };
    (
        StatusCode::UNAUTHORIZED,
        page_render(RecoverTemplate {
            page: AuthPage::Recover.label(),
            csrf: boundary.issue_preauth(AuthPage::Recover),
            error: Some(claim_refusal(&error)),
            issued: None,
        }),
    )
        .into_response()
}

/// Why a link could not be claimed. One sentence per class; the difference between an expired, a spent
/// and a never-existing link is exactly what this endpoint must not reveal.
fn claim_refusal(error: &ServiceError) -> String {
    match error {
        ServiceError::Stale => "that link has expired, been used, or been revoked".to_string(),
        ServiceError::Quota => "too many attempts; try again in a minute".to_string(),
        ServiceError::InvalidInput(_) => "that link, login or password is not usable".to_string(),
        _ => "the request could not be completed".to_string(),
    }
}

// -- owner-facing pages ---------------------------------------------------------------------------------

/// The caller's own account page.
pub(crate) async fn account(boundary: Boundary, session: BrowserSession) -> Response {
    render_account(&boundary, &session, None, None).await
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenForm {
    pub name: String,
}

/// Issues a named agent token for the caller's own account and shows it once.
///
/// The response says what the store guarantees: there is no second reading. Losing it means issuing a
/// new token and revoking this one, which is what the list below is for.
pub(crate) async fn issue_token(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<TokenForm>,
) -> Response {
    match boundary
        .service()
        .create_agent_token(session.owner(), &form.name, Utc::now())
        .await
    {
        Ok(secret) => {
            render_account(
                &boundary,
                &session,
                Some(IssuedRow {
                    kind: "agent token",
                    label: form.name.trim().to_string(),
                    secret: secret.expose().to_string(),
                }),
                None,
            )
            .await
        }
        Err(error) => render_account(&boundary, &session, None, Some(error.to_string())).await,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RevokeForm {
    pub credential: String,
}

/// Revokes one of the caller's own credentials.
///
/// A foreign id and an absent id are the same 404, which is what `AuthRepo::revoke_credential`'s
/// ownership rule already guarantees. Revoking a *session* row ends another device; revoking the row
/// behind this request's own cookie is what logout does, and both publish on the Task 5 bus, so a
/// stream minted from the same credential dies with it.
pub(crate) async fn revoke_credential(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<RevokeForm>,
) -> Response {
    let Some(credential) =
        parse_record(&form.credential, "credential").and_then(|id| CredentialId::try_from(id).ok())
    else {
        // Not "no such credential" but "not one of yours": the same answer for both, so an id cannot be
        // probed for ownership.
        return not_found("no credential of yours has that id");
    };
    match boundary
        .revoke_own_credential(session.owner(), &credential)
        .await
    {
        Ok(()) => render_account(&boundary, &session, None, None).await,
        // Revoking one's own already-revoked row is idempotent (Task 4), so `NotFound` here means the
        // row was never this caller's.
        Err(ServiceError::NotFound) => not_found("no credential of yours has that id"),
        Err(error) => render_account(&boundary, &session, None, Some(error.to_string())).await,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CsrfOnly {}

/// Mints a recovery link for the caller's **own** account.
///
/// The owner-facing half of recovery, and the one that keeps the admin page honest: an administrator
/// mints a link for somebody else through `/debug/admin/recovery`, and the row that produces belongs to
/// the account it rotates.
pub(crate) async fn create_recovery(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<CsrfOnly>,
) -> Response {
    let _ = &form;
    mint_link(&boundary, &session, Mint::Recovery).await
}

enum Mint {
    Invitation,
    Recovery,
}

async fn mint_link(boundary: &Boundary, session: &BrowserSession, kind: Mint) -> Response {
    let (label, minted) = match kind {
        Mint::Invitation => (
            "invitation",
            boundary
                .service()
                .create_invite(session.owner(), Utc::now())
                .await,
        ),
        Mint::Recovery => (
            "recovery",
            boundary
                .service()
                .create_recovery_link(session.owner(), Utc::now())
                .await,
        ),
    };
    match minted {
        Ok(secret) => {
            render_account(
                boundary,
                session,
                Some(IssuedRow {
                    kind: label,
                    label: label.to_string(),
                    secret: secret.expose().to_string(),
                }),
                None,
            )
            .await
        }
        Err(error) => render_account(boundary, session, None, Some(error.to_string())).await,
    }
}

/// Ends this browser session: revokes the credential behind the cookie, then clears both halves.
pub(crate) async fn logout(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<CsrfOnly>,
) -> Response {
    let _ = &form;
    match boundary.logout(&session).await {
        Ok(cookies) => {
            let mut response = redirect(StatusCode::SEE_OTHER, "/debug/login");
            super::web::push_cookies(&mut response, cookies);
            response
        }
        // The cookie is still cleared: a session whose row could not be revoked must not stay usable in
        // the browser, and the refusal is logged rather than rendered as a success.
        Err(error) => {
            tracing::warn!("logout could not revoke the session credential: {error}");
            let mut response = redirect(StatusCode::SEE_OTHER, "/debug/login");
            super::web::push_cookies(&mut response, boundary.cleared_cookies());
            response
        }
    }
}

// -- admin-facing pages ---------------------------------------------------------------------------------

/// The account list, plus content-free scheduler health.
///
/// Reaches `AdminRepo` and the maintenance rollup, and nothing corpus-shaped: an administrator sees
/// logins, states, roles and per-run totals. No scoped reader is constructed here, so there is no path
/// from this page to another account's rows, labels or counts.
pub(crate) async fn admin_accounts(boundary: Boundary, session: BrowserSession) -> Response {
    let Some(capability) = session.admin() else {
        return super::web::plain_forbidden(ADMIN_ONLY);
    };
    let repo = boundary.admin_repo();
    let limit = MAX_PAGE;
    let offset = 0;
    let accounts = match repo.list_accounts(capability, limit, offset).await {
        Ok(rows) => rows,
        Err(error) => return store_failure(&error.to_string()),
    };
    let total = repo
        .count_accounts(capability)
        .await
        .unwrap_or(accounts.len());
    let runs = match boundary.run_rollup(capability, limit, offset).await {
        Ok(rows) => rows,
        // Health is supplementary: a store that cannot answer it still shows the accounts, and the
        // reason stays in the log rather than in the page.
        Err(error) => {
            tracing::debug!("maintenance rollup unavailable: {error}");
            Vec::new()
        }
    };
    let acting_login = boundary
        .login_of(session.account())
        .await
        .unwrap_or_else(|_| "this account".to_string());

    render_admin(
        &boundary,
        &session,
        &acting_login,
        accounts,
        total,
        runs,
        None,
        None,
    )
}

const ADMIN_ONLY: &str = "403 Forbidden: this page manages accounts, which needs the administrator \
    role (see debug/guard.rs)";

#[derive(Debug, Deserialize)]
pub(crate) struct AdminAccountForm {
    pub account: String,
}

/// Suspends an account, and closes the sessions it held.
///
/// Refuses to suspend the acting administrator's own account, because the store would be left with no
/// way back in but a database edit — and Task 14's bootstrap CLI, which is the only sanctioned way to
/// create the first administrator, is not on this server yet.
pub(crate) async fn admin_suspend(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<AdminAccountForm>,
) -> Response {
    admin_set_status(&boundary, &session, &form.account, true).await
}

/// Restores a suspended account. The sessions it held stay dead: a restore is not a re-authentication.
pub(crate) async fn admin_activate(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<AdminAccountForm>,
) -> Response {
    admin_set_status(&boundary, &session, &form.account, false).await
}

async fn admin_set_status(
    boundary: &Boundary,
    session: &BrowserSession,
    raw: &str,
    suspend: bool,
) -> Response {
    let Some(capability) = session.admin() else {
        return super::web::plain_forbidden(ADMIN_ONLY);
    };
    let Some(target) = parse_record(raw, "account").and_then(|id| AccountId::try_from(id).ok())
    else {
        // Foreign and absent are one answer: this is not a probe for which accounts the store holds.
        return not_found("no account by that id");
    };
    match boundary
        .admin_repo()
        .set_status(capability, &target, suspend, Utc::now())
        .await
    {
        Ok(_) => {
            // The store-side half is already done — `authorize` re-reads `status` on every request —
            // but a lease or stream this process handed out before the change is only stopped by
            // saying so on the Task 5 bus, which is the loop `AuthService::invalidate_account`
            // documented and left for the surface that knows the account.
            boundary.invalidate_account(&target).await;
            redirect(StatusCode::SEE_OTHER, "/debug/admin")
        }
        Err(alexandria_storage::error::StorageError::NotFound) => {
            not_found("no account by that id")
        }
        Err(alexandria_storage::error::StorageError::Stale) => super::web::plain_forbidden(
            "403 Forbidden: this administrator's authority ended before the change was written (see \
             debug/guard.rs)",
        ),
        Err(alexandria_storage::error::StorageError::InvalidInput(message)) => {
            admin_refusal(boundary, session, &message).await
        }
        Err(error) => {
            admin_refusal(
                boundary,
                session,
                &format!("the change could not be written: {error}"),
            )
            .await
        }
    }
}

/// Mints a one-use invitation as the administrator, which the account it belongs to can then revoke.
///
/// Invitation minting is an administrator capability and this is the only route that performs it: an
/// owner may still mint a recovery link for their own account, but widening the account population is
/// account management, and the design reserves that to the role. The guard's `admin_route` list is the
/// authority that makes the route admin-only; the check below is the same one its siblings
/// ([`admin_accounts`], [`admin_recovery`] and the suspend/activate pair) carry, so a future mount that
/// escaped the list would still not mint anything.
///
/// The link belongs to the issuer — the invited account does not exist until it is claimed — so the mint
/// runs on the admin's own context, and an invitation is always issued by the account that owns the
/// resulting row.
pub(crate) async fn admin_invitation(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<CsrfOnly>,
) -> Response {
    let _ = &form;
    if session.admin().is_none() {
        return super::web::plain_forbidden(ADMIN_ONLY);
    }
    mint_link(&boundary, &session, Mint::Invitation).await
}

/// Mints a recovery link for another account.
pub(crate) async fn admin_recovery(
    boundary: Boundary,
    session: BrowserSession,
    Form(form): Form<AdminAccountForm>,
) -> Response {
    let Some(capability) = session.admin() else {
        return super::web::plain_forbidden(ADMIN_ONLY);
    };
    let Some(target) =
        parse_record(&form.account, "account").and_then(|id| AccountId::try_from(id).ok())
    else {
        return not_found("no account by that id");
    };
    match boundary
        .admin_repo()
        .create_recovery_link(capability, &target, crate::auth::RECOVERY_TTL, Utc::now())
        .await
    {
        Ok(link) => {
            let acting_login = boundary
                .login_of(session.account())
                .await
                .unwrap_or_else(|_| "this account".to_string());
            render_admin(
                &boundary,
                &session,
                &acting_login,
                Vec::new(),
                0,
                Vec::new(),
                Some(link.secret),
                None,
            )
        }
        Err(alexandria_storage::error::StorageError::NotFound) => {
            not_found("no account by that id")
        }
        Err(error) => admin_refusal(&boundary, &session, &error.to_string()).await,
    }
}

// -- rendering ------------------------------------------------------------------------------------------

/// The most rows either admin table will render.
///
/// A cap rather than a page: `maintenance_log` has no owner column to scope by, so the health table is a
/// store-wide aggregate by design (decision B) and the account listing is the same shape. Paging is not
/// offered until there is a reason to believe the first 200 rows are not the answer.
const MAX_PAGE: usize = 200;

/// Re-renders the account page.
///
/// `error` is `Option<String>` and arrives already-`Some` from the refusal paths; the nesting in the
/// template's field is the template's, so the parameter is the plain form.
async fn render_account(
    boundary: &Boundary,
    session: &BrowserSession,
    issued: Option<IssuedRow>,
    error: Option<String>,
) -> Response {
    let credentials = boundary
        .own_credentials(session.owner())
        .await
        .unwrap_or_default();
    let login = boundary
        .login_of(session.account())
        .await
        .unwrap_or_else(|_| "this account".to_string());
    let here = session
        .credential()
        .map(|credential| record_id_to_string(credential.as_record_id()));
    page(AccountTemplate {
        nav: "account",
        login,
        credentials: credentials
            .into_iter()
            .map(|row| credential_row(&row, here.as_deref()))
            .collect(),
        issued,
        error,
        csrf: session.csrf().to_string(),
        is_admin: session.admin().is_some(),
    })
}

async fn admin_refusal(boundary: &Boundary, session: &BrowserSession, message: &str) -> Response {
    let acting_login = boundary
        .login_of(session.account())
        .await
        .unwrap_or_else(|_| "this account".to_string());
    render_admin(
        boundary,
        session,
        &acting_login,
        Vec::new(),
        0,
        Vec::new(),
        None,
        Some(message.to_string()),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_admin(
    boundary: &Boundary,
    session: &BrowserSession,
    acting_login: &str,
    accounts: Vec<alexandria_storage::repos::AccountEntry>,
    total: usize,
    runs: Vec<alexandria_storage::repos::MaintenanceRunSummary>,
    issued: Option<String>,
    error: Option<String>,
) -> Response {
    let _ = boundary;
    page(AdminTemplate {
        nav: "admin",
        accounts: accounts
            .into_iter()
            .map(|entry| AdminAccountRow {
                is_self: entry.account == *session.account(),
                id: record_id_to_string(entry.account.as_record_id()),
                login: entry.login,
                status: entry.status.unwrap_or_else(absent),
                role: entry.role.unwrap_or_else(absent),
                created: format_dt(entry.created_at),
            })
            .collect(),
        total,
        runs: runs
            .into_iter()
            .map(|run| AdminRunRow {
                run: run.run_id.unwrap_or_else(absent),
                jobs: join_or_absent(&run.jobs),
                actions: join_or_absent(&run.actions),
                entries: run.entries,
                members_moved: run.members_moved,
                first_at: format_dt(run.first_at),
                last_at: format_dt(run.last_at),
            })
            .collect(),
        issued,
        error,
        csrf: session.csrf().to_string(),
        acting_login: acting_login.to_string(),
    })
}

fn credential_row(row: &Credential, here: Option<&str>) -> CredentialRow {
    let id = row
        .id
        .as_ref()
        .map(record_id_to_string)
        .unwrap_or_else(absent);
    CredentialRow {
        kind: match row.kind.as_str() {
            credential_kind::BROWSER => "browser session",
            _ => "agent token",
        },
        current: here == Some(id.as_str()),
        label: row.label.clone().unwrap_or_else(|| "unnamed".to_string()),
        created: format_dt(row.created_at),
        last_used: format_dt(row.last_used_at),
        // An agent token has no deadline at all, and "no deadline" is a different fact from "no
        // value": rendering the absent case as `—` would read as "expired" to the eye that made the
        // column.
        expires: match (&row.expires_at, &row.absolute_expires_at) {
            (None, None) if row.kind == credential_kind::AGENT => "never".to_string(),
            _ => format_dt(row.absolute_expires_at.or(row.expires_at)),
        },
        state: if row.revoked.unwrap_or(false) {
            "revoked"
        } else if row.consumed.unwrap_or(false) {
            "used"
        } else {
            "active"
        },
        id,
    }
}

fn join_or_absent(values: &[String]) -> String {
    if values.is_empty() {
        absent()
    } else {
        values.join(", ")
    }
}

/// The absent-value marker every debug page shares, from `html::ABSENT`.
fn absent() -> String {
    html::ABSENT.to_string()
}

/// A typed record id from a caller-supplied string, and only for the named table.
///
/// The parse is strict and the table is checked, so `fact:1` presented to an account field is a
/// refusal rather than a row nobody intended to name. Never `record_id_to_string`'s inverse: that
/// rendering is not a parser-safe round trip, which is why the store's own `RecordId::parse_simple` is
/// what decides.
fn parse_record(raw: &str, table: &str) -> Option<RecordId> {
    let id = RecordId::parse_simple(raw.trim()).ok()?;
    (id.table.as_str() == table).then_some(id)
}

fn redirect(status: StatusCode, location: &str) -> Response {
    (
        status,
        [(axum::http::header::LOCATION, location.to_string())],
    )
        .into_response()
}

/// A 404 that names no row. Foreign and absent are one answer everywhere in this module.
fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, message.to_string()).into_response()
}

/// A store that could not answer. 503 rather than 200-with-text: the account page's whole job is to
/// say what the account holds, and a wrong list is worse than no list.
fn store_failure(message: &str) -> Response {
    tracing::debug!("account page storage failure: {message}");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "this page could not be read right now".to_string(),
    )
        .into_response()
}
