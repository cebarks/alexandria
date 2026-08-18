/// Escape a string for safe interpolation into HTML text/attribute content.
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
</style>
</head>
<body>
<nav>
<a href="/debug">Dashboard</a>
<a href="/debug/memories">Memories</a>
<a href="/debug/clusters">Clusters</a>
<a href="/debug/query">Query Tester</a>
</nav>
<main>
{body}
</main>
</body>
</html>"#
    )
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
}
