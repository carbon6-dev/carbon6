//! An email's own colours, in the theme chosen for Mail.

/// Tags and their byte ranges, leaving comments and quoted text alone.
fn tags(html: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(at) = html[pos..].find('<') {
        let start = pos + at;
        if html[start..].starts_with("<!--") {
            pos = html[start..].find("-->").map_or(html.len(), |n| start + n + 3);
            continue;
        }
        let mut quote = None;
        pos = html.len();
        for (n, c) in html[start + 1..].char_indices() {
            match (quote, c) {
                (Some(q), c) if q == c => quote = None,
                (None, '\'' | '"') => quote = Some(c),
                (None, '>') => {
                    pos = start + n + 2;
                    out.push((start, pos));
                    let name = html[start + 1..pos - 1].split_ascii_whitespace().next().unwrap_or("").to_ascii_lowercase();
                    if matches!(name.as_str(), "style" | "script" | "textarea" | "title") {
                        pos += html[pos..].to_ascii_lowercase().find(&format!("</{name}")).unwrap_or(html.len() - pos);
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    out
}

fn attributes(tag: &str) -> Vec<(&str, &str)> {
    let mut rest = tag.trim_matches(['<', '>']).trim();
    rest = rest.find(char::is_whitespace).map_or("", |n| &rest[n..]);
    let mut out = Vec::new();
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let n = rest.find(|c: char| c.is_ascii_whitespace() || c == '=').unwrap_or(rest.len());
        let name = &rest[..n];
        rest = rest[n..].trim_start();
        let Some(value) = rest.strip_prefix('=') else { continue };
        rest = value.trim_start();
        let (value, tail) = if rest.starts_with(['\'', '"']) {
            let quote = rest.as_bytes()[0] as char;
            rest = &rest[1..];
            let n = rest.find(quote).unwrap_or(rest.len());
            (&rest[..n], rest.get(n + 1..).unwrap_or(""))
        } else {
            let n = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..n], &rest[n..])
        };
        out.push((name, value));
        rest = tail;
    }
    out
}

/// CSS tokens, without strings or comments that only describe a rule.
fn tokens(css: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < css.len() {
        let rest = &css[pos..];
        if rest.starts_with("/*") {
            pos += rest.find("*/").map_or(rest.len(), |n| n + 2);
            continue;
        }
        let c = rest.chars().next().unwrap();
        if c == '\'' || c == '"' {
            pos += 1;
            let mut escaped = false;
            for c2 in css[pos..].chars() {
                pos += c2.len_utf8();
                if c2 == c && !escaped {
                    break;
                }
                escaped = c2 == '\\' && !escaped;
            }
        } else if c.is_ascii_whitespace() {
            pos += 1;
        } else {
            let n = if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_').unwrap_or(rest.len())
            } else {
                c.len_utf8()
            };
            out.push((pos, &rest[..n]));
            pos += n;
        }
    }
    out
}

fn css_dark(css: &str) -> bool {
    let ts = tokens(css);
    ts.iter().enumerate().any(|(i, (_, word))| {
        if *word == "prefers-color-scheme" {
            return ts.get(i + 1).map(|t| t.1) == Some(":") && ts.get(i + 2).map(|t| t.1) == Some("dark");
        }
        *word == "color-scheme"
            && (i == 0 || matches!(ts[i - 1].1, "{" | ";"))
            && ts.get(i + 1).map(|t| t.1) == Some(":")
            && ts[i + 2..].iter().take_while(|t| !matches!(t.1, ";" | "}")).any(|t| t.1 == "dark")
    })
}

/// Only mail that declares dark colours may leave the white page.
pub(crate) fn supports_dark(html: &str) -> bool {
    let html = html.to_ascii_lowercase();
    for (start, end) in tags(&html) {
        let tag = &html[start..end];
        let name = tag[1..].split(|c: char| c.is_ascii_whitespace() || c == '>').next().unwrap_or("");
        let attrs = attributes(tag);
        if name == "meta"
            && attrs.iter().any(|(k, v)| *k == "name" && *v == "color-scheme")
            && attrs.iter().any(|(k, v)| *k == "content" && v.split_ascii_whitespace().any(|w| w == "dark"))
        {
            return true;
        }
        if attrs.iter().any(|(k, v)| matches!(*k, "style" | "media") && css_dark(v)) {
            return true;
        }
        if name == "style" {
            let css = &html[end..];
            if css_dark(&css[..css.find("</style").unwrap_or(css.len())]) {
                return true;
            }
        }
    }
    false
}

/// Safari versions that ignore the frame's scheme still use Mail's
/// choice for the email's media queries. Other media conditions stay.
fn media(css: &str, dark: bool) -> String {
    let lower = css.to_ascii_lowercase();
    let ts = tokens(&lower);
    let mut out = String::new();
    let mut pos = 0;
    for w in ts.windows(5) {
        if w[0].1 == "(" && w[1].1 == "prefers-color-scheme" && w[2].1 == ":" && w[4].1 == ")" && matches!(w[3].1, "dark" | "light") {
            out.push_str(&css[pos..w[0].0]);
            out.push_str(if (w[3].1 == "dark") == dark { "(min-width:0px)" } else { "(max-width:-1px)" });
            pos = w[4].0 + 1;
        }
    }
    out.push_str(&css[pos..]);
    out
}

pub(crate) fn themed(html: &str, dark: bool) -> String {
    let lower = html.to_ascii_lowercase();
    let mut edits = Vec::new();
    for (start, end) in tags(&lower) {
        let tag = &lower[start..end];
        let name = tag[1..].split(|c: char| c.is_ascii_whitespace() || c == '>').next().unwrap_or("");
        if name == "style" {
            let stop = end + lower[end..].find("</style").unwrap_or(lower.len() - end);
            edits.push((end, stop, media(&html[end..stop], dark)));
        }
        for (key, value) in attributes(tag) {
            if key == "media" {
                let at = value.as_ptr() as usize - lower.as_ptr() as usize;
                edits.push((at, at + value.len(), media(&html[at..at + value.len()], dark)));
            }
        }
    }
    let mut out = html.to_owned();
    edits.sort_by_key(|e| e.0);
    for (start, end, value) in edits.into_iter().rev() {
        out.replace_range(start..end, &value);
    }
    let scheme = if dark { "dark" } else { "light" };
    format!("{out}<style>:root {{ color-scheme: {scheme} !important; }}</style>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_is_declared() {
        for html in [
            r#"<meta name="color-scheme" content="light dark">"#,
            "<META CONTENT = 'LIGHT  DARK' NAME = 'COLOR-SCHEME'>",
            "<meta name=color-scheme content=dark>",
            "<style>:root { color-scheme: light dark; }</style>",
            "<div STYLE = 'COLOR-SCHEME : DARK !important'>hi</div>",
            "<style>@media (prefers-color-scheme: dark) { p { color: white } }</style>",
            "<STYLE>@MEDIA screen and ( PREFERS-COLOR-SCHEME : DARK ) {}</STYLE>",
            "<style>:root { color-scheme: light /* also */ dark }</style>",
        ] {
            assert!(supports_dark(html), "{html}");
        }
        for html in [
            "<p>Hello</p>",
            "<p>Let's meet after dark. color-scheme: dark</p>",
            "<meta name=color-scheme content=light>",
            "<style>:root { color-scheme: light; }</style>",
            "<!-- <meta name=color-scheme content=dark> -->",
            "<style>/* color-scheme: dark */ p:before { content: 'color-scheme: dark' }</style>",
            "<div data-note='color-scheme: dark'>hi</div>",
            "<script>const sample = '<meta name=color-scheme content=dark>';</script>",
            "<textarea><meta name=color-scheme content=dark></textarea>",
        ] {
            assert!(!supports_dark(html), "{html}");
        }
    }

    #[test]
    fn media_follows_the_ui_choice() {
        let html = "<style>@media screen and ( PREFERS-COLOR-SCHEME : DARK ) { p { color: white } } @media (prefers-color-scheme: light) { p { color: black } }</style><p>dark</p>";
        for dark in [false, true] {
            let doc = themed(html, dark);
            let (yes, no) = if dark { ("min-width:0px", "max-width:-1px") } else { ("max-width:-1px", "min-width:0px") };
            assert!(doc.contains(&format!("@media screen and ({yes}) {{ p {{ color: white }} }}")), "{doc}");
            assert!(doc.contains(&format!("@media ({no}) {{ p {{ color: black }} }}")), "{doc}");
            assert!(doc.contains(if dark { "color-scheme: dark !important" } else { "color-scheme: light !important" }));
            assert!(doc.contains("<p>dark</p>"));
        }
    }

    #[test]
    fn unquoted_media_stays_one_attribute() {
        let html = "<style media=(prefers-color-scheme:dark)>p { color: white }</style>";
        assert!(themed(html, true).starts_with("<style media=(min-width:0px)>"));
        assert!(themed(html, false).starts_with("<style media=(max-width:-1px)>"));
    }

    #[test]
    fn other_colours_and_conditions_stay() {
        let html = "<style media='screen and (prefers-color-scheme: dark)'>p { color: red } /* (prefers-color-scheme: light) */ p:before { content: '(prefers-color-scheme: light)' }</style><p>Après dark</p>";
        let doc = themed(html, true);
        assert!(doc.contains("media='screen and (min-width:0px)'"), "{doc}");
        assert!(doc.contains("p { color: red }"), "{doc}");
        assert!(doc.contains("/* (prefers-color-scheme: light) */"), "{doc}");
        assert!(doc.contains("content: '(prefers-color-scheme: light)'"), "{doc}");
        assert!(doc.contains("<p>Après dark</p>"), "{doc}");
        let plain = "<p style='color: black; background: white'>Hello</p>";
        assert!(themed(plain, false).starts_with(plain));
    }
}
