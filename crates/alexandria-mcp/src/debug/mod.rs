pub mod accounts;
pub mod assets;
pub mod clusters;
pub mod collections;
pub mod dashboard;
pub mod graph;
mod guard;
pub mod html;
pub mod maintenance;
pub mod memories;
pub mod operations;
pub mod query;
pub mod refinement;
pub mod reminder_mapping;
pub mod sessions;
pub mod web;

#[cfg(test)]
mod browser_fixture;
#[cfg(test)]
mod test_support;

use axum::Extension;
use axum::Router;
use axum::routing::{get, post};

use crate::AlexandriaServer;
pub use web::{BrowserSession, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, WebBoundary, WebPolicy};

/// Read-only snapshot of resolved configuration, for the debug dashboard only.
///
/// Deliberately *not* fields on [`AlexandriaServer`]: that struct is the MCP tool surface and
/// carries only what the algorithms need (`retrieve_min_similarity`, `activation_top_n`,
/// `cohesion_floor`, …). Everything here is display-only, and some of it — transport, bind
/// address — is meaningless inside a tool handler that is already running. Populated in
/// `main.rs` from the same `Config` that built the server, so it cannot disagree with what the
/// server was given.
///
/// **This is diagnostics, not security.** Task 9 keeps it that way on purpose: authentication,
/// CSRF and origin policy travel in [`WebBoundary`], injected separately, so a build that omits
/// the dashboard's config cannot be a build with a weaker guard. The pre-Task 9 shape — Host
/// checking derived from `ctx.is_some()` — was the opposite, and `guard.rs` documents why that
/// choice is not coming back.
#[derive(Debug, Clone)]
pub struct DebugContext {
    /// `[embedding] model` as *asked for*. The dashboard shows this next to
    /// `EmbeddingProvider::model_id()`, which reports what was actually loaded — with the
    /// model locked on first boot, the gap between the two is the whole story.
    pub embedding_model: String,
    pub embedding_device: String,
    pub transport: String,
    pub bind_host: String,
    pub bind_port: u16,
    pub data_dir: String,
    pub cluster_merge_threshold: f32,
    /// The whole `[dreaming]` cadence as one line, built by the binary that read the file.
    ///
    /// A summary string rather than five numeric fields: the dashboard renders one row per key and
    /// the five jobs are only meaningful together, so six `DebugContext` fields to display one
    /// sentence would be plumbing for its own sake. Built in the binary because `DreamingConfig`
    /// lives there and cannot reach this crate — the same reason `ClusterConfig` values arrive
    /// pre-flattened.
    pub dreaming_summary: String,
    /// The scheduler's heartbeat, shared with the loop that writes it. `None` when
    /// `[dreaming] enabled = false`, which the dashboard renders as "not running" rather than as
    /// the no-context `UNAVAILABLE` — "turned off" and "this process cannot tell you" are different
    /// answers and collapsing them would be a lie.
    ///
    /// The type lives in `alexandria-engine` because neither the binary nor this crate can own it:
    /// `alexandria-mcp` cannot name a type defined in a binary crate, which is the same constraint
    /// that forces the cadences above to arrive pre-flattened.
    pub dreaming_liveness: Option<std::sync::Arc<alexandria_engine::dreaming::Liveness>>,
    /// `[server] allowed_hosts`, verbatim.
    ///
    /// Read only by the **no-boundary** build's Host check. Once a [`WebBoundary`] is mounted its
    /// own policy answers that question, and this field is not consulted: a
    /// [`WebPolicy::hosted`] cannot be built with a wildcard, so the authenticated build has no
    /// Host-check-off state to fall into.
    pub allowed_hosts: Vec<String>,
}

/// The public entry point, signature unchanged: tests, stdio mode and any caller that has no auth
/// service use this, and it now serves **no private data at all**.
///
/// This is not "the unauthenticated debug UI" any more. The router mounts no [`WebBoundary`], so
/// every private route — each read and every mutation, including the Query Tester — is refused by the
/// guard before its handler is entered, and only the closed asset allowlist answers. `/debug`'s
/// config panel and the corpus pages that used to be readable here are the reason Task 6's D1
/// deferral ended at this boundary rather than at a flag.
pub fn router(server: AlexandriaServer) -> Router {
    router_with_context(server, None)
}

/// The same fail-closed build with the dashboard's diagnostics attached.
///
/// HTTP mode no longer calls this — it calls [`router_secured`] — but the signature is kept and the
/// behaviour is now deliberately identical to [`router`]: diagnostics are a display input, so
/// supplying them must not widen what a caller may read, and omitting them must not narrow it.
pub fn router_with_context(server: AlexandriaServer, ctx: Option<DebugContext>) -> Router {
    let allowed_hosts = ctx
        .as_ref()
        .map(|ctx| ctx.allowed_hosts.clone())
        .unwrap_or_default();
    build(
        server,
        ctx,
        guard::Guard {
            boundary: None,
            legacy_hosts: allowed_hosts,
        },
    )
}

/// The build `main.rs` mounts: the same routes, behind an authenticated owner/admin boundary.
///
/// `ctx` still carries the dashboard's diagnostics and `boundary` the auth service, the store and the
/// origin policy. They are two arguments because they are two questions — "what may this caller see"
/// and "what does the operator's config say" — and the pre-Task 9 design collapsed them into one
/// `Option`, which is the bug the guard's module documentation records.
pub fn router_secured(
    server: AlexandriaServer,
    ctx: Option<DebugContext>,
    boundary: WebBoundary,
) -> Router {
    let legacy_hosts = ctx
        .as_ref()
        .map(|ctx| ctx.allowed_hosts.clone())
        .unwrap_or_default();
    build(
        server,
        ctx,
        guard::Guard {
            boundary: Some(std::sync::Arc::new(boundary)),
            legacy_hosts,
        },
    )
}

/// The route table, in one place, so the two builds cannot grow apart.
///
/// **Every debug route must be added here**, below the `guard` layer: the layer applies to the routes
/// registered above it, so a route bolted on after `.layer()` — or served by a different router that
/// merges this one — silently escapes every check in `guard.rs`. That is not a theoretical worry; the
/// layer has to cover the login form's `POST` as thoroughly as the Query Tester's, which is why
/// `every_route_the_router_mounts_is_in_the_matrix` reads this table back as text and asserts it against
/// both guard matrices, and why the private list in `tests/web_auth_test.rs` exists beside it. A new
/// route belongs in both. Write the mount on **one** line: that test finds mounts by scanning for a path
/// literal on the same line as the route call, so a route split across lines is invisible to it and the
/// matrix drift it exists to catch would go unreported (`/debug/login`, `/debug/invite`, `/debug/recover`
/// and `/debug/account/credentials/revoke` are all listed by hand today for that reason).
fn build(server: AlexandriaServer, ctx: Option<DebugContext>, state: guard::Guard) -> Router {
    Router::new()
        // Public by design: the closed asset allowlist, and the three auth pages.
        .route("/debug/assets/{name}", get(assets::asset))
        .route(
            "/debug/login",
            get(accounts::login_form).post(accounts::login),
        )
        .route(
            "/debug/invite",
            get(accounts::invite_form).post(accounts::invite),
        )
        .route(
            "/debug/recover",
            get(accounts::recover_form).post(accounts::recover),
        )
        // Private corpus routes. `Extension(ctx)` travels per route because the dashboard is the only
        // handler that reads it; a wider `State` would rewrite every extractor for one page's row.
        .route("/debug", get(dashboard::handler).layer(Extension(ctx)))
        .route("/debug/memories", get(memories::list))
        .route("/debug/memories/{id}", get(memories::detail))
        .route("/debug/clusters", get(clusters::list))
        .route("/debug/clusters/{id}", get(clusters::detail))
        .route("/debug/sessions", get(sessions::list))
        .route("/debug/sessions/{external_id}", get(sessions::detail))
        .route("/debug/graph/{id}", get(graph::page))
        .route("/debug/api/graph/{id}", get(graph::api_graph))
        .route("/debug/maintenance", get(maintenance::list))
        .route("/debug/reminder-mapping", get(reminder_mapping::list))
        .route(
            "/debug/reminder-mapping/release",
            post(reminder_mapping::release),
        )
        // Owner's private journal: the exact before/after of every change this account made. Both routes
        // resolve through `MutationRepo`, which authorises the operation row *and each snapshot it names*, so
        // a forged or foreign link answers as an absent one.
        .route("/debug/refinement", get(refinement::queue))
        .route("/debug/refinement/proposals/{id}", get(refinement::detail))
        .route("/debug/refinement/settings", get(refinement::settings))
        .route("/debug/refinement/consent", post(refinement::consent))
        .route("/debug/refinement/mode", post(refinement::mode))
        .route("/debug/refinement/enqueue", post(refinement::enqueue))
        .route(
            "/debug/refinement/proposals/{id}/approve",
            post(refinement::approve),
        )
        .route(
            "/debug/refinement/proposals/{id}/dismiss",
            post(refinement::dismiss),
        )
        .route(
            "/debug/refinement/proposals/{id}/defer",
            post(refinement::defer),
        )
        .route(
            "/debug/refinement/proposals/{id}/revert",
            post(refinement::revert),
        )
        .route("/debug/refinement/attempts/{id}", get(refinement::attempt))
        .route("/debug/operations", get(operations::list))
        .route("/debug/operations/{id}", get(operations::detail))
        // Task 11's owner surface. Every route here is one line on purpose: the route matrix finds mounts
        // by scanning for a path literal beside a route call, so a route split across lines is invisible to
        // the tripwire that exists to catch an ungated page.
        .route("/debug/collections", get(collections::list))
        .route("/debug/collections/edit", post(collections::edit))
        .route("/debug/collections/move", post(collections::move_one))
        .route("/debug/classification", get(collections::classification))
        .route("/debug/classification/decide", post(collections::decide))
        .route("/debug/classification/revise", post(collections::revise))
        .route("/debug/collections/archive", get(collections::archive))
        .route("/debug/query", get(query::form))
        .route("/debug/query/run", post(query::run))
        // Owner's own account: settings, credential list, token issue/revoke.
        .route("/debug/account", get(accounts::account))
        .route("/debug/account/tokens", post(accounts::issue_token))
        .route(
            "/debug/account/credentials/revoke",
            post(accounts::revoke_credential),
        )
        .route("/debug/account/recovery", post(accounts::create_recovery))
        // Admin: account management and non-content health. Nothing here reads a corpus.
        //
        // Invitation minting lives on this side and nowhere else. An owner may recover **their own**
        // account (`/debug/account/recovery`), but widening the account population is the
        // account-management authority the design reserves to the administrator role, so there is no
        // owner-facing mint to gate: the route below is the only one, and `admin_route` is what keeps it
        // that way.
        .route("/debug/admin", get(accounts::admin_accounts))
        .route("/debug/admin/invitations", post(accounts::admin_invitation))
        .route("/debug/admin/recovery", post(accounts::admin_recovery))
        .route("/debug/admin/suspend", post(accounts::admin_suspend))
        .route("/debug/admin/activate", post(accounts::admin_activate))
        .route("/debug/logout", post(accounts::logout))
        .with_state(server)
        // Over the whole router, after state is applied, so it covers `/debug/assets/*` and the
        // GET-as-a-cross-site-read case of the rebinding attack just as thoroughly as the
        // CSRF-carrying POST.
        .layer(axum::middleware::from_fn_with_state(state, guard::enforce))
}
