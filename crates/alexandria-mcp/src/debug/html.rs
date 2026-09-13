//! HTML responses for the debug UI.
//!
//! Two paths coexist here while the migration is in flight. `esc()` and `layout()` are
//! the legacy hand-built one: markup assembled with `format!`, where escaping is a
//! per-call-site habit and therefore easy to forget. The templates under
//! `crates/alexandria-mcp/templates/` rendered through [`page()`] are the replacement:
//! askama auto-escapes `{{ }}` at compile time, so escaping is structural. New code
//! must use templates + `page()`; the legacy path stays only because the six existing
//! pages still call `layout()` (removed in Task 1.5).

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// Escape a string for safe interpolation into HTML text/attribute content.
///
/// Legacy: prefer a template, where escaping is automatic. Still needed by the
/// `format!`-built pages until they are migrated.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Wrap a body fragment in the shared page layout (nav + htmx script + minimal CSS).
pub fn layout(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} — Alexandria Debug</title>
<script src="https://unpkg.com/htmx.org@1.9.12"></script>
<style>
body {{ font-family: system-ui, sans-serif; margin: 0; padding: 0; background: #0d1117; color: #c9d1d9; }}
nav {{ background: #161b22; padding: 12px 24px; border-bottom: 1px solid #30363d; }}
nav a {{ color: #58a6ff; margin-right: 16px; text-decoration: none; }}
main {{ padding: 24px; max-width: 1100px; margin: 0 auto; }}
table {{ width: 100%; border-collapse: collapse; margin-top: 12px; }}
th, td {{ text-align: left; padding: 8px; border-bottom: 1px solid #30363d; }}
th {{ color: #8b949e; font-weight: 600; }}
input, select, textarea, button {{ background: #0d1117; color: #c9d1d9; border: 1px solid #30363d; padding: 6px 8px; border-radius: 4px; }}
button {{ cursor: pointer; }}
.badge {{ display: inline-block; background: #21262d; padding: 2px 8px; border-radius: 12px; font-size: 12px; margin-right: 4px; }}
a.link {{ color: #58a6ff; }}
.error {{ color: #f85149; }}
tr.deleted td {{ opacity: 0.45; text-decoration: line-through; }}
tr.deleted td:first-child {{ text-decoration: none; }}
.pagination {{ display: flex; justify-content: space-between; align-items: center; margin-top: 12px; color: #8b949e; font-size: 14px; }}
.pagination a {{ margin: 0 4px; }}
.badge-deleted {{ background: #3d1a1a; color: #f85149; border: 1px solid #6e2020; padding: 2px 10px; border-radius: 12px; font-size: 13px; margin-left: 8px; }}
pre.content-block {{ background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 16px; white-space: pre-wrap; word-break: break-word; max-height: 400px; overflow-y: auto; }}
dl.fact-meta {{ display: grid; grid-template-columns: 160px 1fr; gap: 6px 16px; margin: 12px 0; }}
dl.fact-meta dt {{ color: #8b949e; font-weight: 600; }}
dl.fact-meta dd {{ margin: 0; }}
</style>
</head>
<body>
<nav>
<a href="/debug">Dashboard</a>
<a href="/debug/memories">Memories</a>
<a href="/debug/clusters">Clusters</a>
<a href="/debug/maintenance">Maintenance Log</a>
<a href="/debug/query">Query Tester</a>
</nav>
<main>
{body}
</main>
</body>
</html>"#
    )
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

/// Data-layer failure (DB unreachable, bad record id) rendered through the layout.
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

/// Test-only caller for `templates/_pagination.html`: askama compiles a template only
/// when something derives from it, so this is what keeps the `pager` macro checked.
/// Delete alongside `_test_pager.html` once Task 1.4 wires the macro into a real page.
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

    #[test]
    fn test_esc_escapes_all_special_chars() {
        let input = r#"<script>alert("x")&'y'</script>"#;
        let out = esc(input);
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&quot;x&quot;"));
        assert!(out.contains("&#39;y&#39;"));
    }

    #[test]
    fn test_layout_includes_title_and_body() {
        let html = layout("Test Page", "<p>hello</p>");
        assert!(html.contains("Test Page"));
        assert!(html.contains("<p>hello</p>"));
        assert!(html.contains("htmx.org"));
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
        // askama 0.16 writes character references numerically (`&#60;`, not the `&lt;` that
        // legacy `esc()` emits). Equivalent to a browser, different bytes — so assert the
        // whole escaped paragraph, which fails if escaping is ever weakened in either form.
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
