//! HTML responses for the debug UI.
//!
//! Every page is an askama template under `crates/alexandria-mcp/templates/`, rendered
//! through [`page()`]. askama auto-escapes `{{ }}` at compile time, so escaping is
//! structural rather than a per-call-site habit — there is deliberately no escape helper
//! here to reach for. Values interpolated into a URL rather than into HTML text need the
//! right *context* escape (`|urlencode`), which templates apply explicitly.

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

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

#[cfg(test)]
mod tests {
    use super::*;

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
