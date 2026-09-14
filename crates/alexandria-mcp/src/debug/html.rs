//! HTML responses for the debug UI.
//!
//! Every page is an askama template under `crates/alexandria-mcp/templates/`, rendered
//! through [`page()`]. askama auto-escapes `{{ }}` at compile time, so escaping is
//! structural rather than a per-call-site habit — there is deliberately no escape helper
//! here to reach for. Values interpolated into a URL rather than into HTML text need the
//! right *context* escape (`|urlencode`), which templates apply explicitly.
//!
//! # Presentation contracts
//!
//! Four renderings are shared across every debug page and must not be re-invented per handler
//! or per template; each drifted at least once before being hoisted here:
//!
//! 1. **Absent value** — [`ABSENT`], never an empty cell and never a per-page glyph.
//! 2. **Timestamp** — [`format_dt`] / [`DT_FORMAT`], minute resolution, explicit `UTC`.
//! 3. **Empty table** — the header row still renders, followed by a single
//!    `<tr><td colspan="N" class="empty">…</td></tr>`; the count/summary line still renders too.
//!    A bare `<p>No rows.</p>` in place of the table is the old, retired shape: it drops the
//!    column context an operator needs to read the emptiness.
//! 4. **Storage failure on a detail page** — [`unavailable`], one status for one fault class.

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Utc};

/// The single placeholder for "this value is absent", across every debug page.
///
/// Lives here rather than in a handler because a missing agent id on `/debug/sessions` and a
/// missing `created_at` on `/debug/memories` must not render two different glyphs; it used to.
pub const ABSENT: &str = "—";

/// The single timestamp format, across every debug page.
///
/// Minute resolution, explicit `UTC` suffix because the value is naive-local to nobody and the
/// page has no other hint as to the zone. `maintenance` used to print seconds
/// (`%H:%M:%S UTC`) and an empty cell when the timestamp was absent; losing the seconds there is
/// deliberate — one concept, one rendering, and the maintenance log's ordering is already the
/// `created_at` column, so the extra precision was never load-bearing.
pub const DT_FORMAT: &str = "%Y-%m-%d %H:%M UTC";

/// Renders an optional timestamp with the shared format, so a call site never has to invent its
/// own placeholder for "never".
///
/// This lives here because a timestamp format is a *presentation* decision: with one copy per
/// handler, four handlers meant four ways to drift, and they drifted (see [`DT_FORMAT`]).
pub fn format_dt(dt: Option<DateTime<Utc>>) -> String {
    match dt {
        Some(dt) => dt.format(DT_FORMAT).to_string(),
        None => ABSENT.to_string(),
    }
}

/// Render a template, mapping render failure to a plain-text 500.
///
/// A render failure is a programming error, not a data error — fall back to plain
/// text rather than `error.html`, because rendering *that* could fail too.
pub fn page<T: Template>(tpl: T) -> Response {
    match tpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template render failed: {e}"),
        )
            .into_response(),
    }
}

/// The one status every debug **detail** page answers with when storage fails.
///
/// Promoted from a private helper in `clusters.rs`. One failure class had four spellings:
/// `clusters` 500, `memories` 500 inlined by hand, and `sessions` 500 for a failed
/// `find_by_external_id` but `error_page`'s **200** for a failed `get_memories` — the same fault on
/// one page returning two different statuses depending on which query tripped. A 200-with-error-text
/// beside a 500 is a lie in one direction or the other: the 200 tells a monitoring probe that the
/// diagnostic surface is healthy while storage is not. This keeps the status `clusters.rs` already
/// returned rather than inventing a third.
///
/// Deliberately scoped to the detail pages, whose entire content *is* the record that failed to
/// load. A list handler still answers `error_page`'s 200, because its filters, column headers and
/// nav remain useful context and that has always been its contract.
pub const UNAVAILABLE_STATUS: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

/// Renders the shared error page for a storage failure on a detail page, with
/// [`UNAVAILABLE_STATUS`]. `context` names which query failed, so a session that could not be read
/// is distinguishable from a session whose memories could not.
pub fn unavailable(nav: &'static str, context: &str, err: impl std::fmt::Display) -> Response {
    let mut response = error_page(
        nav,
        &format!("storage error while loading {context}: {err}"),
    );
    *response.status_mut() = UNAVAILABLE_STATUS;
    response
}

/// Data-layer failure (DB unreachable, bad record id) rendered through the layout.
///
/// Returns **200**. Callers that must preserve a legacy non-200 (cluster detail 500,
/// memory-not-found 404) set `*response.status_mut()` themselves — `status_mut` touches only
/// the status line, leaving the body and `text/html` content type intact.
pub fn error_page(nav: &'static str, message: &str) -> Response {
    page(ErrorTemplate {
        nav,
        message: message.to_string(),
    })
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub nav: &'static str,
    pub message: String,
}

/// Unit-level coverage of the `pager` contract in `templates/_pagination.html`.
///
/// askama compiles a template only when something derives from it, so this is also what keeps
/// the macro type-checked. **Kept deliberately even though `maintenance.html` and
/// `memories.html` now call the macro**: those call sites cannot reach the cases covered here.
/// `maintenance.html` is `?page=N` only and its integration tests never exceed one page, so it
/// exercises neither an `&`-bearing href (the `&#38;` escaping guard) nor the
/// both-sides-empty case. Do not delete this in Task 1.5.
#[cfg(test)]
#[derive(Template)]
#[template(path = "_test_pager.html")]
struct PagerTemplate {
    prev_href: String,
    next_href: String,
    summary: String,
}

/// One fixed instant shared by the per-page render tests, so "the pages agree" is checked against
/// a single value rather than three fixtures that each happen to look right.
#[cfg(test)]
pub(crate) fn example_dt() -> DateTime<Utc> {
    DateTime::from_timestamp(1_783_252_211, 0).expect("valid unix epoch seconds")
}

/// Guards the *rendered* pages, not the helper: a page that grew its own formatter would pass
/// every `format_dt` unit test. Fails if a page prints a seconds-precision timestamp, which is the
/// signature of the pre-sweep `maintenance.rs` format. A byte-window scan, because a `regex`
/// dependency for one assertion is not worth it.
#[cfg(test)]
pub(crate) fn assert_no_seconds_timestamp(html: &str, rendered_by: &str) {
    for w in html.as_bytes().windows(8) {
        if w[2] == b':' && w[5] == b':' && w.iter().all(|b| b.is_ascii_digit() || *b == b':') {
            panic!("{rendered_by} rendered a seconds-precision timestamp; got: {html}");
        }
    }
}

/// The strong half of the timestamp contract: asserts that every `<td>` cell ending in ` UTC` is
/// **exactly** the shared minute form, and that at least `min` such cells exist.
///
/// Called from handler tests (bytes the handler built), not only from fixture renders — a
/// `MaintenanceRow` assembled by hand in a test cannot notice that `maintenance.rs` grew its own
/// `strftime` again, and that is the regression this exists to catch. Catches both historical
/// failure modes at once: a revert to `%H:%M:%S` shifts the window so the shape check fails, and a
/// revert to `unwrap_or_default()` leaves no ` UTC` cell for `min` to find.
#[cfg(test)]
pub(crate) fn assert_shared_timestamp_cells(html: &str, rendered_by: &str, min: usize) {
    let mut found = 0usize;
    for (idx, _) in html.match_indices(" UTC</td>") {
        let Some(start) = idx.checked_sub(16) else {
            panic!("{rendered_by} rendered a short timestamp cell; got: {html}");
        };
        let cell = &html[start..idx];
        let shared_shape = cell.chars().enumerate().all(|(i, c)| match i {
            4 | 7 => c == '-',
            10 => c == ' ',
            13 => c == ':',
            _ => c.is_ascii_digit(),
        });
        assert!(
            shared_shape,
            "{rendered_by} rendered {cell:?} before \" UTC</td>\", which is not the shared \
             \"YYYY-MM-DD HH:MM\" form"
        );
        found += 1;
    }
    assert!(
        found >= min,
        "{rendered_by} rendered {found} timestamp cells, expected at least {min}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- timestamps ------------------------------------------------------------

    #[test]
    fn test_format_dt_renders_the_shared_minute_form_and_the_absent_marker() {
        assert_eq!(format_dt(Some(example_dt())), "2026-07-05 11:50 UTC");
        assert_eq!(format_dt(None), ABSENT);
        assert_eq!(ABSENT, "\u{2014}", "one em-dash, not a hyphen");
    }

    // --- templates -------------------------------------------------------------

    fn render_pager(prev_href: &str, next_href: &str, summary: &str) -> String {
        PagerTemplate {
            prev_href: prev_href.to_string(),
            next_href: next_href.to_string(),
            summary: summary.to_string(),
        }
        .render()
        .unwrap()
    }

    /// The layout hard-codes the asset path, so nothing else would notice if it drifted
    /// from the route that actually serves the file.
    #[test]
    fn test_layout_embeds_vendored_htmx_not_a_cdn() {
        let html = ErrorTemplate {
            nav: "dashboard",
            message: "x".into(),
        }
        .render()
        .unwrap();
        assert!(
            html.contains(super::super::assets::HTMX_URL),
            "layout must reference the vendored asset constant"
        );
        assert!(!html.contains("unpkg.com"), "no CDN references may remain");
    }

    #[test]
    fn test_error_template_escapes_message() {
        let html = ErrorTemplate {
            nav: "dashboard",
            message: "<script>alert(1)</script>".into(),
        }
        .render()
        .unwrap();
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "must be escaped"
        );
        // askama 0.16 writes character references numerically (`&#60;`, not the named `&lt;`).
        // Equivalent to a browser, different bytes — so assert the whole escaped paragraph,
        // which fails if escaping is ever weakened in either form.
        assert!(
            html.contains(r#"<p class="error">&#60;script&#62;alert(1)&#60;/script&#62;</p>"#),
            "got: {html}"
        );
    }

    /// The base template reads `nav`, so a child context that omits the field is a compile
    /// error rather than a page with no highlight; this pins the runtime half of the deal.
    #[test]
    fn test_layout_marks_active_nav() {
        let html = ErrorTemplate {
            nav: "clusters",
            message: "x".into(),
        }
        .render()
        .unwrap();
        assert!(
            html.contains(r#"class="active">Clusters"#),
            "active nav link not marked"
        );
        // and a non-active one must not be marked
        assert!(!html.contains(r#"class="active">Memories"#));
    }

    /// With neither link present the macro emits no row at all. Callers that want a fallback
    /// ("7 entries") supply it themselves — the macro must not guess.
    #[test]
    fn test_pager_macro_renders_nothing_without_links() {
        let html = render_pager("", "", "7 entries");
        assert!(!html.contains("pagination"), "got: {html}");
        assert!(
            !html.contains("7 entries"),
            "summary must not render without a row; got: {html}"
        );
    }

    #[test]
    fn test_pager_macro_renders_both_links_when_both_present() {
        let html = render_pager(
            "/debug/maintenance?page=1",
            "/debug/maintenance?page=3",
            "Page 2 of 3 (42 entries)",
        );
        assert!(
            html.contains(r#"href="/debug/maintenance?page=1""#),
            "got: {html}"
        );
        assert!(html.contains("← Prev"), "got: {html}");
        assert!(
            html.contains(r#"href="/debug/maintenance?page=3""#),
            "got: {html}"
        );
        assert!(html.contains("Next →"), "got: {html}");
        assert!(html.contains("Page 2 of 3 (42 entries)"), "got: {html}");
    }

    /// An empty href suppresses that side entirely rather than emitting a dead anchor.
    #[test]
    fn test_pager_macro_omits_absent_side() {
        let first = render_pager("", "/debug/maintenance?page=2", "Page 1 of 3");
        assert!(!first.contains("Prev"), "got: {first}");
        assert!(
            first.contains(r#"href="/debug/maintenance?page=2""#),
            "got: {first}"
        );

        let last = render_pager("/debug/maintenance?page=2", "", "Page 3 of 3");
        assert!(!last.contains("Next"), "got: {last}");
        assert!(
            last.contains(r#"href="/debug/maintenance?page=2""#),
            "got: {last}"
        );
    }

    /// memories.rs paginates by offset and must carry search/tag/include_deleted through the
    /// hop, so its hrefs contain `&`. askama escapes that to `&#38;`, which is correct inside
    /// an attribute and what the browser sends when the link is followed.
    #[test]
    fn test_pager_macro_escapes_ampersands_in_href() {
        let html = render_pager(
            "/debug/memories?offset=0&limit=20&search=foo",
            "",
            "Showing 21-40 of 42 memories",
        );
        assert!(
            html.contains(r#"href="/debug/memories?offset=0&#38;limit=20&#38;search=foo""#),
            "got: {html}"
        );
    }

    /// `.pagination` is `justify-content: space-between`, which only spreads content when the
    /// div has several direct children. Wrapping prev/summary/next in a single span gives it
    /// one flex item, `space-between` becomes a no-op, and the row silently left-aligns — a
    /// visual regression no other test would catch.
    #[test]
    fn test_pager_macro_keeps_prev_summary_next_as_siblings() {
        let html = render_pager(
            "/debug/maintenance?page=1",
            "/debug/maintenance?page=3",
            "Page 2 of 3",
        );
        // Assert against the div's *direct* children. Checking only for an inner
        // `</a> <span>…</span> <a` sequence still passes when everything is wrapped in an
        // extra span, which is the exact defect being guarded against.
        assert!(
            html.contains(r#"<div class="pagination"><a class="link""#),
            "prev link must be the div's first child, not nested in a wrapper; got: {html}"
        );
        assert!(
            html.contains("Next →</a></div>"),
            "next link must be the div's last child, not nested in a wrapper; got: {html}"
        );
    }
}
