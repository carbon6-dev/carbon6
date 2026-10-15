//! Markdown from the repository to HTML for the site: headings get ids to link
//! to, and links between repository files become links between site pages,
//! or to GitHub for files the site doesn't publish.

use std::path::Path;

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};

use crate::{Site, docs};

pub struct Rendered {
    /// The text of the first level-1 heading, which `html` leaves out.
    pub title: Option<String>,
    pub html: String,
    /// The headings below the title, for a table of contents.
    pub toc: Vec<Heading>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Heading {
    pub level: u8,
    pub id: String,
    pub text: String,
}

/// Renders `markdown` from the repository file `file` (relative to the root).
pub fn render(site: &Site, file: &str, markdown: &str) -> Rendered {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES;
    let mut events: Vec<Event> = Parser::new_ext(markdown, options).collect();

    let mut title = None;
    let mut toc = Vec::new();
    let mut used = Vec::<String>::new();
    let mut drop = None;
    let mut i = 0;
    while i < events.len() {
        if let Event::Start(Tag::Heading { level, .. }) = &events[i] {
            let level = *level;
            let end = i + events[i..].iter().position(|e| matches!(e, Event::End(TagEnd::Heading(_)))).unwrap();
            let text: String = events[i + 1..end]
                .iter()
                .filter_map(|e| match e {
                    Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
                    _ => None,
                })
                .collect();
            if level == HeadingLevel::H1 && title.is_none() {
                title = Some(text);
                drop = Some(i..=end);
            } else {
                let mut id = slug(&text);
                let base = id.clone();
                let mut n = 1;
                while used.contains(&id) {
                    id = format!("{base}-{n}");
                    n += 1;
                }
                used.push(id.clone());
                toc.push(Heading { level: level as u8, id: id.clone(), text });
                if let Event::Start(Tag::Heading { id: heading_id, .. }) = &mut events[i] {
                    *heading_id = Some(CowStr::from(id));
                }
            }
            i = end;
        }
        i += 1;
    }
    if let Some(range) = drop {
        events.drain(range);
    }

    for event in &mut events {
        if let Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) = event {
            *dest_url = CowStr::from(link(site, file, dest_url));
        }
    }

    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    Rendered { title, html: out, toc }
}

/// The id for a heading: GitHub's, so links into the repository's markdown
/// work on the site too ("## 4. Cutting a release" → "4-cutting-a-release").
fn slug(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// Where a link in repository file `from` should point on the site.
fn link(site: &Site, from: &str, dest: &str) -> String {
    if dest.is_empty() || dest.starts_with('#') || dest.contains("://") || dest.starts_with("mailto:") {
        return dest.to_string();
    }
    let (path, fragment) = match dest.split_once('#') {
        Some((p, f)) => (p, format!("#{f}")),
        None => (dest, String::new()),
    };
    let target = resolve(from, path);
    if let Some(page) = docs::site_path(&target) {
        return format!("{page}{fragment}");
    }
    let kind = if site.root.join(&target).is_dir() { "tree" } else { "blob" };
    format!("{}/{kind}/main/{target}{fragment}", site.repo_url)
}

/// `path`, relative to the directory of `from`, as a path from the root.
fn resolve(from: &str, path: &str) -> String {
    let mut parts: Vec<&str> =
        if path.starts_with('/') { Vec::new() } else { Path::new(from).parent().into_iter().flat_map(|p| p.to_str()).collect() };
    parts.retain(|p| !p.is_empty());
    let mut parts: Vec<&str> = parts.iter().flat_map(|p| p.split('/')).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> Site {
        Site {
            root: Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf(),
            version: "0.0.0".into(),
            base_url: "https://carbon6.dev".into(),
            repo_url: "https://github.com/carbon6-dev/carbon6".into(),
            asset_version: String::new(),
        }
    }

    #[test]
    fn links_between_files_become_links_between_pages() {
        let site = site();
        assert_eq!(link(&site, "docs/os.md", "packages.md#2-words"), "/docs/packages/#2-words");
        assert_eq!(link(&site, "README.md", "docs/memex.md"), "/docs/memex/");
        assert_eq!(link(&site, "docs/os.md", "../CHANGELOG.md"), "/changelog/");
        assert_eq!(link(&site, "README.md", "examples/apps"), "https://github.com/carbon6-dev/carbon6/tree/main/examples/apps");
        assert_eq!(link(&site, "docs/os.md", "../Makefile"), "https://github.com/carbon6-dev/carbon6/blob/main/Makefile");
        assert_eq!(link(&site, "docs/os.md", "#3-realms"), "#3-realms");
        assert_eq!(link(&site, "docs/os.md", "https://example.com/a.md"), "https://example.com/a.md");
    }

    #[test]
    fn headings_get_github_ids_and_the_title_moves_out() {
        let r = render(&site(), "docs/x.md", "# Title\n\n## 4. Cutting a release\n\ntext\n\n## Words\n\n### Words\n");
        assert_eq!(r.title.as_deref(), Some("Title"));
        assert!(!r.html.contains("<h1"));
        assert!(r.html.contains(r#"<h2 id="4-cutting-a-release">"#), "{}", r.html);
        assert!(r.html.contains(r#"<h3 id="words-1">"#), "{}", r.html);
        let toc: Vec<_> = r.toc.iter().map(|h| (h.level, h.id.as_str(), h.text.as_str())).collect();
        assert_eq!(toc, [(2, "4-cutting-a-release", "4. Cutting a release"), (2, "words", "Words"), (3, "words-1", "Words")]);
    }
}
