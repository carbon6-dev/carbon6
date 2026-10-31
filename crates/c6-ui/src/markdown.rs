//! Markdown rendering for model output. Raw HTML in the source is shown as
//! text, never interpreted, and links and images only go to web, mail and
//! same-site addresses (no `javascript:` or `data:` links).

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub fn to_html(src: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // `doc:<id>` in text links to that document (docs/docs.md §12), not
    // inside code or another link.
    let mut plain = 0i32;
    let events = Parser::new_ext(src, opts).flat_map(move |ev| {
        match &ev {
            Event::Start(Tag::Link { .. } | Tag::CodeBlock(_)) => plain += 1,
            Event::End(pulldown_cmark::TagEnd::Link | pulldown_cmark::TagEnd::CodeBlock) => plain -= 1,
            Event::Text(t) if plain == 0 && t.contains("doc:") => return doc_links(t),
            // `doc:<id>` alone in backticks, as models write refs, links too.
            Event::Code(c) if plain == 0 && c.starts_with("doc:") && c.len() == 40 && doc_links(c).len() == 4 => {
                let mut l = doc_links(c);
                l[2] = Event::Code(c.clone());
                return l;
            }
            _ => {}
        }
        vec![ev]
    });
    let events = events.map(|ev| match ev {
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

/// Text with each `doc:<uuid>` as a link to `/docs/<uuid>`.
fn doc_links(t: &str) -> Vec<Event<'static>> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(i) = rest.find("doc:") {
        let id: String = rest[i + 4..].chars().take_while(|c| c.is_ascii_hexdigit() || *c == '-').collect();
        if id.len() != 36 {
            out.push(Event::Text(CowStr::from(rest[..i + 4].to_owned())));
            rest = &rest[i + 4..];
            continue;
        }
        out.push(Event::Text(CowStr::from(rest[..i].to_owned())));
        let tag = Tag::Link {
            link_type: pulldown_cmark::LinkType::Inline,
            dest_url: CowStr::from(format!("/docs/{id}")),
            title: CowStr::from("Open the document"),
            id: CowStr::from(""),
        };
        out.push(Event::Start(tag));
        out.push(Event::Text(CowStr::from(format!("doc:{}", &id[..8]))));
        out.push(Event::End(pulldown_cmark::TagEnd::Link));
        rest = &rest[i + 4 + 36..];
    }
    if !rest.is_empty() {
        out.push(Event::Text(CowStr::from(rest.to_owned())));
    }
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
    fn doc_refs_link_to_documents() {
        let out = to_html("See doc:01a18f50-9f48-7567-8070-43e1f17ba642 now. `doc:01a18f50-9f48-7567-8070-43e1f17ba642` doc:short");
        assert!(
            out.contains("<a href=\"/docs/01a18f50-9f48-7567-8070-43e1f17ba642\" title=\"Open the document\">doc:01a18f50</a> now."),
            "{out}"
        );
        assert!(out.contains("<a href=\"/docs/01a18f50-9f48-7567-8070-43e1f17ba642\" title=\"Open the document\"><code>doc:01a18f50-9f48-7567-8070-43e1f17ba642</code></a>"), "{out}");
        assert!(out.contains("doc:short"), "{out}");
        let fenced = to_html("```\ndoc:01a18f50-9f48-7567-8070-43e1f17ba642\n```");
        assert!(!fenced.contains("<a "), "{fenced}");
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
