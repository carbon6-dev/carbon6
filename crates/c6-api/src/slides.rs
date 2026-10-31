//! Slides as HTML (docs/docs.md §5): carbon6 draws a deck itself from its
//! blocks and its theme, in the UI and for exports. A slide is a 1280×720
//! section; each layout places its regions; a theme sets CSS custom
//! properties over the base CSS. Markdown is rendered by the caller (the
//! UI and the head each have their renderer), passed in as `md`.

use serde::{Deserialize, Serialize};

use crate::docs::DocBlock;

/// A look for decks: CSS custom properties (`--bg`, `--fg`, `--muted`,
/// `--accent`, `--accent-bg`, `--accent-fg`, `--surface`, `--font-display`,
/// `--font-body`, `--display-weight`) on `:root`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeckTheme {
    pub name: String,
    /// What it's for, one line.
    #[serde(default)]
    pub mood: String,
    pub css: String,
}

/// The themes carbon6 ships (editable records once a realm has them).
pub fn default_themes() -> Vec<DeckTheme> {
    let t = |name: &str, mood: &str, css: &str| DeckTheme { name: name.into(), mood: mood.into(), css: css.into() };
    vec![
        t(
            "Paper",
            "Light and editorial: serif display type, warm paper, ink-blue accent. For narratives, strategy, research.",
            ":root { --bg: #faf7f2; --fg: #1d1d1f; --muted: #6b6760; --accent: #1f4e8c; --accent-bg: #1f4e8c; --accent-fg: #faf7f2; \
             --surface: #efe9df; --font-display: 'Iowan Old Style', 'Palatino Linotype', Palatino, P052, Georgia, serif; \
             --font-body: 'Avenir Next', 'Segoe UI', 'Helvetica Neue', Inter, Arial, sans-serif; --display-weight: 600; }",
        ),
        t(
            "Night",
            "Dark and high-contrast: near-black, bright text, electric accent. For product launches, keynotes, demos.",
            ":root { --bg: #0e1116; --fg: #f2f4f8; --muted: #9aa3b2; --accent: #6ee7b7; --accent-bg: #6ee7b7; --accent-fg: #0e1116; \
             --surface: #1a1f29; --font-display: 'SF Pro Display', 'Segoe UI', 'Helvetica Neue', Inter, Arial, sans-serif; \
             --font-body: 'SF Pro Text', 'Segoe UI', 'Helvetica Neue', Inter, Arial, sans-serif; --display-weight: 700; }",
        ),
        t(
            "Signal",
            "Clean and light: white, graphite text, one strong orange accent. For business reviews, data, updates.",
            ":root { --bg: #ffffff; --fg: #1f2328; --muted: #6e7781; --accent: #e8590c; --accent-bg: #1f2328; --accent-fg: #ffffff; \
             --surface: #f3f4f6; --font-display: 'Helvetica Neue', 'Segoe UI', Inter, Arial, sans-serif; \
             --font-body: 'Helvetica Neue', 'Segoe UI', Inter, Arial, sans-serif; --display-weight: 700; }",
        ),
    ]
}

/// The base CSS, every rule under `.c6deck` so a page can hold several
/// decks (each in its own `.c6deck` with its theme).
pub const BASE_CSS: &str = r#"
.c6deck .slide { width: 1280px; height: 720px; overflow: hidden; position: relative; background: var(--bg); color: var(--fg);
  font-family: var(--font-body); padding: 72px 88px; display: flex; flex-direction: column; box-sizing: border-box;
  -webkit-font-smoothing: antialiased; text-align: left; }
.c6deck .slide * { box-sizing: border-box; margin: 0; padding: 0; }
.c6deck .slide h1 { font: var(--display-weight) 72px/1.05 var(--font-display); letter-spacing: -.02em; }
.c6deck .slide h2, .c6deck .slide h3 { font: var(--display-weight) 44px/1.15 var(--font-display); letter-spacing: -.01em; max-width: 1040px; }
.c6deck .slide p, .c6deck .slide li, .c6deck .slide td, .c6deck .slide th { font-size: 26px; line-height: 1.45; }
.c6deck .slide p + p { margin-top: 16px; }
.c6deck .slide a { color: var(--accent); }
.c6deck .slide code { font-size: .85em; background: var(--surface); padding: 0 .25em; border-radius: 4px; }
.c6deck .slide pre { background: var(--surface); border-radius: 12px; padding: 20px 24px; font-size: 20px; overflow: hidden; }
.c6deck .slide ul, .c6deck .slide ol { list-style: none; display: grid; gap: 18px; }
.c6deck .slide ul + ul, .c6deck .slide ol + ol { margin-top: 18px; }
.c6deck .slide ul > li { padding-left: 32px; position: relative; }
.c6deck .slide ul > li::before { content: ""; position: absolute; left: 0; top: .6em; width: 12px; height: 12px; border-radius: 3px; background: var(--accent); }
.c6deck .slide ol { counter-reset: n; }
.c6deck .slide ol > li { padding-left: 44px; position: relative; counter-increment: n; }
.c6deck .slide ol > li::before { content: counter(n); position: absolute; left: 0; color: var(--accent); font-weight: 700; }
.c6deck .slide table { border-collapse: collapse; width: 100%; }
.c6deck .slide th { text-align: left; color: var(--muted); font-weight: 600; border-bottom: 2px solid var(--muted); padding: 8px 12px; }
.c6deck .slide td { border-bottom: 1px solid var(--surface); padding: 8px 12px; }
.c6deck .slide blockquote { font: italic 40px/1.3 var(--font-display); }
.c6deck .slide img, .c6deck .slide svg { max-width: 100%; max-height: 100%; }
.c6deck .slide .r-title { margin-bottom: 36px; }
.c6deck .slide .region { min-height: 0; }
.c6deck .slide .footer { position: absolute; left: 88px; right: 88px; bottom: 28px; display: flex; justify-content: space-between;
  font-size: 15px; color: var(--muted); }
.c6deck .slide.l-title { justify-content: flex-end; padding-bottom: 110px; }
.c6deck .slide.l-title .r-title { margin-bottom: 0; }
.c6deck .slide.l-title .r-subtitle p { font-size: 30px; color: var(--muted); margin-top: 24px; max-width: 900px; }
.c6deck .slide.l-section { justify-content: center; background: var(--accent-bg); }
.c6deck .slide.l-section h1, .c6deck .slide.l-section h2, .c6deck .slide.l-section p { color: var(--accent-fg); }
.c6deck .slide.l-section .r-title { margin-bottom: 16px; }
.c6deck .slide.l-two-columns .cols { display: grid; grid-template-columns: 1fr 1fr; gap: 56px; flex: 1; align-items: start; }
.c6deck .slide.l-big-number .r-number p, .c6deck .slide.l-big-number .r-number h1, .c6deck .slide.l-big-number .r-number h2 {
  font: var(--display-weight) 160px/1 var(--font-display); color: var(--accent); letter-spacing: -.03em; }
.c6deck .slide.l-big-number .r-caption p { color: var(--muted); margin-top: 20px; }
.c6deck .slide.l-quote { justify-content: center; }
.c6deck .slide.l-quote .r-quote p, .c6deck .slide.l-quote .r-quote blockquote { font: italic 44px/1.3 var(--font-display); max-width: 1040px; }
.c6deck .slide.l-quote .r-attribution p { font-size: 22px; color: var(--muted); margin-top: 28px; }
.c6deck .slide .r-image, .c6deck .slide .r-chart, .c6deck .slide .r-diagram { flex: 1; display: flex; align-items: center; justify-content: center; min-height: 0; }
.c6deck .slide .r-caption p { font-size: 18px; color: var(--muted); margin-top: 12px; }
.c6deck .slide.l-html { padding: 0; }
.c6deck .slide.l-html .r-html { width: 100%; height: 100%; }
.c6deck .slide .embed { border: 2px dashed var(--muted); border-radius: 12px; padding: 24px; color: var(--muted); font-size: 20px; }
.c6deck .slide .pending { border: 2px dashed var(--accent); border-radius: 16px; padding: 32px; color: var(--muted); font-size: 26px; margin: auto 0; }
"#;

/// A theme's CSS scoped to `selector` (its `:root` rules apply there).
pub fn scoped_theme(css: &str, selector: &str) -> String {
    css.replace(":root", selector)
}

/// HTML with what could run or reach out taken away: scripts, other
/// documents' frames, event handlers and `javascript:` links. An html
/// slide's own CSS stays (it is scoped to the slide by its region).
pub fn sanitize_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let blocked = ["script", "iframe", "object", "embed", "frame", "frameset", "link", "meta", "base", "form"];
    while i < html.len() {
        if html.as_bytes()[i] == b'<' {
            let rest = &lower[i + 1..];
            let closing = rest.starts_with('/');
            let name: String = rest.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if blocked.contains(&name.as_str()) {
                // Skip the element (and, for an opening tag of an element
                // with content, up to its end).
                let end_tag = format!("</{name}");
                let skip_to = if !closing && matches!(name.as_str(), "script" | "iframe" | "object" | "form" | "frameset") {
                    lower[i..].find(&end_tag).map(|j| i + j).and_then(|j| lower[j..].find('>').map(|k| j + k + 1))
                } else {
                    lower[i..].find('>').map(|j| i + j + 1)
                };
                i = skip_to.unwrap_or(html.len());
                continue;
            }
            // A tag: drop on* attributes and javascript: URLs.
            if let Some(j) = lower[i..].find('>') {
                let tag = &html[i..i + j + 1];
                out.push_str(&clean_tag(tag));
                i += j + 1;
                continue;
            }
        }
        let c = html[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn clean_tag(tag: &str) -> String {
    let mut out = String::new();
    let mut chars = tag.char_indices().peekable();
    let bytes = tag;
    while let Some((idx, c)) = chars.next() {
        // An attribute starts after whitespace.
        if c.is_whitespace() {
            let rest = &bytes[idx + 1..];
            let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ':').collect();
            let lname = name.to_ascii_lowercase();
            let after = &rest[name.len()..];
            let value_len = attr_value_len(after);
            let value = &after[..value_len].to_ascii_lowercase();
            let bad = lname.starts_with("on")
                || (matches!(lname.as_str(), "href" | "src" | "xlink:href" | "action" | "formaction")
                    && value.replace(char::is_whitespace, "").contains("javascript:"));
            if bad && !name.is_empty() {
                for _ in 0..name.len() + value_len {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// The length of `="…"`, `='…'` or `=word` at the start of `s` (0: none).
fn attr_value_len(s: &str) -> usize {
    let t = s.trim_start();
    let lead = s.len() - t.len();
    let Some(v) = t.strip_prefix('=') else { return 0 };
    let v2 = v.trim_start();
    let lead2 = v.len() - v2.len();
    let len = match v2.chars().next() {
        Some(q @ ('"' | '\'')) => v2[1..].find(q).map(|e| e + 2).unwrap_or(v2.len()),
        Some(_) => v2.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(v2.len()),
        None => 0,
    };
    lead + 1 + lead2 + len
}

/// Chips as text inside a block's Markdown (docs/docs.md §3).
pub fn chips_shown(text: &str, attrs: &serde_json::Value) -> String {
    let chips = attrs.get("chips").and_then(serde_json::Value::as_object);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("<?c6 block ") {
        let after = &rest[i + 11..];
        let Some(j) = after.find("?>") else { break };
        let key = after[..j].trim();
        out.push_str(&rest[..i]);
        let v = chips.and_then(|c| c.get(key)).cloned().unwrap_or(serde_json::Value::Null);
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        out.push_str(&match v["type"].as_str().unwrap_or("") {
            "date" => s("value"),
            "mention" => format!("@{}", if s("name").is_empty() { s("user") } else { s("name") }),
            "dropdown" => s("value"),
            "ref" => {
                if s("title").is_empty() {
                    s("ref")
                } else {
                    s("title")
                }
            }
            _ => String::new(),
        });
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

fn escape(t: &str) -> String {
    t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// One block on a slide as HTML.
fn block_html(b: &DocBlock, md: &dyn Fn(&str) -> String) -> String {
    match b.kind.as_str() {
        "pending" => format!("<div class=\"pending\">✎ {}</div>", escape(b.attrs["intent"].as_str().unwrap_or(""))),
        "embed" => format!("<div class=\"embed\">{}</div>", escape(b.attrs["type"].as_str().unwrap_or("embed"))),
        "html" => sanitize_html(&b.text),
        _ => md(&chips_shown(&b.text, &b.attrs)),
    }
}

/// A slide as a `<section class="slide …">` (`n` of `total`; the deck's
/// title goes in the footer).
pub fn slide_html(s: &DocBlock, n: usize, total: usize, deck_title: &str, md: &dyn Fn(&str) -> String) -> String {
    if s.kind == "pending" {
        return format!(
            "<section class=\"slide l-body\" data-id=\"{}\"><div class=\"pending\">✎ {}</div></section>",
            escape(&s.id),
            escape(s.attrs["intent"].as_str().unwrap_or(""))
        );
    }
    let layout = s.attrs["layout"].as_str().unwrap_or("body");
    let regions = crate::docs::regions(layout);
    let region = |name: &str| -> String {
        let inner: String = s.children.iter().filter(|c| c.region == name).map(|c| block_html(c, md)).collect();
        if inner.is_empty() {
            return String::new();
        }
        if layout == "html" {
            // Its own HTML and CSS, in a frame of its own: its CSS can't
            // reach the page or other slides, and nothing in it runs.
            let doc = format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><style>html,body{{margin:0;width:1280px;height:720px;overflow:hidden}}</style></head><body>{inner}</body></html>"
            );
            return format!(
                "<div class=\"region r-html\"><iframe sandbox=\"\" scrolling=\"no\" style=\"border:0;width:1280px;height:720px;display:block\" srcdoc=\"{}\"></iframe></div>",
                escape(&doc)
            );
        }
        format!("<div class=\"region r-{name}\">{inner}</div>")
    };
    let mut body = String::new();
    if layout == "two-columns" {
        body.push_str(&region("title"));
        body.push_str(&format!("<div class=\"cols\">{}{}</div>", region("left"), region("right")));
    } else {
        for r in regions {
            body.push_str(&region(r));
        }
    }
    // Blocks in a region the layout lacks stay visible at the end.
    let stray: String = s.children.iter().filter(|c| !regions.contains(&c.region.as_str())).map(|c| block_html(c, md)).collect();
    body.push_str(&stray);
    let footer = if matches!(layout, "title" | "section" | "html") {
        String::new()
    } else {
        format!("<div class=\"footer\"><span>{}</span><span>{n} / {total}</span></div>", escape(deck_title))
    };
    format!("<section class=\"slide l-{layout}\" data-id=\"{}\">{body}{footer}</section>", escape(&s.id))
}

/// A whole deck as one HTML file: its theme, the base CSS, and every slide
/// (hidden ones too when `hidden`), one after another (and one per page
/// when printed).
pub fn deck_html(title: &str, slides: &[DocBlock], theme_css: &str, hidden: bool, md: &dyn Fn(&str) -> String) -> String {
    let shown: Vec<&DocBlock> = slides.iter().filter(|s| hidden || s.attrs["hidden"] != true).collect();
    let total = shown.len();
    let body: String = shown.iter().enumerate().map(|(i, s)| slide_html(s, i + 1, total, title, md)).collect();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>@page {{ size: 1280px 720px; margin: 0; }} \
         html, body {{ margin: 0; background: #777; }} .c6deck .slide {{ margin: 0 auto 24px; break-after: page; }} \
         @media print {{ html, body {{ background: none; }} .c6deck .slide {{ margin: 0; }} }}{}{}</style></head>\
         <body><div class=\"c6deck\">{body}</div></body></html>",
        escape(title),
        scoped_theme(theme_css, ".c6deck"),
        BASE_CSS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b(kind: &str, region: &str, text: &str) -> DocBlock {
        DocBlock { id: "x".into(), kind: kind.into(), text: text.into(), region: region.into(), ..Default::default() }
    }

    #[test]
    fn slides_place_their_regions() {
        let md = |t: &str| format!("<p>{t}</p>");
        let s = DocBlock {
            id: "s1".into(),
            kind: "slide".into(),
            attrs: json!({"layout": "two-columns"}),
            children: vec![b("heading", "title", "## Before and after"), b("item", "left", "- slow"), b("item", "right", "- fast")],
            ..Default::default()
        };
        let h = slide_html(&s, 2, 5, "Q3 <review>", &md);
        assert!(h.starts_with("<section class=\"slide l-two-columns\" data-id=\"s1\">"), "{h}");
        assert!(h.contains("<div class=\"cols\"><div class=\"region r-left\"><p>- slow</p></div><div class=\"region r-right\">"), "{h}");
        assert!(h.contains("<span>Q3 &lt;review&gt;</span><span>2 / 5</span>"), "{h}");
        let deck = deck_html(
            "T",
            &[s.clone(), DocBlock { attrs: json!({"layout": "body", "hidden": true}), ..s }],
            ":root { --bg: red; }",
            false,
            &md,
        );
        assert!(deck.contains(".c6deck { --bg: red; }") && deck.matches("<section").count() == 1, "hidden slides are left out");
    }

    #[test]
    fn html_slides_lose_what_could_run() {
        let dirty = "<div onclick=\"x()\" class=a><script>alert(1)</script><a href=\"javascript:alert(1)\">x</a>\
                     <iframe src=\"https://e\"></iframe><img src=x onerror=alert(1)><svg><rect width=\"10\"/></svg></div>";
        let clean = sanitize_html(dirty);
        assert!(!clean.to_lowercase().contains("script") && !clean.contains("onclick") && !clean.contains("onerror"), "{clean}");
        assert!(!clean.contains("iframe") && !clean.contains("javascript:"), "{clean}");
        assert!(clean.contains("<div class=a>") && clean.contains("<rect width=\"10\"/>") && clean.contains("<img src=x>"), "{clean}");
    }

    #[test]
    fn html_slides_draw_in_a_frame_of_their_own() {
        let s = DocBlock {
            id: "h".into(),
            kind: "slide".into(),
            attrs: json!({"layout": "html"}),
            children: vec![b(
                "html",
                "html",
                "<style>.slide{background:red}</style><section class=\"slide\">x</section><script>1</script>",
            )],
            ..Default::default()
        };
        let h = slide_html(&s, 1, 1, "T", &|t| t.to_owned());
        assert!(h.contains("<iframe sandbox=\"\""), "{h}");
        assert!(h.contains("srcdoc=\"&lt;!doctype html&gt;") && h.contains("&lt;style&gt;.slide{background:red}"), "{h}");
        assert!(!h.contains("script"), "{h}");
    }

    #[test]
    fn chips_read_as_text() {
        let attrs = json!({"chips": {"d": {"type": "date", "value": "2026-11-01"}, "m": {"type": "mention", "name": "Ana"}}});
        assert_eq!(chips_shown("Due <?c6 block d?> with <?c6 block m?>.", &attrs), "Due 2026-11-01 with @Ana.");
    }
}
