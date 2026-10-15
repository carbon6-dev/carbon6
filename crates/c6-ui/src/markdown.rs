//! Markdown rendering for model output. Raw HTML in the source is shown as
//! text, never interpreted, and links and images only go to web, mail and
//! same-site addresses (no `javascript:` or `data:` links).

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub fn to_html(src: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Link { link_type, dest_url: safe_url(dest_url), title, id })
        }
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Image { link_type, dest_url: safe_url(dest_url), title, id })
        }
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// `url` for an `href` from outside data (a calendar, a web page): itself
/// if it is http(s), mailto or relative, otherwise `#`.
pub fn safe_href(url: &str) -> String {
    safe_url(CowStr::Borrowed(url)).into_string()
}

/// `url` if it is http(s), mailto or has no scheme (relative, `#anchor`);
/// otherwise `#`.
fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    // Browsers ignore ASCII whitespace and control characters in a scheme
    // ("java\tscript:"), so look past them.
    let plain: String = url.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control()).collect();
    let scheme = plain.split_once(':').map(|(s, _)| s).filter(|s| !s.contains(['/', '?', '#']));
    match scheme.map(str::to_ascii_lowercase).as_deref() {
        None | Some("http" | "https" | "mailto") => url,
        Some(_) => CowStr::Borrowed("#"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_raw_html() {
        let out = to_html("hi <script>alert(1)</script> **b**");
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("<strong>b</strong>"));
    }

    #[test]
    fn only_safe_link_schemes() {
        for bad in ["javascript:alert(1)", "JavaScript:alert(1)", "java%09script:x", "data:text/html,x", "vbscript:x"] {
            let out = to_html(&format!("[x]({bad}) ![i]({bad})"));
            assert!(!out.to_ascii_lowercase().contains("script:") && !out.contains("data:"), "{bad}: {out}");
        }
        let out = to_html("[x](<java\tscript:alert(1)>)");
        assert!(!out.contains("script:"), "{out}");
        for good in ["https://example.com/a?b=c:d", "http://x", "mailto:a@b.c", "/sessions/1", "#top", "docs/a:b.md", "?q=a:b"] {
            assert!(to_html(&format!("[x]({good})")).contains(&format!("href=\"{}\"", good.replace('&', "&amp;"))), "{good}");
        }
    }
}
