//! Markdown rendering for model output. Raw HTML in the source is shown as
//! text, never interpreted, and links and images only go to web, mail and
//! same-site addresses (no `javascript:` or `data:` links). Links to
//! carbon6's own things (sessions, documents, work, projects) show as
//! mentions: the thing's mark and title on a tint of its colour.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// A carbon6 thing a message can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Session,
    Doc,
    Work,
    Project,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Session, Kind::Doc, Kind::Work, Kind::Project];

    /// Its name in a reference (`session:<id>`).
    pub fn name(self) -> &'static str {
        match self {
            Kind::Session => "session",
            Kind::Doc => "doc",
            Kind::Work => "work",
            Kind::Project => "project",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// The first part of its path in the app (`/sessions/<id>`).
    fn segment(self) -> &'static str {
        match self {
            Kind::Session => "sessions",
            Kind::Doc => "docs",
            Kind::Work => "work",
            Kind::Project => "projects",
        }
    }

    /// What it's called, when its title isn't known.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Session => "session",
            Kind::Doc => "document",
            Kind::Work => "work",
            Kind::Project => "project",
        }
    }

    /// Its path in the app, within the realm.
    pub fn path(self, id: &str) -> String {
        format!("/{}/{id}", self.segment())
    }

    /// Its name when its title isn't known: "session 01a18f50".
    pub fn short(self, id: &str) -> String {
        format!("{} {}", self.word(), id.chars().take(8).collect::<String>())
    }
}

/// What a mention shows of its thing: its title and mark, as last seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Known {
    pub title: Option<String>,
    pub mark: Option<String>,
}

/// Where the page is, to tell its own links from others: the head's
/// origin (`https://head.example`), the page's realm and the start of its
/// addresses (`/r/home`); and what is known of the things named.
pub struct Here<'a> {
    pub origin: String,
    pub realm: Option<String>,
    pub base: String,
    pub known: &'a dyn Fn(Kind, &str) -> Known,
}

/// What a document's mention is tinted with (documents have no mark).
pub const DOC_COLOUR: &str = "#0ea5e9";

pub fn to_html(src: &str) -> String {
    // Titles and marks as the message renders, untracked: a message isn't
    // drawn again for every change to a session (its hover card shows
    // those). Outside the app (no live feed) nothing is known.
    let live = leptos::prelude::use_context::<crate::api::LiveFeed>();
    let origin = live.and_then(|_| leptos::prelude::window().location().origin().ok()).unwrap_or_default();
    let here = Here { origin, realm: crate::place::realm(), base: crate::place::base(), known: &crate::mentions::known };
    render(src, &here)
}

/// Markdown as HTML, with its links to carbon6 things (`session:<id>`,
/// `/docs/<id>`, a bare `work:<uuid>`…) as mentions. None in code or in
/// another link.
pub fn render(src: &str, here: &Here) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Raw HTML first, as text: the mentions are the only HTML let through.
    let events = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        other => other,
    });
    let mut out: Vec<Event> = Vec::new();
    // Inside a code block, an image or a web link: no mentions there.
    let mut plain = 0i32;
    // A mention's link being read: its thing, and the text it was given.
    let mut reading: Option<(Kind, String, String, Vec<Event>)> = None;
    for ev in events {
        if let Some((kind, id, rest, inner)) = &mut reading {
            if let Event::End(TagEnd::Link) = ev {
                let given = given_text(inner, &[&format!("{}:{id}", kind.name()), id.as_str()]);
                out.push(Event::InlineHtml(CowStr::from(mention(*kind, id, rest, given, here))));
                reading = None;
            } else {
                inner.push(sanitize(ev));
            }
            continue;
        }
        match &ev {
            Event::Start(Tag::Link { dest_url, .. }) if plain == 0 => {
                if let Some((kind, id, rest)) = thing_of(dest_url, here) {
                    reading = Some((kind, id, rest, Vec::new()));
                    continue;
                }
                plain += 1;
            }
            Event::Start(Tag::Link { .. } | Tag::CodeBlock(_) | Tag::Image { .. }) => plain += 1,
            Event::End(TagEnd::Link | TagEnd::CodeBlock | TagEnd::Image) => plain -= 1,
            Event::Text(t) if plain == 0 && Kind::ALL.iter().any(|k| t.contains(&format!("{}:", k.name()))) => {
                out.extend(bare_mentions(t, here));
                continue;
            }
            // A reference alone in backticks, as models write them.
            Event::Code(c) if plain == 0 => {
                if let Some((kind, id)) = bare_ref(c).filter(|(k, id)| c.len() == k.name().len() + 1 + id.len()) {
                    out.push(Event::InlineHtml(CowStr::from(mention(kind, &id, "", None, here))));
                    continue;
                }
            }
            _ => {}
        }
        out.push(sanitize(ev));
    }
    let mut html_out = String::new();
    html::push_html(&mut html_out, out.into_iter());
    html_out
}

/// Links and images only to safe addresses.
fn sanitize(ev: Event) -> Event {
    match ev {
        Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Link { link_type, dest_url: safe_url(dest_url), title, id })
        }
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Image { link_type, dest_url: safe_url(dest_url), title, id })
        }
        other => other,
    }
}

/// The carbon6 thing a link goes to, and what follows its path (a query,
/// an anchor): `session:<id>` (`doc:`, `work:`, `project:`), or
/// `/sessions/<id>` (`/docs/`, `/work/`, `/projects/`) with or without
/// `/r/<realm>` or the head's origin in front. A link into another realm
/// than the page's stays a link.
pub fn thing_of(href: &str, here: &Here) -> Option<(Kind, String, String)> {
    let href = href.trim();
    if href.split_once(':').is_some_and(|(k, _)| Kind::parse(k).is_some()) {
        return reference(href).map(|(kind, id)| (kind, id, String::new()));
    }
    let mut path = href;
    if !here.origin.is_empty()
        && let Some(rest) = path.strip_prefix(here.origin.as_str())
        && rest.starts_with('/')
    {
        path = rest;
    }
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    if let Some(rest) = path.strip_prefix("/r/") {
        let (realm, rest) = rest.split_once('/')?;
        if here.realm.as_deref().is_some_and(|r| r != realm) {
            return None;
        }
        path = rest;
    }
    let (path, rest) = path.split_at(path.find(['?', '#']).unwrap_or(path.len()));
    let mut parts = path.trim_matches('/').split('/');
    let (Some(segment), Some(id), None) = (parts.next(), parts.next(), parts.next()) else { return None };
    let kind = Kind::ALL.into_iter().find(|k| k.segment() == segment)?;
    valid_id(id).then(|| (kind, id.to_owned(), rest.to_owned()))
}

/// A reference (`session:<id>`, `doc:`, `work:`, `project:`) as its kind
/// and id.
pub fn reference(r: &str) -> Option<(Kind, String)> {
    let (name, id) = r.split_once(':')?;
    let kind = Kind::parse(name)?;
    valid_id(id).then(|| (kind, id.to_owned()))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A reference by uuid at the start of `t` (`doc:01a18f50-…`).
fn bare_ref(t: &str) -> Option<(Kind, String)> {
    let (name, rest) = t.split_once(':')?;
    let kind = Kind::parse(name)?;
    let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit() || *c == '-').collect();
    (id.len() == 36 && id.matches('-').count() == 4).then_some((kind, id))
}

/// Text with each `session:<uuid>` (`doc:`, `work:`, `project:`) in it as
/// a mention.
fn bare_mentions(t: &str, here: &Here) -> Vec<Event<'static>> {
    let mut out = Vec::new();
    let (mut from, mut i) = (0, 0);
    while i < t.len() {
        // At a word's start: not in `subsession:…` or a path.
        let starts_word = t[..i].chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || "_-/:".contains(c)));
        if starts_word
            && Kind::ALL.iter().any(|k| t[i..].starts_with(k.name()))
            && let Some((kind, id)) = bare_ref(&t[i..])
        {
            let end = i + kind.name().len() + 1 + id.len();
            if t[end..].chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_')) {
                if from < i {
                    out.push(Event::Text(CowStr::from(t[from..i].to_owned())));
                }
                out.push(Event::InlineHtml(CowStr::from(mention(kind, &id, "", None, here))));
                (from, i) = (end, end);
                continue;
            }
        }
        i += t[i..].chars().next().map_or(1, char::len_utf8);
    }
    if from < t.len() {
        out.push(Event::Text(CowStr::from(t[from..].to_owned())));
    }
    out
}

/// A link's text as HTML, unless it says no more than where it goes (an
/// autolink, `[session:x](session:x)`): then the mention shows the title.
fn given_text(inner: &[Event], bare: &[&str]) -> Option<String> {
    let plain: String = inner
        .iter()
        .map(|e| match e {
            Event::Text(t) | Event::Code(t) => t.as_ref(),
            _ => "",
        })
        .collect();
    let plain = plain.trim();
    if plain.is_empty() || bare.contains(&plain) {
        return None;
    }
    let mut out = String::new();
    html::push_html(&mut out, inner.iter().cloned());
    Some(out)
}

/// A mention: the thing's mark (a document's drawn file icon), then the text given or
/// its title, on a tint of its colour. `data-ref` says what it is, for the
/// click that opens it and the card hovering shows (`mentions.rs`).
fn mention(kind: Kind, id: &str, rest: &str, given: Option<String>, here: &Here) -> String {
    let known = (here.known)(kind, id);
    let label = given.unwrap_or_else(|| esc(&known.title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| kind.short(id))));
    let (colour, glyph) = match kind {
        Kind::Doc => (DOC_COLOUR, format!("<span class=\"c6-mention-doc\">{}</span>", crate::icons::html::file())),
        _ => {
            let mark = known.mark.unwrap_or_else(|| c6_api::marks::hashed(id).to_owned());
            let shape = crate::marks::Shape::of(kind.name()).css(7);
            (
                crate::marks::colour_hex(&mark),
                format!("<span class=\"c6-mention-dot\" style=\"{shape};{}\"></span>", esc(&crate::marks::fill(&mark))),
            )
        }
    };
    format!(
        "<a href=\"{}\" data-ref=\"{}:{}\" class=\"c6-mention\" style=\"--m:{colour}\">{glyph}{label}</a>",
        esc(&format!("{}{}{rest}", here.base, kind.path(id))),
        kind.name(),
        esc(id),
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
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

    const ID: &str = "01a18f50-9f48-7567-8070-43e1f17ba642";

    fn nothing(_: Kind, _: &str) -> Known {
        Known::default()
    }

    #[test]
    fn document_mentions_keep_a_drawn_file_before_the_given_label() {
        let html = render(&format!("[the plan](doc:{ID})"), &here(&nothing));
        assert!(html.find("<svg").expect("keep the document icon") < html.find("the plan").unwrap(), "{html}");
        assert!(!html.contains(">Doc "), "don't replace the icon with a word: {html}");
        crate::icons::tests::assert_no_icon_glyphs(&html);
    }

    fn here(known: &dyn Fn(Kind, &str) -> Known) -> Here<'_> {
        Here { origin: "https://head.example".into(), realm: Some("home".into()), base: "/r/home".into(), known }
    }

    /// The mentions in `html`: (data-ref, href, text without tags).
    fn mentions(html: &str) -> Vec<(String, String, String)> {
        html.split("<a href=\"")
            .skip(1)
            .filter(|a| a.contains("class=\"c6-mention\""))
            .map(|a| {
                let href = a.split('"').next().unwrap().to_owned();
                let data = a.split("data-ref=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
                let body = &a[a.find('>').unwrap() + 1..a.find("</a>").unwrap()];
                let mut text = String::new();
                let mut in_tag = false;
                for c in body.chars() {
                    match c {
                        '<' => in_tag = true,
                        '>' => in_tag = false,
                        c if !in_tag => text.push(c),
                        _ => {}
                    }
                }
                (data, href, text)
            })
            .collect()
    }

    #[test]
    fn escapes_raw_html() {
        let out = to_html("hi <script>alert(1)</script> **b**");
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("<strong>b</strong>"));
    }

    #[test]
    fn every_link_form_is_a_mention() {
        let h = here(&nothing);
        for (kind, segment) in [("session", "sessions"), ("doc", "docs"), ("work", "work"), ("project", "projects")] {
            for href in [
                format!("{kind}:{ID}"),
                format!("/{segment}/{ID}"),
                format!("/r/home/{segment}/{ID}"),
                format!("https://head.example/r/home/{segment}/{ID}"),
                format!("https://head.example/{segment}/{ID}"),
            ] {
                let out = render(&format!("See [the plan]({href}) now."), &h);
                let text = "the plan";
                assert_eq!(
                    mentions(&out),
                    vec![(format!("{kind}:{ID}"), format!("/r/home/{segment}/{ID}"), text.to_owned())],
                    "{href}: {out}"
                );
                assert!(out.contains("</a> now."), "{out}");
                assert!(!out.contains("href=\"#\""), "{href}: {out}");
            }
        }
    }

    #[test]
    fn text_and_backticked_references_are_mentions() {
        let h = here(&nothing);
        for kind in ["session", "doc", "work", "project"] {
            let out = render(&format!("Started {kind}:{ID}, and `{kind}:{ID}`."), &h);
            let found = mentions(&out);
            assert_eq!(found.len(), 2, "{out}");
            for (data, href, _) in &found {
                assert_eq!(data, &format!("{kind}:{ID}"));
                assert!(href.starts_with("/r/home/") && href.ends_with(ID), "{href}");
            }
            assert!(out.contains("Started <a ") && out.contains("</a>, and <a "), "{out}");
            assert!(!out.contains("<code>"), "{out}");
        }
        // Not a reference: too short, inside a longer word or a path, or
        // more than the reference in backticks.
        for text in ["doc:short", &format!("subsession:{ID}"), &format!("/x/doc:{ID}"), &format!("`see doc:{ID}`"), "session: later"] {
            let out = render(text, &h);
            assert!(mentions(&out).is_empty(), "{text}: {out}");
        }
    }

    #[test]
    fn a_mention_shows_its_title_unless_it_was_given_text() {
        let known = |k: Kind, id: &str| Known { title: Some(format!("<{}> {}", k.name(), &id[..4])), mark: Some("rose/dots".into()) };
        let h = here(&known);
        let out = render(&format!("[session:{ID}](session:{ID}) <session:{ID}> [**Fix it**](/sessions/{ID})"), &h);
        let found = mentions(&out);
        let titled = "&lt;session&gt; 01a1";
        assert_eq!(found.iter().map(|m| m.2.as_str()).collect::<Vec<_>>(), [titled, titled, "Fix it"], "titles, escaped: {out}");
        assert!(out.contains("<strong>Fix it</strong>"), "{out}");
        assert!(out.contains(&format!("--m:{}", crate::marks::colour_hex("rose"))), "its mark's colour: {out}");
        // Nothing known: its kind and the id's start.
        let out = render(&format!("work:{ID} [`doc:{ID}`](doc:{ID})"), &here(&nothing));
        assert_eq!(mentions(&out).iter().map(|m| m.2.as_str()).collect::<Vec<_>>(), ["work 01a18f50", "document 01a18f50"], "{out}");
    }

    #[test]
    fn web_links_and_code_stay_as_they_are() {
        let h = here(&nothing);
        let out = render("[docs](https://example.com/docs/x) and [elsewhere](https://other.example/sessions/abc)", &h);
        assert!(out.contains("<a href=\"https://example.com/docs/x\">docs</a>"), "{out}");
        assert!(out.contains("<a href=\"https://other.example/sessions/abc\">elsewhere</a>"), "{out}");
        assert!(mentions(&out).is_empty(), "{out}");
        // Another realm's is a link there; other paths of the app stay links.
        let out = render(&format!("[x](/r/work/sessions/{ID}) [y](/settings/models) [z](/sessions/{ID}/files)"), &h);
        assert!(mentions(&out).is_empty(), "{out}");
        assert!(out.contains(&format!("href=\"/r/work/sessions/{ID}\"")), "{out}");
        let fenced = render(&format!("```\nsession:{ID}\n[a](session:{ID})\n```\n\n    doc:{ID}"), &h);
        assert!(!fenced.contains("<a "), "{fenced}");
        // A reference inside a web link's text stays its text.
        let out = render(&format!("[see doc:{ID}](https://example.com)"), &h);
        assert!(mentions(&out).is_empty() && out.contains(&format!(">see doc:{ID}</a>")), "{out}");
    }

    #[test]
    fn a_session_link_keeps_its_place() {
        let out = render(&format!("[there](/r/home/sessions/{ID}?q=error#ev-12)"), &here(&nothing));
        assert_eq!(mentions(&out)[0].1, format!("/r/home/sessions/{ID}?q=error#ev-12"), "{out}");
    }

    #[test]
    fn only_safe_link_schemes() {
        for bad in ["javascript:alert(1)", "JavaScript:alert(1)", "java%09script:x", "data:text/html,x", "vbscript:x"] {
            let out = to_html(&format!("[x]({bad}) ![i]({bad})"));
            assert!(!out.to_ascii_lowercase().contains("script:") && !out.contains("data:"), "{bad}: {out}");
        }
        let out = to_html("[x](<java\tscript:alert(1)>)");
        assert!(!out.contains("script:"), "{out}");
        // Not in a mention's text either.
        let out = to_html(&format!("[![i](javascript:x)](session:{ID})"));
        assert!(!out.contains("script:"), "{out}");
        for good in ["https://example.com/a?b=c:d", "http://x", "mailto:a@b.c", "/sessions/1", "#top", "docs/a:b.md", "?q=a:b"] {
            assert!(to_html(&format!("[x]({good})")).contains(&format!("href=\"{}\"", good.replace('&', "&amp;"))), "{good}");
        }
    }
}
