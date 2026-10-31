//! The Markdown exchange (docs/docs.md §2): a document's Markdown split
//! into blocks, and blocks written back as Markdown. This is carbon6's own
//! format, not an outside one: it splits Markdown at block boundaries
//! (headings, paragraphs, list items, tables, code, quotes, HTML, rules)
//! and keeps each block's source as is; nothing renders it here.
//!
//! `<?c6 block k?>` tokens stand for what isn't text: alone on a line, a
//! pending section or an embed (its value in the `blocks` map, by `k`);
//! inside text, a chip. A deck's Markdown separates slides with `---`; a
//! slide's header is HTML comments: `<!-- layout: two-columns -->`,
//! `<!-- hidden -->`, `<!-- notes: … -->`, and `<!-- region: right -->`
//! moves what follows into a region.

use serde_json::{Map, Value, json};

use crate::bad_request;

/// A block as parsed, before it has an id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parsed {
    pub kind: String,
    pub text: String,
    pub attrs: Value,
    /// On a slide: its region.
    pub region: String,
    /// A slide's blocks.
    pub children: Vec<Parsed>,
    /// The token key it came from (pending, embeds), to report its id.
    pub key: Option<String>,
}

/// Embeds take a line of their own; chips sit inside text.
pub const EMBEDS: [&str; 5] = ["chart", "diagram", "image", "file", "view"];
pub const CHIPS: [&str; 4] = ["date", "mention", "dropdown", "ref"];

const TOKEN_OPEN: &str = "<?c6 block ";
const TOKEN_CLOSE: &str = "?>";

/// The keys of the tokens in `text`, in order.
pub fn token_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(TOKEN_OPEN) {
        let after = &rest[i + TOKEN_OPEN.len()..];
        match after.find(TOKEN_CLOSE) {
            Some(j) => {
                out.push(after[..j].trim().to_owned());
                rest = &after[j + TOKEN_CLOSE.len()..];
            }
            None => break,
        }
    }
    out
}

/// `text` with each token's key renamed by `f` (None: kept).
pub fn rename_tokens(text: &str, f: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(TOKEN_OPEN) {
        let after = &rest[i + TOKEN_OPEN.len()..];
        let Some(j) = after.find(TOKEN_CLOSE) else { break };
        let key = after[..j].trim();
        out.push_str(&rest[..i]);
        out.push_str(TOKEN_OPEN);
        out.push_str(&f(key).unwrap_or_else(|| key.to_owned()));
        out.push_str(TOKEN_CLOSE);
        rest = &after[j + TOKEN_CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

pub fn token(key: &str) -> String {
    format!("{TOKEN_OPEN}{key}{TOKEN_CLOSE}")
}

/// The key of a line that is only a token.
fn lone_token(line: &str) -> Option<String> {
    let t = line.trim();
    let inner = t.strip_prefix(TOKEN_OPEN)?.strip_suffix(TOKEN_CLOSE)?;
    (!inner.contains(TOKEN_CLOSE)).then(|| inner.trim().to_owned())
}

fn value_type(v: &Value) -> &str {
    v["type"].as_str().unwrap_or("")
}

fn heading_level(line: &str) -> Option<usize> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let n = t.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&n) && (t.len() == n || t[n..].starts_with(' '))).then_some(n)
}

fn fence(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

/// A list item's start: `- `, `* `, `+ `, `1. ` or `1) ` at most 3 spaces in.
fn is_item(line: &str) -> bool {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return false;
    }
    if let Some(r) = t.strip_prefix(['-', '*', '+']) {
        return r.is_empty() || r.starts_with(' ');
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && digits <= 9 && t[digits..].starts_with(['.', ')']) && (t.len() == digits + 1 || t[digits + 1..].starts_with(' '))
}

fn is_table(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_quote(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

fn is_html(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('<') && !t.starts_with(TOKEN_OPEN) && t[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '!' || c == '/')
}

fn starts_block(line: &str) -> bool {
    heading_level(line).is_some()
        || fence(line).is_some()
        || is_item(line)
        || is_table(line)
        || is_quote(line)
        || is_html(line)
        || lone_token(line).is_some()
        || is_rule(line)
}

/// A doc's (or a slide region's) Markdown as blocks. Tokens' values come
/// from `blocks`; a token with no value is refused.
pub fn parse(md: &str, blocks: &Map<String, Value>) -> anyhow::Result<Vec<Parsed>> {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        let kind;
        if let Some(f) = fence(line) {
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(f) {
                i += 1;
            }
            i = (i + 1).min(lines.len());
            kind = "code";
        } else if heading_level(line).is_some() {
            i += 1;
            kind = "heading";
        } else if let Some(key) = lone_token(line) {
            i += 1;
            let v = blocks.get(&key).ok_or_else(|| bad_request(format!("no value for {} in blocks", token(&key))))?;
            match value_type(v) {
                "pending" => {
                    out.push(Parsed { kind: "pending".into(), attrs: pending_attrs(v)?, key: Some(key), ..Default::default() });
                    continue;
                }
                t if EMBEDS.contains(&t) => {
                    out.push(Parsed { kind: "embed".into(), attrs: v.clone(), key: Some(key), ..Default::default() });
                    continue;
                }
                // A chip alone on its line is a paragraph holding it.
                _ => kind = "paragraph",
            }
        } else if is_rule(line) {
            i += 1;
            kind = "rule";
        } else if is_table(line) {
            while i < lines.len() && is_table(lines[i]) {
                i += 1;
            }
            kind = "table";
        } else if is_quote(line) {
            while i < lines.len() && !lines[i].trim().is_empty() && (is_quote(lines[i]) || !starts_block(lines[i])) {
                i += 1;
            }
            kind = "quote";
        } else if is_item(line) {
            i += 1;
            // Its continuation: indented lines, also after a blank line.
            loop {
                if i < lines.len() && !lines[i].trim().is_empty() && (lines[i].starts_with("  ") || lines[i].starts_with('\t')) {
                    i += 1;
                } else if i < lines.len() && !lines[i].trim().is_empty() && !starts_block(lines[i]) {
                    // A lazy continuation line.
                    i += 1;
                } else if i + 1 < lines.len()
                    && lines[i].trim().is_empty()
                    && (lines[i + 1].starts_with("  ") || lines[i + 1].starts_with('\t'))
                    && !lines[i + 1].trim().is_empty()
                {
                    i += 2;
                } else {
                    break;
                }
            }
            kind = "item";
        } else if is_html(line) {
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            kind = "html";
        } else {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() && !starts_block(lines[i]) {
                i += 1;
            }
            kind = "paragraph";
        }
        let text = lines[start..i].join("\n").trim_end().to_owned();
        let mut p = Parsed { kind: kind.into(), text, attrs: json!({}), ..Default::default() };
        chips_of(&mut p, blocks)?;
        out.push(p);
    }
    Ok(out)
}

fn pending_attrs(v: &Value) -> anyhow::Result<Value> {
    let intent = v["intent"].as_str().unwrap_or("").trim();
    if intent.is_empty() {
        return Err(bad_request("a pending block says what will be written there: {\"type\": \"pending\", \"intent\": \"…\"}"));
    }
    Ok(json!({"intent": intent.chars().take(500).collect::<String>()}))
}

/// The chips a text block holds, from `blocks`, kept in its attrs.
fn chips_of(p: &mut Parsed, blocks: &Map<String, Value>) -> anyhow::Result<()> {
    let keys = token_keys(&p.text);
    if keys.is_empty() {
        return Ok(());
    }
    let mut chips = Map::new();
    for k in keys {
        let v = blocks.get(&k).ok_or_else(|| bad_request(format!("no value for {} in blocks", token(&k))))?;
        let t = value_type(v);
        if !CHIPS.contains(&t) {
            return Err(bad_request(format!(
                "{}: a {t:?} takes a line of its own; inside text go chips ({})",
                token(&k),
                CHIPS.join(", ")
            )));
        }
        chips.insert(k, v.clone());
    }
    p.attrs["chips"] = Value::Object(chips);
    Ok(())
}

/// A deck's Markdown as slides (and pending slides).
pub fn parse_deck(md: &str, blocks: &Map<String, Value>) -> anyhow::Result<Vec<Parsed>> {
    let mut slides = Vec::new();
    let mut fenced: Option<&str> = None;
    let mut current: Vec<&str> = Vec::new();
    let mut sections: Vec<String> = Vec::new();
    for line in md.lines() {
        if let Some(f) = fenced {
            if line.trim_start().starts_with(f) {
                fenced = None;
            }
        } else if let Some(f) = fence(line) {
            fenced = Some(f);
        } else if line.trim() == "---" {
            sections.push(current.join("\n"));
            current.clear();
            continue;
        }
        current.push(line);
    }
    sections.push(current.join("\n"));
    let first_nonempty = sections.iter().position(|s| !s.trim().is_empty());
    for (n, s) in sections.iter().enumerate() {
        if s.trim().is_empty() {
            continue;
        }
        slides.push(parse_slide(s, blocks, Some(n) == first_nonempty)?);
    }
    Ok(slides)
}

/// One slide's Markdown: its header, then its regions' blocks.
pub fn parse_slide(md: &str, blocks: &Map<String, Value>, first: bool) -> anyhow::Result<Parsed> {
    // A slide that is only a pending token is a pending slide.
    let only: Vec<&str> = md.lines().filter(|l| !l.trim().is_empty()).collect();
    if only.len() == 1
        && let Some(key) = lone_token(only[0])
        && let Some(v) = blocks.get(&key)
        && value_type(v) == "pending"
    {
        return Ok(Parsed { kind: "pending".into(), attrs: pending_attrs(v)?, key: Some(key), ..Default::default() });
    }
    let mut layout: Option<String> = None;
    let mut notes = String::new();
    let mut hidden = false;
    // Content by region (None: not said), in order.
    let mut parts: Vec<(Option<String>, String)> = vec![(None, String::new())];
    let mut lines = md.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if let Some(c) = t.strip_prefix("<!--") {
            let mut body = c.to_owned();
            if !body.contains("-->") {
                for more in lines.by_ref() {
                    body.push('\n');
                    body.push_str(more);
                    if more.contains("-->") {
                        break;
                    }
                }
            }
            let inner = body.split("-->").next().unwrap_or("").trim();
            let (key, value) = match inner.split_once(':') {
                Some((k, v)) => (k.trim().to_lowercase(), v.trim().to_owned()),
                None => (inner.to_lowercase(), String::new()),
            };
            match key.as_str() {
                "layout" => layout = Some(value),
                "notes" => {
                    if !notes.is_empty() {
                        notes.push('\n');
                    }
                    notes.push_str(&value);
                }
                "hidden" => hidden = true,
                "region" => parts.push((Some(value), String::new())),
                // Any other comment is the slide's content.
                _ => {
                    let part = &mut parts.last_mut().unwrap().1;
                    part.push_str(line);
                    part.push('\n');
                }
            }
            continue;
        }
        let part = &mut parts.last_mut().unwrap().1;
        part.push_str(line);
        part.push('\n');
    }
    let layout = layout.filter(|l| !l.is_empty()).unwrap_or_else(|| if first { "title".into() } else { "body".into() });
    if !c6_api::docs::LAYOUTS.iter().any(|(l, _)| *l == layout) {
        return Err(bad_request(format!(
            "{layout:?} is not a layout ({})",
            c6_api::docs::LAYOUTS.iter().map(|(l, _)| *l).collect::<Vec<_>>().join(", ")
        )));
    }
    let regions = c6_api::docs::regions(&layout);
    let default = regions.iter().find(|r| **r != "title").copied().unwrap_or(regions[0]);
    let mut children = Vec::new();
    for (region, text) in parts {
        let said = region.is_some();
        let region = region.unwrap_or_else(|| default.to_owned());
        if !regions.contains(&region.as_str()) {
            return Err(bad_request(format!("a {layout} slide's regions are {}, not {region:?}", regions.join(", "))));
        }
        for mut b in parse(&text, blocks)? {
            // Unsaid: a heading first goes to the title.
            b.region =
                if !said && regions.contains(&"title") && b.kind == "heading" && !children.iter().any(|c: &Parsed| c.region == "title") {
                    "title".into()
                } else {
                    region.clone()
                };
            children.push(b);
        }
    }
    let mut attrs = json!({"layout": layout});
    if !notes.trim().is_empty() {
        attrs["notes"] = json!(notes.trim());
    }
    if hidden {
        attrs["hidden"] = json!(true);
    }
    Ok(Parsed { kind: "slide".into(), attrs, children, ..Default::default() })
}

// --- Writing ------------------------------------------------------------------

/// A block as written: its Markdown, with its tokens' values put in `blocks`.
pub struct Out<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub text: &'a str,
    pub attrs: &'a Value,
    pub region: &'a str,
}

/// One block's Markdown (not a slide).
pub fn write_block(b: &Out, blocks: &mut Map<String, Value>) -> String {
    match b.kind {
        "pending" => {
            let mut v = b.attrs.clone();
            v["type"] = json!("pending");
            blocks.insert(b.id.to_owned(), v);
            token(b.id)
        }
        "embed" => {
            blocks.insert(b.id.to_owned(), b.attrs.clone());
            token(b.id)
        }
        _ => {
            if let Some(chips) = b.attrs["chips"].as_object() {
                for (k, v) in chips {
                    blocks.insert(k.clone(), v.clone());
                }
            }
            b.text.to_owned()
        }
    }
}

/// Blocks in order as Markdown: list items one line apart, the rest a
/// blank line apart.
pub fn write_blocks(list: &[Out], blocks: &mut Map<String, Value>) -> String {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for b in list {
        if let Some(p) = prev {
            out.push_str(if p == "item" && b.kind == "item" { "\n" } else { "\n\n" });
        }
        out.push_str(&write_block(b, blocks));
        prev = Some(b.kind);
    }
    out
}

/// A slide as Markdown: its header, then its regions in the layout's order.
pub fn write_slide(attrs: &Value, children: &[Out], blocks: &mut Map<String, Value>) -> String {
    let layout = attrs["layout"].as_str().unwrap_or("body");
    let mut out = format!("<!-- layout: {layout} -->\n");
    if attrs["hidden"] == true {
        out.push_str("<!-- hidden -->\n");
    }
    let regions = c6_api::docs::regions(layout);
    let default = regions.iter().find(|r| **r != "title").copied().unwrap_or(regions[0]);
    let has_title = regions.contains(&"title");
    let title_empty = !children.iter().any(|c| c.region == "title");
    // Unmarked, a heading first goes to the title and the rest to the
    // default region; once a marker is written, every region is marked.
    let mut marked = false;
    for region in regions {
        let in_region: Vec<&Out> = children.iter().filter(|c| c.region == *region).collect();
        if in_region.is_empty() {
            continue;
        }
        let implicit = !marked
            && ((*region == "title" && in_region.len() == 1 && in_region[0].kind == "heading")
                || (*region == default && !(has_title && title_empty && in_region[0].kind == "heading")));
        out.push('\n');
        if !implicit {
            out.push_str(&format!("<!-- region: {region} -->\n"));
            marked = true;
        }
        let owned: Vec<Out> =
            in_region.iter().map(|o| Out { id: o.id, kind: o.kind, text: o.text, attrs: o.attrs, region: o.region }).collect();
        out.push_str(&write_blocks(&owned, blocks));
        out.push('\n');
    }
    if let Some(notes) = attrs["notes"].as_str().filter(|n| !n.trim().is_empty()) {
        out.push_str(&format!("\n<!-- notes: {} -->\n", notes.replace("-->", "-- >")));
    }
    out.trim_end().to_owned()
}

/// A pending slide's Markdown.
pub fn write_pending_slide(id: &str, attrs: &Value, blocks: &mut Map<String, Value>) -> String {
    write_block(&Out { id, kind: "pending", text: "", attrs, region: "" }, blocks)
}

/// A heading's text without its marks, or a block's first words.
pub fn first_words(kind: &str, text: &str, n: usize) -> String {
    let t = match kind {
        "heading" => text.trim_start().trim_start_matches('#').trim(),
        "item" => text.trim_start().trim_start_matches(['-', '*', '+']).trim_start(),
        _ => text.trim(),
    };
    let line = t.lines().next().unwrap_or("").trim();
    let mut s: String = line.chars().take(n).collect();
    if line.chars().count() > n {
        s.push('…');
    }
    s
}

pub fn level(text: &str) -> usize {
    heading_level(text).unwrap_or(0)
}

// --- Tables -------------------------------------------------------------------

/// A table's rows of cells (the separator row left out), and whether the
/// second line was a separator.
pub fn table_cells(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter(|l| !is_separator(l))
        .map(|l| {
            let t = l.trim();
            let t = t.strip_prefix('|').unwrap_or(t);
            let t = t.strip_suffix('|').unwrap_or(t);
            split_cells(t).into_iter().map(|c| c.trim().to_owned()).collect()
        })
        .collect()
}

fn split_cells(row: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut escaped = false;
    for c in row.chars() {
        if c == '|' && !escaped {
            cells.push(String::new());
        } else {
            cells.last_mut().unwrap().push(c);
        }
        escaped = c == '\\' && !escaped;
    }
    cells
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Rows of cells as a table, the first row its header.
pub fn table_text(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let line = |r: &[String]| {
        let cells: Vec<String> = (0..cols).map(|i| r.get(i).map(|c| c.replace('\n', " ")).unwrap_or_default()).collect();
        format!("| {} |", cells.join(" | "))
    };
    let mut out = Vec::new();
    for (n, r) in rows.iter().enumerate() {
        out.push(line(r));
        if n == 0 {
            out.push(format!("|{}|", vec![" --- "; cols].join("|")));
        }
    }
    out.join("\n")
}

/// A cell's text, escaped for a table.
pub fn cell_text(t: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for c in t.trim().chars() {
        if c == '|' && !escaped {
            out.push('\\');
        }
        out.push(c);
        escaped = c == '\\' && !escaped;
    }
    out.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(v: &[Parsed]) -> Vec<&str> {
        v.iter().map(|p| p.kind.as_str()).collect()
    }

    #[test]
    fn markdown_splits_into_blocks() {
        let md = "# Plan\n\nWe ship in two phases,\nstarting now.\n\n- one\n- two\n  more of two\n\n1. first\n\n```rust\nfn x() {}\n\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n> still\n\n---\n\n<div>x</div>\n\n## Next";
        let p = parse(md, &Map::new()).unwrap();
        assert_eq!(kinds(&p), ["heading", "paragraph", "item", "item", "item", "code", "table", "quote", "rule", "html", "heading"]);
        assert_eq!(p[1].text, "We ship in two phases,\nstarting now.");
        assert_eq!(p[3].text, "- two\n  more of two");
        assert_eq!(p[5].text, "```rust\nfn x() {}\n\n```");
        // Written back, it's the same Markdown.
        let outs: Vec<Out> = p.iter().map(|b| Out { id: "x", kind: &b.kind, text: &b.text, attrs: &b.attrs, region: "" }).collect();
        let back = write_blocks(&outs, &mut Map::new());
        assert_eq!(back, md.replace("- two\n  more of two\n\n1. first", "- two\n  more of two\n1. first"));
    }

    #[test]
    fn tokens_are_pending_embeds_and_chips() {
        let blocks: Map<String, Value> = serde_json::from_value(json!({
            "s1": {"type": "pending", "intent": "Risks: what could slip"},
            "d": {"type": "date", "value": "2026-11-01"},
            "c": {"type": "chart", "rows": []},
        }))
        .unwrap();
        let p = parse("# T\n\nDue <?c6 block d?>.\n\n<?c6 block s1?>\n\n<?c6 block c?>", &blocks).unwrap();
        assert_eq!(kinds(&p), ["heading", "paragraph", "pending", "embed"]);
        assert_eq!(p[1].attrs["chips"]["d"]["value"], "2026-11-01");
        assert_eq!((p[2].key.as_deref(), p[2].attrs["intent"].as_str()), (Some("s1"), Some("Risks: what could slip")));
        assert!(parse("<?c6 block nope?>", &blocks).is_err(), "a token with no value");
        assert!(parse("A chart <?c6 block c?> inline", &blocks).is_err(), "embeds take a line");
        assert_eq!(rename_tokens("a <?c6 block d?> b", |k| (k == "d").then(|| "u.4".into())), "a <?c6 block u.4?> b");
    }

    #[test]
    fn decks_are_slides_with_layouts_regions_and_notes() {
        let blocks: Map<String, Value> =
            serde_json::from_value(json!({"p": {"type": "pending", "intent": "The numbers: revenue up"}})).unwrap();
        let md = "# Q3 review\n\nWhere we are\n\n<!-- notes: Open with the headline. -->\n\n---\n\n<!-- layout: two-columns -->\n## Before and after\n<!-- region: left -->\n- slow\n<!-- region: right -->\n- fast\n\n---\n\n<?c6 block p?>\n\n---\n\n<!-- hidden -->\n## Spare\n\n- x\n\n```\n---\n```";
        let s = parse_deck(md, &blocks).unwrap();
        assert_eq!(kinds(&s), ["slide", "slide", "pending", "slide"]);
        assert_eq!(s[0].attrs["layout"], "title");
        assert_eq!(s[0].attrs["notes"], "Open with the headline.");
        assert_eq!(s[0].children.iter().map(|c| c.region.as_str()).collect::<Vec<_>>(), ["title", "subtitle"]);
        assert_eq!(s[1].children.iter().map(|c| c.region.as_str()).collect::<Vec<_>>(), ["title", "left", "right"]);
        assert_eq!(s[3].attrs["hidden"], true);
        assert_eq!(s[3].attrs["layout"], "body");
        assert_eq!(s[3].children.last().unwrap().kind, "code", "--- in a fence is not a slide break");
        // Written back and read again, it's the same slide.
        for slide in [&s[0], &s[1], &s[3]] {
            let outs: Vec<Out> =
                slide.children.iter().map(|b| Out { id: "x", kind: &b.kind, text: &b.text, attrs: &b.attrs, region: &b.region }).collect();
            let md = write_slide(&slide.attrs, &outs, &mut Map::new());
            let again = parse_slide(&md, &Map::new(), false).unwrap();
            assert_eq!(&again, slide, "{md}");
        }
        assert!(parse_deck("<!-- layout: nope -->\n# x", &Map::new()).is_err());
    }

    #[test]
    fn tables_by_cell() {
        let t = "| a | b |\n|---|---|\n| 1 | x \\| y |";
        let cells = table_cells(t);
        assert_eq!(cells, [vec!["a", "b"], vec!["1", "x \\| y"]]);
        assert_eq!(table_text(&cells), "| a | b |\n| --- | --- |\n| 1 | x \\| y |");
        assert_eq!(cell_text("p|q"), "p\\|q");
    }
}
