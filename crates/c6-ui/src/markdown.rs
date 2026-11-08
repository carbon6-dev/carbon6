//! Markdown rendering for model output. A small, balanced HTML allowlist is
//! rebuilt without attributes; other raw HTML stays visible as text.
//! Links and images only go to web, mail and
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
    with_citations(src, &[])
}

/// Plain text with carbon6 references as the usual mentions, not Markdown.
/// Card-button messages can name a new session without interpreting the
/// task's title as formatting or HTML.
pub fn text_with_mentions(src: &str) -> String {
    let here = Here { origin: String::new(), realm: crate::place::realm(), base: crate::place::base(), known: &crate::mentions::known };
    let mut out = String::new();
    html::push_html(&mut out, bare_mentions(src, &here).into_iter());
    out
}

pub fn with_citations(src: &str, citations: &[c6_api::citations::UrlCitation]) -> String {
    // Titles and marks as the message renders, untracked: a message isn't
    // drawn again for every change to a session (its hover card shows
    // those). Outside the app (no live feed) nothing is known.
    let live = leptos::prelude::use_context::<crate::api::LiveFeed>();
    let origin = live.and_then(|_| leptos::prelude::window().location().origin().ok()).unwrap_or_default();
    let here = Here { origin, realm: crate::place::realm(), base: crate::place::base(), known: &crate::mentions::known };
    if citations.is_empty() { render(src, &here) } else { render_citations(src, citations, &here) }
}

/// Markdown as HTML, with its links to carbon6 things (`session:<id>`,
/// `/docs/<id>`, a bare `work:<uuid>`…) as mentions. None in code or in
/// another link.
pub fn render(src: &str, here: &Here) -> String {
    render_citations(src, &[], here)
}

fn render_citations(src: &str, citations: &[c6_api::citations::UrlCitation], here: &Here) -> String {
    let src = c6_api::citations::markdown(src, citations);
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Sanitize model HTML before adding our own trusted mentions/citations.
    let events = allowed_html(Parser::new_ext(&src, opts)).into_iter();
    // String markup only: Leptos views cannot be rendered to strings in wasm.
    // Reference links we just generated get a small superscript, not raw HTML
    // from the model. All URLs still pass through the normal sanitizer.
    let mut links = Vec::new();
    let events = events.flat_map(|ev| match &ev {
        Event::Start(Tag::Link { id, .. }) => {
            let citation = !citations.is_empty() && c6_api::citations::reference(id);
            links.push(citation);
            if citation { vec![Event::InlineHtml("<sup>".into()), ev] } else { vec![ev] }
        }
        Event::End(TagEnd::Link) if links.pop().unwrap_or(false) => vec![ev, Event::InlineHtml("</sup>".into())],
        _ => vec![ev],
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

/// Raw tags begin as text. Only a complete, properly nested tree is promoted
/// to rebuilt HTML, so streamed/unclosed input cannot leak into another cell,
/// paragraph or view. No source bytes ever become an HTML event.
fn allowed_html<'a>(events: impl Iterator<Item = Event<'a>>) -> Vec<Event<'a>> {
    let mut events = events.peekable();
    let mut out = Vec::new();
    let mut stack: Vec<(&str, usize)> = Vec::new();
    let mut pending = Vec::new();
    let mut literal: Option<(&str, usize)> = None;
    let mut depth = 0usize;
    let mut phrasing = 0usize;
    while let Some(ev) = events.next() {
        match &ev {
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. } | Tag::Heading { .. }) => {
                phrasing += 1
            }
            Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link | TagEnd::Image | TagEnd::Heading(_)) => {
                phrasing -= 1
            }
            _ => {}
        }
        match ev {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                if literal.is_some_and(|(_, at)| at >= depth) {
                    literal = None;
                }
                if stack.last().is_some_and(|(_, at)| *at >= depth) {
                    stack.clear();
                    pending.clear();
                }
                depth -= 1;
            }
            _ => {}
        }
        if let Event::Html(raw) | Event::InlineHtml(raw) = ev {
            // Block HTML is emitted one line at a time, including tags and
            // comments whose attributes/content span several lines.
            let mut raw = raw.into_string();
            while matches!(events.peek(), Some(Event::Html(_) | Event::InlineHtml(_))) {
                if let Some(Event::Html(next) | Event::InlineHtml(next)) = events.next() {
                    raw.push_str(&next);
                }
            }
            let mut rest = raw.as_str();
            while !rest.is_empty() {
                // In raw text, '<', quotes and comments are body bytes. Find
                // only its closing delimiter, independently of tag syntax.
                if let Some((tag, _)) = literal {
                    let end = rest.char_indices().find_map(|(i, c)| {
                        if c != '<' {
                            return None;
                        }
                        let after = rest[i..].strip_prefix("</")?;
                        if !after.get(..tag.len())?.eq_ignore_ascii_case(tag) {
                            return None;
                        }
                        let tail = &after[tag.len()..];
                        let trimmed = tail.trim_start_matches([' ', '\t', '\n', '\r', '\u{c}']);
                        trimmed.starts_with('>').then_some(i + 2 + tag.len() + tail.len() - trimmed.len() + 1)
                    });
                    let (text, tail) = rest.split_at(end.unwrap_or(rest.len()));
                    out.push(Event::Text(text.to_owned().into()));
                    rest = tail;
                    if end.is_some() {
                        literal = None;
                    }
                    continue;
                }
                // HTML blocks contain entire lines; inline events usually
                // contain one tag. Keep comments opaque, including inner '<'.
                let end = if let Some(comment) = rest.strip_prefix("<!--") {
                    comment.find("-->").map_or(rest.len(), |i| i + 7)
                } else if rest.starts_with('<') {
                    let mut quote = None;
                    rest.char_indices()
                        .find_map(|(i, c)| match c {
                            '\'' | '"' if quote == Some(c) => {
                                quote = None;
                                None
                            }
                            '\'' | '"' if quote.is_none() => {
                                quote = Some(c);
                                None
                            }
                            '>' if quote.is_none() => Some(i + 1),
                            _ => None,
                        })
                        .unwrap_or(rest.len())
                } else {
                    rest.find('<').unwrap_or(rest.len())
                };
                let (text, tail) = rest.split_at(end);
                rest = tail;
                let index = out.len();
                out.push(Event::Text(text.to_owned().into()));
                // Rejected raw-text/RCDATA elements are source, not a place
                // to interpret tag-looking strings in JS, CSS or text.
                let name = html_name(text);
                if let Some((name, false, tail)) = name
                    && (tail.is_empty() || tail.starts_with(|c: char| c.is_ascii_whitespace()) || tail == "/")
                    && let Some(tag) =
                        ["script", "style", "iframe", "textarea", "title", "xmp", "noembed", "noframes", "noscript", "plaintext"]
                            .into_iter()
                            .find(|tag| name.eq_ignore_ascii_case(tag))
                {
                    literal = Some((tag, depth));
                    stack.clear();
                    pending.clear();
                    continue;
                }
                if let Some((tag, close)) = html_tag(text) {
                    let parent = stack.last().map(|(tag, _)| *tag);
                    let list = |t| matches!(t, "ul" | "ol");
                    let valid = if !close && matches!(tag, "ul" | "ol" | "li" | "p") && phrasing > 0 {
                        false
                    } else if close {
                        stack.last() == Some(&(tag, depth))
                    } else if tag == "li" {
                        parent.is_some_and(list)
                    } else if parent.is_some_and(list) {
                        false
                    } else if matches!(tag, "ul" | "ol" | "p") {
                        parent.is_none() || parent == Some("li")
                    } else {
                        true
                    };
                    if valid && (close || stack.len() < 32) {
                        pending.push((index, tag, close));
                        if close {
                            stack.pop();
                        } else if tag != "br" {
                            stack.push((tag, depth));
                        }
                        if stack.is_empty() {
                            for (index, tag, close) in pending.drain(..) {
                                out[index] = Event::InlineHtml(format!("<{}{tag}>", if close { "/" } else { "" }).into());
                            }
                        }
                        continue;
                    }
                } else if !text.starts_with('<') && (text.trim().is_empty() || !matches!(stack.last(), Some(("ul" | "ol", _)))) {
                    continue;
                }
                stack.clear();
                pending.clear();
            }
        } else {
            // Inline Markdown can live in an HTML wrapper, but raw lists may
            // contain only list items, not bare Markdown text/containers.
            if matches!(stack.last(), Some(("ul" | "ol", _))) && !matches!(&ev, Event::Text(t) if t.trim().is_empty()) {
                stack.clear();
                pending.clear();
            }
            out.push(ev);
        }
    }
    // A list or p in an ordinary Markdown paragraph must not be nested in a
    // generated <p> (the browser would implicitly close/reparent that wrapper).
    let mut paragraph = None;
    let mut block = false;
    for i in 0..out.len() {
        match &out[i] {
            Event::Start(Tag::Paragraph) => {
                paragraph = Some(i);
                block = false;
            }
            Event::InlineHtml(h) if matches!(h.as_ref(), "<ul>" | "<ol>" | "<p>") => block = true,
            Event::End(TagEnd::Paragraph) => {
                if let Some(start) = paragraph.take().filter(|_| block) {
                    out[start] = Event::InlineHtml("".into());
                    out[i] = Event::InlineHtml("\n".into());
                }
            }
            _ => {}
        }
    }
    out
}

/// A deliberately strict tag grammar. Attribute syntax is checked, but no
/// attributes (or entities, names, whitespace, casing) are copied to output.
fn html_tag(text: &str) -> Option<(&'static str, bool)> {
    let space = |c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}');
    let (name, close, mut rest) = html_name(text)?;
    let tag = ["br", "ul", "ol", "li", "b", "strong", "i", "em", "code", "sub", "sup", "p", "kbd"]
        .into_iter()
        .find(|tag| name.eq_ignore_ascii_case(tag))?;
    if close {
        return (tag != "br" && rest.trim_matches(space).is_empty()).then_some((tag, true));
    }
    loop {
        let trimmed = rest.trim_start_matches(space);
        if trimmed.is_empty() || (tag == "br" && trimmed == "/") {
            return Some((tag, false));
        }
        if trimmed.len() == rest.len() {
            return None;
        }
        rest = trimmed;
        let name_end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))).unwrap_or(rest.len());
        if name_end == 0 || !rest.starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '_' | ':')) {
            return None;
        }
        rest = &rest[name_end..];
        let value = rest.trim_start_matches(space);
        if let Some(value) = value.strip_prefix('=') {
            let value = value.trim_start_matches(space);
            if value.starts_with(['\'', '"']) {
                let quote = value.as_bytes()[0] as char;
                let end = value[1..].find(quote)? + 1;
                if value[1..end].contains('\0') {
                    return None;
                }
                rest = &value[end + 1..];
            } else {
                let end = value.find(space).unwrap_or(value.len());
                if end == 0 || value[..end].contains(['\'', '"', '=', '<', '>', '`', '\0']) {
                    return None;
                }
                rest = &value[end..];
            }
        }
    }
}

fn html_name(text: &str) -> Option<(&str, bool, &str)> {
    let mut rest = text.strip_prefix('<')?.strip_suffix('>')?;
    let close = rest.starts_with('/');
    if close {
        rest = &rest[1..];
    }
    let end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
    Some((&rest[..end], close, &rest[end..]))
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
        let (reference_path, rest) = href.split_at(href.find(['?', '#']).unwrap_or(href.len()));
        return reference(reference_path).map(|(kind, id)| (kind, id, rest.to_owned()));
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
    use crate::icons::{Icon, Size};

    const ID: &str = "01a18f50-9f48-7567-8070-43e1f17ba642";

    fn nothing(_: Kind, _: &str) -> Known {
        Known::default()
    }

    #[test]
    fn single_and_grouped_citations_render_as_numbered_superscript_links() {
        for marker in
            ["\u{e200}cite\u{e202}turn347046search1\u{e201}", "\u{e200}cite\u{e202}turn919800search4\u{e202}turn919800search5\u{e201}"]
        {
            let text = format!("Fact{marker}.");
            let mut citations = vec![c6_api::citations::UrlCitation {
                url: "https://source.test/one".into(),
                title: "One & <two>".into(),
                start_index: 4,
                end_index: text.chars().count() - 1,
            }];
            if marker.contains("search5") {
                citations.push(c6_api::citations::UrlCitation {
                    url: "https://source.test/two".into(),
                    title: "Second".into(),
                    ..citations[0].clone()
                });
            }
            let html = render_citations(&text, &citations, &here(&nothing));
            assert!(html.contains("<sup><a href=\"https://source.test/one\" title=\"One &amp; &lt;two&gt;\">[1]</a></sup>"), "{html}");
            assert_eq!(html.matches("<sup>").count(), citations.len(), "{html}");
            assert!(html.ends_with("</sup>.</p>\n"), "{html}");
            assert!(!html.contains(['\u{e200}', '\u{e201}', '\u{e202}']));
        }
    }

    #[test]
    fn unresolved_citations_disappear_without_spaces_before_punctuation() {
        let marker = "\u{e200}cite\u{e202}turn347046search1\u{e201}";
        assert_eq!(render(&format!("Fact {marker}."), &here(&nothing)), "<p>Fact.</p>\n");
        assert_eq!(render(&format!("Fact. {marker}"), &here(&nothing)), "<p>Fact.</p>\n");
        assert_eq!(render(&format!("`{marker}`"), &here(&nothing)), format!("<p><code>{marker}</code></p>\n"));
        assert_eq!(render("An **ordinary** sentence.", &here(&nothing)), "<p>An <strong>ordinary</strong> sentence.</p>\n");
    }

    #[test]
    fn citation_titles_cannot_inject_html_and_only_web_urls_become_links() {
        let marker = "\u{e200}cite\u{e202}turn1search1\u{e201}";
        let mut citation = c6_api::citations::UrlCitation {
            url: "https://source.test/?q=\"<tag>".into(),
            title: "\" onmouseover=\"alert(1)".into(),
            start_index: 0,
            end_index: marker.chars().count(),
        };
        let html = render_citations(marker, &[citation.clone()], &here(&nothing));
        assert!(html.contains("title=\"&quot; onmouseover=&quot;alert(1)\""), "{html}");
        assert!(!html.contains("<tag>"), "{html}");
        citation.url = "javascript:alert(1)".into();
        assert_eq!(render_citations(marker, &[citation], &here(&nothing)), "");
    }

    #[test]
    fn document_mentions_keep_a_drawn_file_before_the_given_label() {
        let html = render(&format!("[the plan](doc:{ID})"), &here(&nothing));
        assert!(html.contains(&crate::icons::html::icon(Icon::FileText, Size::S14)), "{html}");
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
    fn allowed_html_is_rebuilt_without_attributes() {
        for tag in ["b", "strong", "i", "em", "code", "sub", "sup", "kbd", "p"] {
            let src = format!("<{tag} style='color:red' class=bad href=x src=x onclick=run() data-x=\"a>b<c\">text</{tag}>");
            let out = to_html(&src);
            assert!(out.contains(&format!("<{tag}>text</{tag}>")), "{src}: {out}");
            assert!(!out.contains("bad") && !out.contains("run()") && !out.contains("style="), "{out}");
        }
        assert_eq!(to_html("first<BR class=x />second<br>third"), "<p>first<br>second<br>third</p>\n");
        assert_eq!(to_html("<B title='x'>upper</B>"), "<p><b>upper</b></p>\n");
        assert_eq!(to_html("before <b>**bold**</b> after"), "<p>before <b><strong>bold</strong></b> after</p>\n");
    }

    #[test]
    fn html_lists_render_in_cells_paragraphs_and_blocks() {
        let list = "<ul onclick><li>first<ol start=5><li><em>nested</em></li></ol></li><li>second<br>line</li></ul>";
        let clean = "<ul><li>first<ol><li><em>nested</em></li></ol></li><li>second<br>line</li></ul>";
        let table = to_html(&format!("| List | Next |\n| --- | --- |\n| {list} | safe |"));
        assert!(table.contains(&format!("<td>{clean}</td><td>safe</td>")), "{table}");
        assert_eq!(to_html(&format!("before {list} after")), format!("before {clean} after\n"));
        assert_eq!(
            to_html("<ul>\n<li>first</li>\n<li>second</li>\n</ul>\n\nafter"),
            "<ul>\n<li>first</li>\n<li>second</li>\n</ul>\n<p>after</p>\n"
        );
        assert_eq!(to_html("<p>first<br>second</p>"), "<p>first<br>second</p>");
        assert_eq!(to_html("<ul\n onclick=run()>\n<li>multiline</li>\n</ul>"), "<ul>\n<li>multiline</li>\n</ul>");
    }

    #[test]
    fn unbalanced_or_badly_nested_html_stays_text_with_container_boundaries() {
        for src in [
            "<ul>",
            "<ul><li>first</li>",
            "<b>unclosed",
            "<li>orphan</li>",
            "<ul><b>bad</b></ul>",
            "<ul><li>x</ul></li>",
            "<b><i>x</b></i>",
            "<p><ul><li>x</li></ul></p>",
            "<ul>bare</ul>",
            "<b/>x</b>",
            "<b title='unfinished>x</b>",
            "<b =x>x</b>",
            "<b title=\"x\"id=y>x</b>",
        ] {
            let out = to_html(src);
            assert!(!out.contains("<ul>") && !out.contains("<li>") && !out.contains("<b>") && !out.contains("<i>"), "{src}: {out}");
            assert!(out.contains("&lt;"), "{src}: {out}");
        }
        let table = to_html("| A | B |\n|---|---|\n| <ul><li>open | </li></ul><b>safe</b> |\n\nafter");
        assert!(table.contains("<td>&lt;ul&gt;&lt;li&gt;open</td><td>&lt;/li&gt;&lt;/ul&gt;<b>safe</b></td>"), "{table}");
        assert!(table.ends_with("<p>after</p>\n"), "{table}");
        let paragraphs = to_html("first <b>open\n\nsecond</b>");
        assert_eq!(paragraphs, "<p>first &lt;b&gt;open</p>\n<p>second&lt;/b&gt;</p>\n");
        let crossing = to_html("*inside <b>outside* tail</b>");
        assert!(crossing.contains("<em>inside &lt;b&gt;outside</em> tail&lt;/b&gt;"), "{crossing}");
        let deep = to_html(&format!("{}x{}", "<b>".repeat(33), "</b>".repeat(33)));
        assert!(!deep.contains("<b>"), "depth is bounded: {deep}");
        let deepest = format!("{}x{}", "<b>".repeat(32), "</b>".repeat(32));
        assert_eq!(to_html(&deepest), format!("<p>{deepest}</p>\n"));
        assert_eq!(to_html("<p><ul><li><b>x</b></li></ul></p>"), "&lt;p&gt;&lt;ul&gt;&lt;li&gt;<b>x</b>&lt;/li&gt;&lt;/ul&gt;&lt;/p&gt;");
    }

    #[test]
    fn rejected_raw_text_bodies_and_multiline_comments_remain_literal() {
        for tag in ["script", "style", "iframe", "textarea", "title", "xmp", "noembed", "noframes", "noscript", "plaintext"] {
            for prefix in ["", "before "] {
                let out = to_html(&format!("{prefix}<{tag}>\"<b>literal</b>\"</{tag}>"));
                assert!(out.contains("&lt;b&gt;literal&lt;/b&gt;"), "{tag}: {out}");
                assert!(!out.contains("<b>"), "{tag}: {out}");
            }
        }
        let out = to_html("<scRipt>\nconst x = '<ul><li>literal</li></ul>';\n</scRipt>");
        assert!(out.contains("&lt;ul&gt;&lt;li&gt;literal&lt;/li&gt;&lt;/ul&gt;"), "{out}");
        assert!(!out.contains("<ul>"), "{out}");
        assert_eq!(to_html("<!--\n<b>comment</b>\n-->"), "&lt;!--\n&lt;b&gt;comment&lt;/b&gt;\n--&gt;");
        assert!(
            to_html("before <script><b>literal</b></script><b>real</b>").contains("&lt;b&gt;literal&lt;/b&gt;&lt;/script&gt;<b>real</b>")
        );
        assert_eq!(to_html("<script>if (a < b) x();</script><b>real</b>"), "&lt;script&gt;if (a &lt; b) x();&lt;/script&gt;<b>real</b>");
        assert_eq!(to_html("<style>/* a < b */</style><b>real</b>"), "&lt;style&gt;/* a &lt; b */&lt;/style&gt;<b>real</b>");
        let table = to_html("| A | B |\n|---|---|\n| <script><b>literal</b> | <b>real</b> |");
        assert!(table.contains("&lt;b&gt;literal&lt;/b&gt;</td><td><b>real</b>"), "{table}");
    }

    #[test]
    fn unsafe_html_and_entity_tricks_are_visible_not_interpreted() {
        for src in [
            "<img src=x onerror=alert(1)>",
            "<scRipt>alert(1)</scRipt>",
            "<style>p{display:none}</style>",
            "<iframe src=x></iframe>",
            "<a href=\"javascript:alert(1)\">link</a>",
            "<!-- <b>comment</b> -->",
            "<svg onload=alert(1)></svg>",
            "<u&#108;><li>x</li></ul>",
            "<b\0>x</b>",
            "<b\u{b}>x</b>",
        ] {
            let out = to_html(src);
            assert!(out.contains("&lt;"), "{src}: {out}");
            assert!(
                !out.contains("<img")
                    && !out.contains("<scRipt")
                    && !out.contains("<style")
                    && !out.contains("<iframe")
                    && !out.contains("<a ")
                    && !out.contains("<svg")
                    && !out.contains("<b>")
                    && !out.contains("<li>"),
                "{src}: {out}"
            );
        }
        assert_eq!(to_html("&lt;script&gt;alert(1)&lt;/script&gt;"), "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>\n");
        assert_eq!(to_html("&lt;b&gt;literal&lt;/b&gt;"), "<p>&lt;b&gt;literal&lt;/b&gt;</p>\n");
        assert_eq!(to_html("`<b>code</b>`"), "<p><code>&lt;b&gt;code&lt;/b&gt;</code></p>\n");
        assert!(to_html("```html\n<ul><li>code</li></ul>\n```").contains("&lt;ul&gt;&lt;li&gt;code&lt;/li&gt;&lt;/ul&gt;"));
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
    fn document_links_and_reference_mentions_keep_block_targets() {
        for href in [format!("/docs/{ID}#block.2"), format!("doc:{ID}#block.2"), format!("doc:{ID}?block=block.2")] {
            let out = render(&format!("[there]({href})"), &here(&nothing));
            let found = mentions(&out);
            let suffix = if href.contains('?') { "?block=block.2" } else { "#block.2" };
            assert_eq!(found[0].1, format!("/r/home/docs/{ID}{suffix}"), "{out}");
            assert_eq!(thing_of(&href, &here(&nothing)), Some((Kind::Doc, ID.into(), suffix.into())));
        }
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
