use std::collections::HashMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::html::{self, error_page, page, unavailable};
use crate::AlexandriaServer;
use crate::server::record_id_to_string;

/// Rows per page when the caller does not ask for a specific `?limit=`.
const DEFAULT_LIMIT: usize = 50;

/// Sessions are listed by offset, not by page number, so the hrefs carry `limit` + `offset`.
///
/// There are no filters on this page yet, which is why the helper takes nothing else — but the
/// href shape is offset-based (like `memories.rs`) rather than `?page=N` (like `maintenance.rs`)
/// precisely so a future `?agent=` survives the hop the same way `memories_url` carries search.
fn sessions_url(limit: usize, offset: usize) -> String {
    format!("/debug/sessions?limit={limit}&offset={offset}")
}

// The `—` placeholder and the timestamp format are shared, not local: `html::ABSENT` and
// `html::format_dt`. This file used to own a private copy of each, which is how the pages drifted.

/// One row of the sessions table, flattened out of `SessionSummary` so the template never deals
/// with `Option`, with the timestamps, or with how the state badge is worded.
struct SessionRow {
    external_id: String,
    agent: String,
    model: String,
    memory_count: usize,
    started: String,
    /// Last activity. Deliberately not called "ended" — see the note under the table.
    last_activity: String,
    /// `summary.is_some()`. The template owns the badge markup; this carries the discriminator.
    finalized: bool,
}

/// `prev_href` / `next_href` / `summary` feed `templates/_pagination.html`'s `pager` macro,
/// which is presentational: the full hrefs are built here via [`sessions_url`] and an empty
/// string means no link on that side.
#[derive(Template)]
#[template(path = "sessions.html")]
struct SessionsTemplate {
    nav: &'static str,
    rows: Vec<SessionRow>,
    prev_href: String,
    next_href: String,
    summary: String,
}

/// `?limit=` / `?offset=`, read from a free-form map rather than a typed struct so that junk
/// falls through to the page defaults instead of failing the request with a 400 — the same
/// tolerance `memories::list` gives its own pagination and `graph::api_graph` gives `?hops`.
/// A hand-typed `?limit=abc` is a typo, not a malformed request, and this page is a diagnostic
/// surface that a failing request would leave the operator staring at.
pub async fn list(
    State(server): State<AlexandriaServer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_LIMIT);
    let offset: usize = params
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
    let sessions = match repo.list(limit, offset).await {
        Ok(s) => s,
        Err(e) => return error_page("sessions", &e.to_string()),
    };
    let total = repo.count().await.unwrap_or(sessions.len());

    let rows: Vec<SessionRow> = sessions
        .iter()
        .map(|s| SessionRow {
            external_id: s.external_id.clone(),
            agent: s
                .agent_id
                .clone()
                .unwrap_or_else(|| html::ABSENT.to_string()),
            model: s.model.clone().unwrap_or_else(|| html::ABSENT.to_string()),
            memory_count: s.memory_count,
            started: html::format_dt(s.started_at),
            last_activity: html::format_dt(s.ended_at),
            // Finalized is *only* ever the summary. `ended_at` is written by `touch()` on every
            // attached memory, so a live idle session has one too and would be mislabelled.
            finalized: s.summary.is_some(),
        })
        .collect();

    let showing_from = if rows.is_empty() { 0 } else { offset + 1 };
    let showing_to = offset + rows.len();
    let summary = format!("Showing {showing_from}\u{2013}{showing_to} of {total} sessions");

    let prev_href = if offset > 0 {
        sessions_url(limit, offset.saturating_sub(limit))
    } else {
        String::new()
    };
    let next_href = if offset + rows.len() < total {
        sessions_url(limit, offset + limit)
    } else {
        String::new()
    };

    page(SessionsTemplate {
        nav: "sessions",
        rows,
        prev_href,
        next_href,
        summary,
    })
}

/// A memory attached to a session. `id` is the raw record id: the template urlencodes it for
/// the href and prints it as-is for the link text.
///
/// The preview is a plain 120-char cut with no ellipsis, matching the linked-rows table in
/// `cluster_detail.html`, which is the same "child rows on a detail page" shape.
struct MemoryRow {
    id: String,
    preview: String,
    created: String,
}

#[derive(Template)]
#[template(path = "session_detail.html")]
struct SessionDetailTemplate {
    nav: &'static str,
    external_id: String,
    agent: String,
    model: String,
    started: String,
    last_activity: String,
    /// `summary.is_some()` — never derived from `ended_at`; see [`SessionRow::finalized`].
    finalized: bool,
    summary: Option<String>,
    tags: Vec<String>,
    memories: Vec<MemoryRow>,
}

pub async fn detail(
    State(server): State<AlexandriaServer>,
    Path(external_id): Path<String>,
) -> Response {
    let repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());

    // Keyed on `external_id`, the id operators actually pass to `store_memory(session_id)` and
    // `get_session`, not the internal RecordId.
    let session = match repo.find_by_external_id(&external_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            // Unlike the memories detail page, an unknown id has to be a 404 rather than
            // `error_page`'s 200, so the status is set explicitly on the shared error template.
            let mut response = error_page("sessions", "Session not found.");
            *response.status_mut() = StatusCode::NOT_FOUND;
            return response;
        }
        // A storage failure is not the 404 above: the record may exist and still be unreadable.
        Err(e) => return unavailable("sessions", "session", e),
    };

    // `get_memories` already filters `deleted = false`, so the detail page cannot show a memory
    // the list's derived `memory_count` refused to count.
    let memories = match repo.get_memories(&external_id).await {
        Ok(m) => m,
        // Used to take `error_page`'s 200 while the branch above answered 500 — one page, one
        // fault class, two statuses.
        Err(e) => return unavailable("sessions", "session memories", e),
    };

    let memories = memories
        .iter()
        .map(|fact| MemoryRow {
            id: fact
                .id
                .as_ref()
                .map(record_id_to_string)
                .unwrap_or_default(),
            preview: fact.content.chars().take(120).collect(),
            created: html::format_dt(fact.created_at),
        })
        .collect();

    page(SessionDetailTemplate {
        nav: "sessions",
        external_id,
        agent: session.agent_id.unwrap_or_else(|| html::ABSENT.to_string()),
        model: session.model.unwrap_or_else(|| html::ABSENT.to_string()),
        started: html::format_dt(session.started_at),
        last_activity: html::format_dt(session.ended_at),
        finalized: session.summary.is_some(),
        summary: session.summary,
        tags: session.tags,
        memories,
    })
}

#[cfg(test)]
mod tests {
    use super::{SessionDetailTemplate, SessionsTemplate};
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn fetch_body(app: axum::Router, uri: &str) -> String {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "unexpected status for {uri}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// A session with two live memories and one soft-deleted one, plus a finalized sibling.
    async fn seed(server: &crate::AlexandriaServer) {
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());

        let sess = session_repo
            .create("pi:sess-7", Some("agent-alpha"), Some("claude-sonnet-4-5"))
            .await
            .unwrap();
        for content in ["first memory", "second memory"] {
            let fact = memory_repo
                .create_fact(content, 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
            session_repo.add_memory(&sess, &fact).await.unwrap();
        }
        let gone = memory_repo
            .create_fact("deleted memory", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        session_repo.add_memory(&sess, &gone).await.unwrap();
        memory_repo.soft_delete_fact(&gone).await.unwrap();
        session_repo.touch("pi:sess-7").await.unwrap();

        let other = session_repo.create("sess-plain", None, None).await.unwrap();
        let kept = memory_repo
            .create_fact("kept", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        session_repo.add_memory(&other, &kept).await.unwrap();
    }

    #[tokio::test]
    async fn test_sessions_list_shows_external_ids_and_derived_counts() {
        let server = super::super::test_support::test_server().await;
        seed(&server).await;

        let text = fetch_body(crate::debug::router(server), "/debug/sessions").await;
        assert!(
            text.contains("pi:sess-7"),
            "external_id must be the visible key: {text}"
        );
        assert!(text.contains("agent-alpha"));
        assert!(text.contains("claude-sonnet-4-5"));
        // Derived from the traversal: 2 live, the soft-deleted one excluded.
        assert!(
            text.contains(r#"<td>2</td>"#),
            "expected the derived memory count for pi:sess-7; got: {text}"
        );
        // Column headers, including the deliberate "Last activity" wording.
        assert!(text.contains("Last activity"));
        assert!(text.contains("Started"));
        // Handler-level, one page of two seeded sessions = at least two `started` cells rendered
        // by `sessions::list` itself rather than by a test fixture.
        super::super::html::assert_shared_timestamp_cells(&text, "GET /debug/sessions", 2);
    }

    #[tokio::test]
    async fn test_sessions_list_marks_finalized_only_from_summary() {
        let server = super::super::test_support::test_server().await;
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());

        // Touched but NOT finalized: it has a real ended_at, so a badge derived from
        // `ended_at.is_some()` would wrongly label it finalized. This is the assertion the
        // whole test exists to make.
        session_repo
            .create("sess-touched-only", None, None)
            .await
            .unwrap();
        session_repo.touch("sess-touched-only").await.unwrap();
        assert!(
            session_repo
                .find_by_external_id("sess-touched-only")
                .await
                .unwrap()
                .unwrap()
                .ended_at
                .is_some(),
            "test setup: touch() must have written ended_at"
        );

        // Never touched: no ended_at at all.
        session_repo.create("sess-never", None, None).await.unwrap();

        // Genuinely finalized.
        session_repo.create("sess-final", None, None).await.unwrap();
        session_repo
            .finalize("sess-final", Some("a summary"), None)
            .await
            .unwrap();

        let text = fetch_body(crate::debug::router(server), "/debug/sessions").await;
        let row_of = |id: &str| -> String {
            let start = text
                .find(id)
                .unwrap_or_else(|| panic!("row for {id} missing; got: {text}"));
            text[start..].split("</tr>").next().unwrap().to_string()
        };

        let touched = row_of("sess-touched-only");
        assert!(
            touched.contains(r#"<span class="badge">active</span>"#),
            "touched-but-not-finalized must read as active; got: {touched}"
        );
        assert!(
            !touched.contains("finalized"),
            "touched-but-not-finalized must not read as finalized; got: {touched}"
        );
        assert!(
            row_of("sess-final").contains(r#"<span class="badge">finalized</span>"#),
            "a session with a summary must read as finalized"
        );
        assert!(
            row_of("sess-never").contains(r#"<span class="badge">active</span>"#),
            "a never-touched session must read as active"
        );
    }

    #[tokio::test]
    async fn test_sessions_list_escapes_xss_payload_in_agent_id() {
        // Security regression test: agent_id is free-form text an operator controls, it is a
        // column here, and so the escaped element is assertable whole. askama 0.16 writes
        // character references numerically (`&#60;`, not the named `&lt;`).
        let server = super::super::test_support::test_server().await;
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        session_repo
            .create("sess-xss", Some("<script>alert(1)</script>"), None)
            .await
            .unwrap();

        let text = fetch_body(crate::debug::router(server), "/debug/sessions").await;
        assert!(
            !text.contains("<script>alert(1)</script>"),
            "raw <script> tag must not appear unescaped; got: {text}"
        );
        assert!(
            text.contains(r#"<td>&#60;script&#62;alert(1)&#60;/script&#62;</td>"#),
            "agent_id must render HTML-escaped; got: {text}"
        );
    }

    #[tokio::test]
    async fn test_session_detail_escapes_xss_payload_in_tags_and_summary() {
        // Tags and the summary render on the *detail* page, not on the list (there is no Tags
        // column there), so that is where those payloads have to be proven inert. Asserting
        // their absence on the list would be vacuous: the list never renders them at all.
        let server = super::super::test_support::test_server().await;
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        session_repo
            .create("sess-tagged", None, None)
            .await
            .unwrap();
        session_repo
            .finalize(
                "sess-tagged",
                Some("<svg onload=alert(4)>"),
                Some(&["<img src=x onerror=alert(2)>".to_string()]),
            )
            .await
            .unwrap();

        let detail = fetch_body(crate::debug::router(server), "/debug/sessions/sess-tagged").await;
        assert!(
            detail.contains(r#"<span class="badge">&#60;img src=x onerror=alert(2)&#62;</span>"#),
            "tag must render HTML-escaped; got: {detail}"
        );
        assert!(
            detail.contains(r#"<pre class="content-block">&#60;svg onload=alert(4)&#62;</pre>"#),
            "summary must render HTML-escaped; got: {detail}"
        );
        for raw in ["<img src=x onerror=alert(2)>", "<svg onload=alert(4)>"] {
            assert!(
                !detail.contains(raw),
                "raw payload reached the response: {raw}"
            );
        }
    }

    /// `?limit=abc` and `?limit=` must render the default page, not a 400. `fetch_body` asserts
    /// 200, so this fails at the request rather than on a missing substring.
    #[tokio::test]
    async fn test_sessions_list_tolerates_junk_pagination_params() {
        let server = super::super::test_support::test_server().await;
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        for id in ["sess-1", "sess-2", "sess-3"] {
            session_repo.create(id, None, None).await.unwrap();
        }

        let app = crate::debug::router(server);
        let junk = fetch_body(app.clone(), "/debug/sessions?limit=abc&offset=%20").await;
        let blank = fetch_body(app.clone(), "/debug/sessions?limit=&offset=").await;
        let plain = fetch_body(app, "/debug/sessions").await;

        // DEFAULT_LIMIT (50) is above the three seeded sessions, so all three rows appear on
        // every variant — the junk values must behave as "no opinion", not as zero.
        for text in [&junk, &blank, &plain] {
            for id in ["sess-1", "sess-2", "sess-3"] {
                assert!(
                    text.contains(&format!(r##"href="/debug/sessions/{id}">"##)),
                    "unparseable or empty limit/offset must fall back to the default page size; \
                     missing {id} in one of the rendered pages"
                );
            }
        }
        assert_eq!(
            junk.matches("<tr>").count(),
            plain.matches("<tr>").count(),
            "a junk limit must render exactly the default page"
        );
        assert_eq!(
            blank.matches("<tr>").count(),
            plain.matches("<tr>").count(),
            "a blank limit must render exactly the default page"
        );
    }

    #[tokio::test]
    async fn test_sessions_list_pagination_and_pager_hrefs() {
        let server = super::super::test_support::test_server().await;
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        for id in ["sess-1", "sess-2", "sess-3"] {
            session_repo.create(id, None, None).await.unwrap();
        }

        let app = crate::debug::router(server);
        let first = fetch_body(app.clone(), "/debug/sessions?limit=2&offset=0").await;
        assert!(
            first.contains("of 3 sessions"),
            "summary must use the total, not the page size; got: {first}"
        );
        // The & inside the href is escaped to &#38; by askama; that is correct in an attribute.
        assert!(
            first.contains(r#"href="/debug/sessions?limit=2&#38;offset=2">Next →"#),
            "first page must link forward with the next offset; got: {first}"
        );
        assert!(
            !first.contains("Prev"),
            "first page must not link backwards; got: {first}"
        );

        let second = fetch_body(app.clone(), "/debug/sessions?limit=2&offset=2").await;
        assert!(
            second.contains(r#"href="/debug/sessions?limit=2&#38;offset=0">← Prev"#),
            "second page must link back to offset 0; got: {second}"
        );
        assert!(
            !second.contains("Next"),
            "last page must not link forward; got: {second}"
        );

        // Every row must be reachable: the two pages together cover all three sessions. Matching
        // on the row link rather than a bare substring of the page keeps the nav and the note
        // from counting as hits.
        let mut seen = Vec::new();
        for page_text in [first, second] {
            for id in ["sess-1", "sess-2", "sess-3"] {
                if page_text.contains(&format!(r#"href="/debug/sessions/{id}">"#)) {
                    seen.push(id);
                }
            }
        }
        seen.sort();
        assert_eq!(
            seen,
            vec!["sess-1", "sess-2", "sess-3"],
            "paging the sessions list must neither repeat nor skip a session; saw {seen:?}"
        );
    }

    #[tokio::test]
    async fn test_session_detail_shows_summary_and_memories() {
        let server = super::super::test_support::test_server().await;
        seed(&server).await;
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        session_repo
            .finalize(
                "pi:sess-7",
                Some("the session summary"),
                Some(&["review".to_string()]),
            )
            .await
            .unwrap();

        let text = fetch_body(crate::debug::router(server), "/debug/sessions/pi%3Asess-7").await;
        assert!(text.contains("pi:sess-7"));
        assert!(
            text.contains(r#"<pre class="content-block">the session summary</pre>"#),
            "summary must render in the content block; got: {text}"
        );
        assert!(text.contains("first memory"));
        assert!(text.contains("second memory"));
        assert!(
            text.contains(r#"<span class="badge">review</span>"#),
            "tags must render as badges; got: {text}"
        );
        assert!(text.contains("agent-alpha"));
        assert!(text.contains("Last activity"));
        assert!(
            text.contains("← Back to sessions"),
            "detail page must link back to the list; got: {text}"
        );
    }

    #[tokio::test]
    async fn test_session_detail_memory_rows_link_to_memory_detail() {
        let server = super::super::test_support::test_server().await;
        seed(&server).await;
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        let fact_id = memory_repo
            .create_fact("linked", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        let sess = session_repo
            .find_by_external_id("pi:sess-7")
            .await
            .unwrap()
            .unwrap()
            .id
            .unwrap();
        session_repo
            .add_memory(&crate::server::record_id_to_string(&sess), &fact_id)
            .await
            .unwrap();

        let text = fetch_body(crate::debug::router(server), "/debug/sessions/pi%3Asess-7").await;
        // The id in the href is urlencoded (`:` -> `%3A`); the link text is the plain id.
        let href = fact_id.replace(':', "%3A");
        assert!(
            text.contains(&format!(
                r#"<a class="link" href="/debug/memories/{href}">{fact_id}</a>"#
            )),
            "memory rows must link to the memory detail page; got: {text}"
        );
    }

    #[tokio::test]
    async fn test_session_detail_excludes_soft_deleted_memories() {
        // Pins the `deleted = false` filter in `SessionRepo::get_memories` at the UI layer too:
        // the storage test covers the query, this covers the page an operator reads.
        let server = super::super::test_support::test_server().await;
        seed(&server).await;

        let text = fetch_body(crate::debug::router(server), "/debug/sessions/pi%3Asess-7").await;
        assert!(text.contains("first memory"));
        assert!(
            !text.contains("deleted memory"),
            "soft-deleted memory must not appear on the session page; got: {text}"
        );
    }

    #[tokio::test]
    async fn test_session_detail_404_for_unknown_external_id() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/sessions/nope-not-here")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "an unknown id must be a real 404");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("Session not found."),
            "the 404 must still carry the shared error page; got: {text}"
        );
    }

    fn sessions_template() -> SessionsTemplate {
        SessionsTemplate {
            nav: "sessions",
            rows: vec![
                super::SessionRow {
                    external_id: "pi:sess-7".into(),
                    agent: "agent-alpha".into(),
                    model: "claude-sonnet-4-5".into(),
                    memory_count: 2,
                    started: "2026-09-11 10:00 UTC".into(),
                    last_activity: "2026-09-11 10:05 UTC".into(),
                    finalized: true,
                },
                super::SessionRow {
                    external_id: "sess-idle".into(),
                    agent: super::html::ABSENT.into(),
                    model: super::html::ABSENT.into(),
                    memory_count: 0,
                    started: "2026-09-11 09:00 UTC".into(),
                    last_activity: "2026-09-11 09:01 UTC".into(),
                    finalized: false,
                },
            ],
            prev_href: String::new(),
            next_href: "/debug/sessions?limit=2&offset=2".into(),
            summary: "Showing 1\u{2013}2 of 3 sessions".into(),
        }
    }

    /// The list template's only other coverage is through handlers that never finalise anything
    /// on both sides of one page, so the badge branch and the nav highlight would rot silently.
    /// The shared timestamp contract on both session pages.
    ///
    /// `sessions.rs` already agreed with `memories.rs` on the format, but it agreed by
    /// coincidence: two private copies of the same `strftime` string and the same `—` const. This
    /// asserts the *rendered* cells; the handler's own bytes are covered by
    /// `assert_shared_timestamp_cells` in `test_sessions_list_shows_external_ids_and_derived_counts`.
    #[test]
    fn test_sessions_pages_use_the_shared_timestamp_format_and_absent_marker() {
        use crate::debug::html;

        let mut tpl = sessions_template();
        tpl.rows[0].started = html::format_dt(Some(html::example_dt()));
        tpl.rows[1].last_activity = html::format_dt(None);
        let html_list = tpl.render().unwrap();
        assert!(
            html_list.contains("<td>2026-07-05 11:50 UTC</td>"),
            "the shared minute form must appear verbatim; got: {html_list}"
        );
        assert!(
            html_list.contains(&format!("<td>{}</td>", html::ABSENT)),
            "an absent last-activity must render the shared marker; got: {html_list}"
        );
        assert!(
            !html_list.contains("<td></td>"),
            "the list must not emit an empty cell; got: {html_list}"
        );
        html::assert_no_seconds_timestamp(&html_list, "sessions.html");

        let mut detail = detail_template(None);
        detail.started = html::format_dt(Some(html::example_dt()));
        detail.last_activity = html::format_dt(None);
        let html_detail = detail.render().unwrap();
        assert!(
            html_detail.contains("2026-07-05 11:50 UTC"),
            "got: {html_detail}"
        );
        assert!(
            html_detail.contains(html::ABSENT),
            "the detail page must use the same marker for a session with no recorded activity; got: {html_detail}"
        );
        html::assert_no_seconds_timestamp(&html_detail, "session_detail.html");
    }

    #[test]
    fn test_sessions_template_renders_badge_branch_and_nav_and_urlencodes_ids() {
        let html = sessions_template().render().unwrap();

        assert!(
            html.contains(r#"class="active">Sessions"#),
            "layout must highlight the Sessions nav entry; got: {html}"
        );
        assert!(
            html.contains(r#"<span class="badge">finalized</span>"#),
            "the finalized branch must render; got: {html}"
        );
        assert!(
            html.contains(r#"<span class="badge">active</span>"#),
            "the active branch must render; got: {html}"
        );
        // External ids contain `:` and `/`; the href must be percent-encoded, the link text
        // must not be. A browser does not decode character references inside a URL.
        assert!(
            html.contains(r#"href="/debug/sessions/pi%3Asess-7">pi:sess-7</a>"#),
            "external_id must be urlencoded in the href and verbatim as link text; got: {html}"
        );
        // The `&` in the pager href is escaped by askama, which is correct in an attribute.
        assert!(
            html.contains(r#"href="/debug/sessions?limit=2&#38;offset=2">Next →"#),
            "pager must carry the full offset href; got: {html}"
        );
        assert!(
            html.contains("Showing 1\u{2013}2 of 3 sessions"),
            "got: {html}"
        );
        // An absent agent/model still renders the em-dash fallback rather than an empty cell.
        assert!(
            html.contains(&format!(
                "<td>{}</td><td>{}</td>",
                super::html::ABSENT,
                super::html::ABSENT
            )),
            "got: {html}"
        );
    }

    #[test]
    fn test_sessions_template_falls_back_to_summary_line_without_links() {
        let mut tpl = sessions_template();
        tpl.prev_href = String::new();
        tpl.next_href = String::new();
        let html = tpl.render().unwrap();
        assert!(
            !html.contains("Next") && !html.contains("Prev"),
            "the macro must contribute no links; got: {html}"
        );
        assert!(
            html.contains("Showing 1\u{2013}2 of 3 sessions"),
            "the page owns the single-page fallback wording; got: {html}"
        );
    }

    fn detail_template(summary: Option<&str>) -> SessionDetailTemplate {
        SessionDetailTemplate {
            nav: "sessions",
            external_id: "pi:sess-7".into(),
            agent: super::html::ABSENT.into(),
            model: super::html::ABSENT.into(),
            started: "2026-09-11 10:00 UTC".into(),
            last_activity: super::html::ABSENT.into(),
            finalized: summary.is_some(),
            summary: summary.map(String::from),
            tags: vec![],
            memories: vec![],
        }
    }

    /// A session that was never finalised must say so instead of rendering an empty box, and a
    /// session with an empty memory list must not render an empty table.
    #[test]
    fn test_session_detail_template_renders_not_finalized_state() {
        let html = detail_template(None).render().unwrap();
        assert!(
            html.contains("Not finalized"),
            "the not-finalized state must be explicit; got: {html}"
        );
        assert!(
            !html.contains(r#"<pre class="content-block">"#),
            "an absent summary must not render an empty content block; got: {html}"
        );
        assert!(
            html.contains(r#"<span class="badge">active</span>"#),
            "got: {html}"
        );
        assert!(
            !html.contains(r#"<span class="badge">finalized</span>"#),
            "got: {html}"
        );
        assert!(
            html.contains("No memories"),
            "an empty memory list must be explicit; got: {html}"
        );
        assert!(html.contains(r#"class="active">Sessions"#), "got: {html}");
        // The last-activity caveat belongs on the detail page too, since it shows the field.
        assert!(html.contains("Last activity"), "got: {html}");
    }

    #[test]
    fn test_session_detail_template_renders_summary_when_finalized() {
        let html = detail_template(Some("a summary with <b>bold</b> prose"))
            .render()
            .unwrap();
        assert!(
            html
            .contains(r#"<pre class="content-block">a summary with &#60;b&#62;bold&#60;/b&#62; prose</pre>"#),
            "summary must render escaped inside the content block; got: {html}"
        );
        assert!(!html.contains("Not finalized"), "got: {html}");
        assert!(
            html.contains(r#"<span class="badge">finalized</span>"#),
            "got: {html}"
        );
    }

    #[tokio::test]
    async fn test_sessions_list_and_detail_are_read_only() {
        // The debug UI is unauthenticated *because* nothing mutates. A form or a non-GET control
        // here would break that premise, so it is part of the contract, not an accident.
        let server = super::super::test_support::test_server().await;
        seed(&server).await;
        let app = crate::debug::router(server);
        let list = fetch_body(app.clone(), "/debug/sessions").await;
        let detail = fetch_body(app, "/debug/sessions/pi%3Asess-7").await;
        for page in [&list, &detail] {
            assert!(
                !page.contains("<form"),
                "the sessions pages must not contain a form"
            );
            assert!(!page.contains("hx-post"), "no htmx write triggers allowed");
            assert!(!page.contains("<input"), "no inputs allowed");
        }
    }

    /// Storage failure ⇒ `html::UNAVAILABLE_STATUS`, asserted on bytes the handler produced.
    ///
    /// Forced by handing the handler a database that was never migrated: the first repo call then
    /// fails for real, with no mock and no change to `alexandria-storage`. The body assertion matters
    /// as much as the status one — it is what proves this page goes through `html::unavailable`
    /// rather than re-inlining `error_page` plus a `status_mut` (the old `memories.rs` shape), which
    /// would keep the status identical and only change the wording.
    #[tokio::test]
    async fn test_session_storage_failure_answers_with_the_one_status() {
        let db = alexandria_storage::Database::connect_embedded()
            .await
            .unwrap();
        let server = crate::AlexandriaServer::new(
            std::sync::Arc::new(db),
            std::sync::Arc::new(super::super::test_support::StubEmbedding),
            0.75,
            86400.0,
        );
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/debug/sessions/pi%3Asess-7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The literal, not `html::UNAVAILABLE_STATUS`: asserting against the constant would pass no
        // matter what status it names, which is the opposite of pinning one.
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "a storage failure must answer with the one detail-page status"
        );
        assert_eq!(
            response.status(),
            super::super::html::UNAVAILABLE_STATUS,
            "…and the shared constant must still name it"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("storage error while loading session"),
            "the page must name what failed, via the shared helper; got: {text}"
        );
    }
}
