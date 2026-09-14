//! The Query Tester: a form page plus the htmx fragment that answers it.
//!
//! Both live in templates: `query.html` is a normal page, `query_results.html` is a body
//! fragment (it does not extend `layout.html`). Because `run` only ever answers with a fragment,
//! its error paths are `Fragment::Error` — `html::error_page` would wrap the message in a whole
//! layout and swap a second `<nav>` into `#query-results`, which is not what this endpoint has
//! ever returned.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::Response;
use serde::Deserialize;

use super::html::page;
use crate::AlexandriaServer;
use crate::tools::{RecallParams, RetrieveMemoriesParams};

/// The form. `nav` is the only field: no values are pre-filled, the inputs are static markup
/// whose `name` attributes are what [`QueryForm`] deserializes.
#[derive(Template)]
#[template(path = "query.html")]
struct QueryTemplate {
    nav: &'static str,
}

pub async fn form(State(_server): State<AlexandriaServer>) -> Response {
    page(QueryTemplate { nav: "query" })
}

#[derive(Debug, Deserialize)]
pub struct QueryForm {
    pub mode: String,
    pub query: String,
    /// Retrieve-only: `RecallParams` has no limit field.
    pub limit: Option<usize>,
    /// Retrieve-only, and the *external* session id — the same value `store_memory(session_id)`
    /// takes, because that is what `SessionRepo::get_memories` keys on. An `<input type="text">`
    /// left blank posts the key with an empty value, so this is `Some("")` rather than `None`
    /// when the operator does not fill it in; [`run`] normalizes that away.
    pub session_id: Option<String>,
    /// Retrieve-only. The template posts this as `<input type="checkbox" value="true">`, which is
    /// **absent from the body entirely** when unchecked rather than present-and-empty, so serde
    /// gives `Some("true")` or `None` — never `Some(false)`. [`run`] normalizes that pair down to
    /// a `bool` at the point of use, the same shape `memories::list` uses for its `include_deleted`
    /// checkbox. Deliberately not a field on `RetrieveMemoriesParams`, which is the public MCP tool
    /// schema.
    pub dry_run: Option<String>,
}

/// One row of the ID / Content / Similarity table. `retrieve_memories` results and focused
/// recall share this shape — the legacy code rendered both with the same `format!` line.
struct ResultRow {
    id: String,
    content: String,
    similarity: String,
}

/// One cluster of a broad recall. Only the representative memory *contents* are listed: the
/// legacy renderer never showed the per-memory ids or similarities inside the `<ul>`, so this
/// drops nothing.
struct RecallCluster {
    id: String,
    similarity: String,
    contents: Vec<String>,
}

/// What `/debug/query/run` can answer — one variant per return branch of the two `format!`
/// renderers this replaces. An enum rather than a struct of optional fields so the fragment
/// cannot render two branches at once, and so the template's `{% match %}` is checked for
/// exhaustiveness at compile time.
enum Fragment {
    /// A tool error, an unknown `mode` value, or a recall payload that failed to parse.
    Error { message: String },
    /// A bare-`<p>` empty state ("No results." and friends), which legacy had no class for.
    Notice { message: String },
    /// The results table. `heading` is `Some("Focused recall")` for recall's focused mode;
    /// retrieve results had no heading at all.
    Table {
        heading: Option<&'static str>,
        rows: Vec<ResultRow>,
    },
    /// Broad recall: one section per cluster. The `<h3>Broad recall</h3>` lives in the template.
    Clusters { sections: Vec<RecallCluster> },
}

impl Fragment {
    fn error(message: String) -> Self {
        Self::Error { message }
    }

    fn notice(message: &str) -> Self {
        Self::Notice {
            message: message.to_string(),
        }
    }

    /// A `retrieve_memories` result: `{ "results": [ { id, content, similarity }, … ] }`.
    fn retrieve(value: &serde_json::Value) -> Self {
        let rows = result_rows(value.get("results"));
        if rows.is_empty() {
            Self::notice("No results.")
        } else {
            Self::Table {
                heading: None,
                rows,
            }
        }
    }

    /// A `recall` result. The tool hands back a JSON *string*, so it can still fail to parse
    /// here after a successful call — that is this function's own `Error` branch.
    fn recall(json_str: &str) -> Self {
        let value: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => return Self::error(format!("Failed to parse recall response: {e}")),
        };

        // The form never sends a scope handle, so in practice only "broad" arrives here, but
        // both branches are kept: a missing or unexpected `mode` falls through to broad, which
        // is what legacy did (`unwrap_or("")`), and broad with no clusters prints
        // "No clusters found." rather than an empty table.
        if value.get("mode").and_then(|v| v.as_str()).unwrap_or("") == "focused" {
            let rows = result_rows(value.get("memories"));
            if rows.is_empty() {
                Self::notice("No memories in this scope.")
            } else {
                Self::Table {
                    heading: Some("Focused recall"),
                    rows,
                }
            }
        } else {
            let empty = Vec::new();
            let clusters = value
                .get("clusters")
                .and_then(|c| c.as_array())
                .unwrap_or(&empty);
            let sections: Vec<RecallCluster> = clusters
                .iter()
                .map(|c| RecallCluster {
                    id: string_field(c, "cluster_id"),
                    similarity: similarity_field(c),
                    contents: c
                        .get("representative_memories")
                        .and_then(|m| m.as_array())
                        .unwrap_or(&empty)
                        .iter()
                        .map(|m| string_field(m, "content"))
                        .collect(),
                })
                .collect();
            if sections.is_empty() {
                Self::notice("No clusters found.")
            } else {
                Self::Clusters { sections }
            }
        }
    }
}

/// Rows for a JSON array of `{ id, content, similarity }` entries. A missing or non-array value
/// yields no rows, which the callers turn into their empty state — as legacy did.
fn result_rows(value: Option<&serde_json::Value>) -> Vec<ResultRow> {
    let empty = Vec::new();
    let entries = value.and_then(|v| v.as_array()).unwrap_or(&empty);
    entries
        .iter()
        .map(|entry| ResultRow {
            id: string_field(entry, "id"),
            content: string_field(entry, "content"),
            similarity: similarity_field(entry),
        })
        .collect()
}

/// A string field, empty when absent or not a string — the legacy renderers used
/// `unwrap_or("")` rather than skipping a malformed entry, so a bad row shows blank cells.
fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A `similarity` as the legacy `{:.4}` printed it. Formatting stays in Rust: askama renders an
/// `f64` with plain `Display`, which would print `0.5` where the table printed `0.5000`.
fn similarity_field(value: &serde_json::Value) -> String {
    let similarity = value
        .get("similarity")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    format!("{similarity:.4}")
}

/// The fragment template. No `nav` field, because `query_results.html` is swapped into a div
/// rather than extending `layout.html`.
#[derive(Template)]
#[template(path = "query_results.html")]
struct QueryResultsTemplate {
    fragment: Fragment,
}

pub async fn run(State(server): State<AlexandriaServer>, Form(form): Form<QueryForm>) -> Response {
    let fragment = match form.mode.as_str() {
        "retrieve" => {
            // A checkbox sends `dry_run=true` when checked and nothing at all when not, so
            // "present and `true`" is the whole yes test — and anything else a hand-crafted body
            // could carry (`dry_run=1`) reads as not-dry, which is the safe direction to guess:
            // it leaves the run doing what the real tool does rather than quietly suppressing a
            // write the operator did not ask to suppress.
            let dry_run = form.dry_run.as_deref() == Some("true");
            // A blank text input still submits `session_id=`, which deserializes to `Some("")`
            // (and `Some("   ")` for spaces). Scoping to that would walk the edges of a session
            // whose external id is empty, find nothing, and report "No results." — a false
            // negative that looks like retrieval being broken rather than like an unfilled form
            // field, which is exactly the confusion this page exists to remove. So: blank means
            // unscoped.
            let session_id = form.session_id.filter(|id| !id.trim().is_empty());
            let params = RetrieveMemoriesParams {
                query: form.query.clone(),
                limit: form.limit,
                session_id,
            };
            // Spreading activation writes heat, so a retrieve run is the one debug route that
            // mutates. The template says so out loud; the checkbox routes around it for an
            // operator who wants an answer that a previous test query did not bias. The params
            // are built once above and moved into whichever path is taken, so the two cannot drift
            // apart in what they search for.
            let retrieval = if dry_run {
                server.do_retrieve_memories_dry(params).await
            } else {
                server.do_retrieve_memories(params).await
            };
            match retrieval {
                Ok(value) => Fragment::retrieve(&value),
                Err(e) => Fragment::error(e.to_string()),
            }
        }
        // recall mode intentionally ignores `limit` and `session_id`: RecallParams has no limit
        // field, and its `scope_handle` is an opaque handle returned by a previous broad recall
        // that narrows into one cluster — a different concept from a session id, so there is
        // nothing honest to map a session onto. It also ignores `dry_run`, for a reason worth
        // stating before anyone adds a branch: `do_recall` writes nothing at all (no activation,
        // no heat), so there is no side effect left for a dry run to skip. The form says
        // "(retrieve only)" for all three fields.
        "recall" => {
            let params = RecallParams {
                query: form.query.clone(),
                scope_handle: None,
            };
            match server.do_recall(params).await {
                Ok(json_str) => Fragment::recall(&json_str),
                Err(e) => Fragment::error(e.to_string()),
            }
        }
        other => Fragment::error(format!("Unknown mode: {other}")),
    };
    page(QueryResultsTemplate { fragment })
}

#[cfg(test)]
mod tests {
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_query_run_retrieve_mode_empty_db() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=retrieve&query=test&limit=5"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("No results"));
    }

    #[tokio::test]
    async fn test_query_run_retrieve_mode_finds_stored_fact() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("findable memory", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=retrieve&query=test&limit=5"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("findable memory"));
    }

    #[tokio::test]
    async fn test_query_run_recall_mode_without_limit() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=recall&query=test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("No clusters found") || text.contains("Broad recall"));
    }

    /// POST a urlencoded form body to `/debug/query/run` and return the fragment it swapped in.
    /// `Router` is consumed by `oneshot`, so callers pass a clone — same shape as the `fetch_body`
    /// helper in `sessions.rs`.
    async fn run_form(app: axum::Router, body: &'static str) -> String {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// Two sessions, each carrying exactly one memory of its own, so a scoped retrieval has
    /// something it must return and something it must not.
    async fn seed_two_sessions(server: &crate::AlexandriaServer) {
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        for (external_id, content) in [
            ("sess-alpha", "alpha project decision"),
            ("sess-beta", "beta project decision"),
        ] {
            // `create` hands back the internal record id (what `add_memory` wants); the form
            // posts the *external* id, which is what `store_memory(session_id)` uses.
            let sess = session_repo.create(external_id, None, None).await.unwrap();
            let fact = memory_repo
                .create_fact(content, 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
            session_repo.add_memory(&sess, &fact).await.unwrap();
        }
    }

    /// `</tr>` occurrences = 1 header row + N result rows, which pins the row count rather than
    /// just testing for a substring's presence/absence. Saturating so a fragment with no table at
    /// all (the `<p>No results.</p>` empty state) reports 0 and lets the assertion print the
    /// fragment, rather than panicking on a subtract overflow first.
    fn row_count(fragment: &str) -> usize {
        fragment.matches("</tr>").count().saturating_sub(1)
    }

    /// One result fragment per line, sorted, so two submissions are compared on *which* rows came
    /// back rather than the order they came back in. `do_retrieve_memories` ranks with a stable
    /// sort, so a tie — every embedding in these tests is the identical stub vector — preserves
    /// whatever order SurrealDB's `SELECT * FROM fact` scan returned. That is stable inside one
    /// process but is not a guarantee across a restart, and row sequence carries nothing worth
    /// asserting here anyway.
    fn sorted_lines(fragment: &str) -> String {
        let mut lines: Vec<&str> = fragment.lines().collect();
        lines.sort_unstable();
        lines.join("\n")
    }

    #[tokio::test]
    async fn test_query_tester_scopes_retrieve_to_session() {
        let server = super::super::test_support::test_server().await;
        seed_two_sessions(&server).await;
        let app = crate::debug::router(server);

        let scoped = run_form(
            app.clone(),
            "mode=retrieve&query=project+decision&limit=10&session_id=sess-alpha",
        )
        .await;
        assert!(
            scoped.contains("alpha project decision"),
            "the scoped session's own memory must be returned; got: {scoped}"
        );
        assert!(
            !scoped.contains("beta project decision"),
            "another session's memory must not leak into a session-scoped retrieval; got: {scoped}"
        );
        assert_eq!(
            row_count(&scoped),
            1,
            "scoped retrieval must return exactly one row; got: {scoped}"
        );

        // Unscoped, both must appear. Without this half the assertions above would also pass if
        // the query or the similarity floor were what dropped session B.
        let unscoped = run_form(app, "mode=retrieve&query=project+decision&limit=10").await;
        assert!(
            unscoped.contains("alpha project decision"),
            "the scoped session's memory must still be visible unscoped; got: {unscoped}"
        );
        assert!(
            unscoped.contains("beta project decision"),
            "an unscoped retrieval must see every session's memory; got: {unscoped}"
        );
        assert_eq!(
            row_count(&unscoped),
            2,
            "the same query without a session must return both rows; got: {unscoped}"
        );
    }

    #[tokio::test]
    async fn test_query_tester_empty_session_id_is_unscoped() {
        let server = super::super::test_support::test_server().await;
        seed_two_sessions(&server).await;
        let app = crate::debug::router(server);

        let omitted = run_form(app.clone(), "mode=retrieve&query=project+decision&limit=10").await;
        // A blank `<input type="text">` still posts the key, so the handler receives
        // `Some("")` (and `Some("  ")` for whitespace) rather than `None`.
        let empty = run_form(
            app.clone(),
            "mode=retrieve&query=project+decision&limit=10&session_id=",
        )
        .await;
        let whitespace = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&session_id=%20%20",
        )
        .await;

        for fragment in [&empty, &whitespace] {
            assert!(
                fragment.contains("alpha project decision")
                    && fragment.contains("beta project decision"),
                "a blank session id must run unscoped, not against a session whose external id is \
                 empty; got: {fragment}"
            );
        }
        assert_eq!(
            sorted_lines(&empty),
            sorted_lines(&omitted),
            "an empty session_id= must produce exactly the unscoped results"
        );
        assert_eq!(
            sorted_lines(&whitespace),
            sorted_lines(&omitted),
            "a whitespace-only session_id must produce exactly the unscoped results"
        );
    }

    /// The two handler tests above post the wire format, so they cannot see the template half of
    /// the contract: if an input's `name` drifted from a `QueryForm` field the field would simply
    /// stop being submittable and every handler test would still pass. This renders the form.
    #[test]
    fn test_query_form_template_names_every_query_form_field() {
        let html = super::QueryTemplate { nav: "query" }.render().unwrap();

        assert!(
            html.contains(r#"class="active">Query Tester"#),
            "layout must highlight the Query Tester nav entry; got: {html}"
        );
        assert!(
            html.contains(r##"hx-post="/debug/query/run" hx-target="#query-results""##),
            "the swap targets are part of the contract with `run`; got: {html}"
        );
        // One `name` per QueryForm field, so the struct stays submittable in full.
        for name in ["mode", "query", "limit", "session_id", "dry_run"] {
            assert!(
                html.contains(&format!(r#"name="{name}""#)),
                "`name=\"{name}\"` missing from the form; got: {html}"
            );
        }
        // Both retrieve-only fields must say so on the label, since neither is hidden when
        // mode=recall is selected.
        assert!(
            html.contains("Limit (retrieve only)") && html.contains("Session ID (retrieve only)"),
            "fields recall ignores must be labelled retrieve-only; got: {html}"
        );
        assert!(
            html.contains(r##"name="session_id" placeholder="external session id,"##),
            "the session id input must be free text hinting at the expected value; got: {html}"
        );
        // Nothing is pre-filled — `run` answers with a fragment only, so this form is never
        // re-rendered after a submission. A `value="{{ ... }}"` here would be a lie.
        assert!(
            !html.contains(r#"session_id" value="#),
            "the form must not pretend to persist submissions; got: {html}"
        );
        // The dry-run control must be a checkbox carrying exactly the value [`QueryForm`] expects
        // to see when it is present: a text input posting "" would make the absent-vs-empty
        // distinction the handler relies on unreachable through the real form.
        assert!(
            html.contains(r##"type="checkbox" name="dry_run" value="true""##),
            "dry run must be a checkbox posting `true`; got: {html}"
        );
        // Recall writes nothing, so the knob means nothing there — the label has to say so.
        assert!(
            html.contains("Dry run (retrieve only)"),
            "the dry-run label must be marked retrieve-only; got: {html}"
        );
        // The mutating default is surfaced without the operator having to read `server.rs`.
        assert!(
            html.contains("it bumps heat on the top-ranked results"),
            "the form must warn that a non-dry retrieve run writes heat; got: {html}"
        );
        assert!(
            !html.contains(r#"name="dry_run" value="true" checked"#),
            "dry run must start unchecked, so the tester keeps the real tool's behaviour; got: {html}"
        );
    }

    // --- The dry-run checkbox, end to end ---------------------------------------
    //
    // `server.rs` already proves `do_retrieve_memories_dry` does not write heat. The paired
    // wet/dry tests below prove the *form field* is what selects it: a handler that read `dry_run`
    // into a variable and then called the activating path anyway would pass every server-level
    // test while lying to the operator.

    /// A two-memory graph with a real edge between them, which is what spreading activation walks.
    /// Same construction the server-level fixture uses: `create_fact` alone is not enough, because
    /// `HeatRepo::add_heat` updates an existing `heat_state` row and silently matches nothing when
    /// there is none — so these go through `do_store_memory`, which creates one.
    async fn seed_edge_pair(server: &crate::AlexandriaServer) -> (String, String) {
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
        let mut ids = Vec::new();
        for content in ["alpha project decision", "beta project decision"] {
            // `do_store_memory` so each fact gets its `heat_state` row; the stub embedding makes
            // every similarity 1.0, so both memories are always above the retrieve floor.
            ids.push(
                server
                    .do_store_memory(crate::tools::StoreMemoryParams {
                        content: content.to_string(),
                        tags: None,
                        session_id: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        edge_repo
            .create_edge(&ids[0], &ids[1], "relates_to", 1.0)
            .await
            .unwrap();
        (ids[0].clone(), ids[1].clone())
    }

    /// Heat of both seeded memories. Read through `HeatRepo` — no raw query from a route test.
    async fn seeded_heat(
        server: &crate::AlexandriaServer,
        ids: &(String, String),
    ) -> (Option<f64>, Option<f64>) {
        let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
        (
            heat_repo.get(&ids.0).await.unwrap().map(|h| h.heat),
            heat_repo.get(&ids.1).await.unwrap().map(|h| h.heat),
        )
    }

    /// The control: the default (non-dry) submission must still behave like the real tool, heat
    /// write included, and it is what stops the dry assertion below passing on a fixture that
    /// cannot activate at all.
    #[tokio::test]
    async fn test_query_run_retrieve_writes_heat_without_dry_run() {
        let server = super::super::test_support::test_server().await;
        let ids = seed_edge_pair(&server).await;
        let before = seeded_heat(&server, &ids).await;
        assert_eq!(
            before,
            (Some(1.0), Some(1.0)),
            "each seeded memory starts at heat 1.0"
        );

        let app = crate::debug::router(server.clone());
        let fragment = run_form(app, "mode=retrieve&query=project+decision&limit=10").await;
        assert!(
            fragment.contains("alpha project decision")
                && fragment.contains("beta project decision"),
            "the query must actually retrieve the seeded pair; got: {fragment}"
        );

        let after = seeded_heat(&server, &ids).await;
        // One hop at the default propagation factor: 1.0 * 0.3^1 * edge strength 1.0.
        let warmed = |h: Option<f64>| h.is_some_and(|h| (h - 1.3).abs() < 1e-3);
        assert!(
            warmed(after.0) && warmed(after.1),
            "a non-dry retrieve run must warm both ends of the edge, exactly as \
             retrieve_memories does; before {before:?}, after {after:?}"
        );
    }

    /// `dry_run=true` in the form body must reach `do_retrieve_memories_dry`.
    #[tokio::test]
    async fn test_query_run_retrieve_dry_run_does_not_write_heat() {
        let server = super::super::test_support::test_server().await;
        let ids = seed_edge_pair(&server).await;
        let before = seeded_heat(&server, &ids).await;

        let app = crate::debug::router(server.clone());
        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;
        assert!(
            fragment.contains("alpha project decision")
                && fragment.contains("beta project decision"),
            "dry must still run the retrieval and show its results; got: {fragment}"
        );
        assert_eq!(
            seeded_heat(&server, &ids).await,
            before,
            "a dry retrieve run must leave heat exactly where it was"
        );
    }

    /// Recall writes nothing, so the checkbox must not change its behaviour: same results with
    /// and without it. If a dry branch were ever added to recall, this is the test that says it
    /// bought nothing.
    #[tokio::test]
    async fn test_query_run_recall_ignores_dry_run() {
        let server = super::super::test_support::test_server().await;
        server
            .do_store_memory(crate::tools::StoreMemoryParams {
                content: "a recallable fact".to_string(),
                tags: None,
                session_id: None,
            })
            .await
            .unwrap();
        let app = crate::debug::router(server.clone());

        let plain = run_form(app.clone(), "mode=recall&query=recallable+fact").await;
        let dry = run_form(app, "mode=recall&query=recallable+fact&dry_run=true").await;
        assert_eq!(
            sorted_lines(&plain),
            sorted_lines(&dry),
            "recall writes nothing, so dry_run must be a no-op there"
        );
    }
}
