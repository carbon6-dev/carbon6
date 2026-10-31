//! A document in memory: its fields, tabs and blocks, and the operations
//! that change it (docs/docs.md §7). A batch is applied to a copy; if any
//! operation is refused, the copy is dropped and nothing lands.

use std::collections::BTreeMap;

use c6_api::docs::{self as d, DocBlock, DocOp};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Digest;

use super::markdown::{self as md, Out, Parsed};
use crate::{bad_request, conflict, not_found};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub theme: String,
    #[serde(default)]
    pub size: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub title: String,
    pub pos: f64,
}

/// A block as stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub tab: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    pub pos: f64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub attrs: Value,
    pub by: String,
    pub rev: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub meta: Meta,
    pub tabs: Vec<Tab>,
    pub rows: BTreeMap<String, Row>,
    /// The clock new ids are issued from.
    pub clock: i64,
}

/// What applying a batch did, for its answer.
#[derive(Debug, Default)]
pub struct Applied {
    pub ids: Vec<Vec<String>>,
    pub keys: Map<String, Value>,
}

/// The writer part of an id: who wrote it, short.
pub fn writer(by: &str) -> String {
    if by == "you" {
        return "u".into();
    }
    if let Some(s) = by.strip_prefix("session:") {
        let short: String = s.chars().filter(char::is_ascii_alphanumeric).take(6).collect();
        return format!("s{short}");
    }
    if by == "assistant" {
        return "a".into();
    }
    "t".into()
}

fn hash_of(parts: &[&str]) -> String {
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex::encode(&h.finalize()[..6])
}

impl State {
    pub fn new(meta: Meta) -> Self {
        Self { meta, ..Default::default() }
    }

    /// A new document's first tab.
    pub fn first_tab(&mut self, title: &str, by: &str) -> String {
        let id = self.issue(&writer(by));
        self.tabs.push(Tab { id: id.clone(), title: title.to_owned(), pos: 1024.0 });
        id
    }

    fn issue(&mut self, w: &str) -> String {
        self.clock += 1;
        format!("{w}.{}", self.clock)
    }

    pub fn tab(&self, id: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tabs_in_order(&self) -> Vec<&Tab> {
        let mut t: Vec<&Tab> = self.tabs.iter().collect();
        t.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        t
    }

    /// Blocks in a place (tab, parent, region), in order.
    pub fn siblings(&self, tab: &str, parent: &str, region: &str) -> Vec<&Row> {
        let mut v: Vec<&Row> = self.rows.values().filter(|r| r.tab == tab && r.parent == parent && r.region == region).collect();
        v.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        v
    }

    /// A slide's blocks, by its layout's regions, in order.
    pub fn children(&self, slide: &Row) -> Vec<&Row> {
        let layout = slide.attrs["layout"].as_str().unwrap_or("body");
        let mut out = Vec::new();
        for r in d::regions(layout) {
            out.extend(self.siblings(&slide.tab, &slide.id, r));
        }
        // Blocks in a region the layout no longer has stay, at the end.
        let regions = d::regions(layout);
        let mut stray: Vec<&Row> = self.rows.values().filter(|r| r.parent == slide.id && !regions.contains(&r.region.as_str())).collect();
        stray.sort_by(|a, b| a.region.cmp(&b.region).then(a.pos.total_cmp(&b.pos)));
        out.extend(stray);
        out
    }

    pub fn row(&self, id: &str) -> anyhow::Result<&Row> {
        self.rows.get(id).ok_or_else(|| not_found(format!("no block {id} in this document")))
    }

    pub fn hash(&self, r: &Row) -> String {
        if r.kind == "slide" {
            let kids: Vec<String> = self.children(r).iter().map(|c| format!("{}:{}:{}", c.id, c.region, self.hash(c))).collect();
            let attrs = r.attrs.to_string();
            let mut parts = vec!["slide", attrs.as_str()];
            parts.extend(kids.iter().map(String::as_str));
            hash_of(&parts)
        } else {
            hash_of(&[&r.kind, &r.text, &r.attrs.to_string()])
        }
    }

    /// A block as the API gives it.
    pub fn block(&self, r: &Row) -> DocBlock {
        DocBlock {
            id: r.id.clone(),
            kind: r.kind.clone(),
            text: r.text.clone(),
            attrs: if r.attrs.is_null() { json!({}) } else { r.attrs.clone() },
            region: r.region.clone(),
            children: if r.kind == "slide" { self.children(r).into_iter().map(|c| self.block(c)).collect() } else { Vec::new() },
            hash: self.hash(r),
            by: r.by.clone(),
            rev: r.rev,
        }
    }

    pub fn blocks_of(&self, tab: &str) -> Vec<DocBlock> {
        self.siblings(tab, "", "").into_iter().map(|r| self.block(r)).collect()
    }

    // --- Markdown out -------------------------------------------------------------

    fn out<'a>(r: &'a Row) -> Out<'a> {
        Out { id: &r.id, kind: &r.kind, text: &r.text, attrs: &r.attrs, region: &r.region }
    }

    /// One top-level block's (or a slide's) Markdown.
    pub fn markdown_of(&self, r: &Row, blocks: &mut Map<String, Value>) -> String {
        match r.kind.as_str() {
            "slide" => {
                let kids = self.children(r);
                let outs: Vec<Out> = kids.iter().map(|c| Self::out(c)).collect();
                md::write_slide(&r.attrs, &outs, blocks)
            }
            _ => md::write_block(&Self::out(r), blocks),
        }
    }

    /// A tab's Markdown: a deck's slides separated by `---`.
    pub fn markdown(&self, tab: &str, blocks: &mut Map<String, Value>) -> String {
        let top = self.siblings(tab, "", "");
        if self.meta.kind == "deck" {
            top.iter().map(|r| self.markdown_of(r, blocks)).collect::<Vec<_>>().join("\n\n---\n\n")
        } else {
            let outs: Vec<Out> = top.iter().map(|r| Self::out(r)).collect();
            md::write_blocks(&outs, blocks)
        }
    }

    /// The text of a tab, for limits.
    fn tab_size(&self, tab: &str) -> usize {
        self.rows.values().filter(|r| r.tab == tab).map(|r| r.text.len() + r.attrs.to_string().len()).sum()
    }

    // --- Positions ------------------------------------------------------------

    /// `n` positions in order between two (None: an end), spreading the
    /// place's blocks out again first when there's no room.
    fn positions(&mut self, place: (&str, &str, &str), after: Option<&str>, before: Option<&str>, n: usize) -> Vec<f64> {
        let get = |s: &State, id: Option<&str>| id.and_then(|i| s.rows.get(i)).map(|r| r.pos);
        let (mut lo, mut hi) = (get(self, after), get(self, before));
        let room = match (lo, hi) {
            (Some(a), Some(b)) => (b - a) / (n as f64 + 1.0) > 1e-9,
            _ => true,
        };
        if !room {
            let ids: Vec<String> = self.siblings(place.0, place.1, place.2).iter().map(|r| r.id.clone()).collect();
            for (i, id) in ids.iter().enumerate() {
                self.rows.get_mut(id).unwrap().pos = (i as f64 + 1.0) * 1024.0;
            }
            lo = get(self, after);
            hi = get(self, before);
        }
        match (lo, hi) {
            (Some(a), Some(b)) => (1..=n).map(|i| a + (b - a) * i as f64 / (n as f64 + 1.0)).collect(),
            (Some(a), None) => (1..=n).map(|i| a + 1024.0 * i as f64).collect(),
            (None, Some(b)) => (1..=n).map(|i| b - 1024.0 * (n + 1 - i) as f64).collect(),
            (None, None) => (1..=n).map(|i| 1024.0 * i as f64).collect(),
        }
    }

    /// The neighbours a new block goes between: after `after` (None: at
    /// the start), before the one following it.
    fn next_after(&self, place: (&str, &str, &str), after: Option<&str>) -> Option<String> {
        let sib = self.siblings(place.0, place.1, place.2);
        match after {
            None => sib.first().map(|r| r.id.clone()),
            Some(a) => sib.iter().position(|r| r.id == a).and_then(|i| sib.get(i + 1)).map(|r| r.id.clone()),
        }
    }

    fn prev_before(&self, place: (&str, &str, &str), before: &str) -> Option<String> {
        let sib = self.siblings(place.0, place.1, place.2);
        sib.iter().position(|r| r.id == before).and_then(|i| i.checked_sub(1)).map(|i| sib[i].id.clone())
    }

    fn last_in(&self, place: (&str, &str, &str)) -> Option<String> {
        self.siblings(place.0, place.1, place.2).last().map(|r| r.id.clone())
    }

    /// Writes parsed blocks into a place between two neighbours, issuing
    /// ids (the first may keep `keep`). Returns their ids.
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        parsed: Vec<Parsed>,
        place: (&str, &str, &str),
        after: Option<String>,
        before: Option<String>,
        keep: Option<String>,
        by: &str,
        rev: i64,
        keys: &mut Map<String, Value>,
    ) -> Vec<String> {
        let w = writer(by);
        let pos = self.positions(place, after.as_deref(), before.as_deref(), parsed.len());
        let mut ids = Vec::new();
        let mut keep = keep;
        for (p, at) in parsed.into_iter().zip(pos) {
            let id = keep.take().unwrap_or_else(|| self.issue(&w));
            self.write_parsed(&id, p, place.0, place.1, place.2, at, by, rev, keys);
            ids.push(id);
        }
        ids
    }

    /// One parsed block (and a slide's blocks) under `id`.
    #[allow(clippy::too_many_arguments)]
    fn write_parsed(
        &mut self,
        id: &str,
        p: Parsed,
        tab: &str,
        parent: &str,
        region_hint: &str,
        pos: f64,
        by: &str,
        rev: i64,
        keys: &mut Map<String, Value>,
    ) {
        let w = writer(by);
        if let Some(k) = &p.key {
            keys.insert(k.clone(), json!(id));
        }
        let mut attrs = p.attrs;
        let mut text = p.text;
        // Chips get ids of their own, so keys never clash across blocks.
        if let Some(chips) = attrs.get("chips").and_then(Value::as_object).cloned() {
            let mut renamed = Map::new();
            let mut names = std::collections::HashMap::new();
            for (k, v) in chips {
                let new = if self.is_chip_id(&k) { k.clone() } else { self.issue(&w) };
                keys.insert(k.clone(), json!(new));
                names.insert(k, new.clone());
                renamed.insert(new, v);
            }
            text = md::rename_tokens(&text, |k| names.get(k).cloned());
            attrs["chips"] = Value::Object(renamed);
        }
        if p.kind == "pending" {
            attrs["by"] = json!(by);
        }
        let region = if parent.is_empty() {
            String::new()
        } else if !region_hint.is_empty() {
            region_hint.to_owned()
        } else {
            p.region.clone()
        };
        self.rows.insert(
            id.to_owned(),
            Row {
                id: id.to_owned(),
                tab: tab.to_owned(),
                parent: parent.to_owned(),
                region,
                pos,
                kind: p.kind.clone(),
                text,
                attrs,
                by: by.to_owned(),
                rev,
            },
        );
        if p.kind == "slide" {
            // A slide's blocks, region by region.
            let mut by_region: BTreeMap<String, Vec<Parsed>> = BTreeMap::new();
            for c in p.children {
                by_region.entry(c.region.clone()).or_default().push(c);
            }
            for (region, kids) in by_region {
                let place = (tab, id, region.as_str());
                let pos = self.positions(place, self.last_in(place).as_deref(), None, kids.len());
                for (c, at) in kids.into_iter().zip(pos) {
                    let cid = self.issue(&w);
                    let mut c = c;
                    c.region = region.clone();
                    self.write_parsed(&cid, c, tab, id, &region, at, by, rev, keys);
                }
            }
        }
    }

    /// Whether `k` is a chip id already in the document (kept when a block
    /// holding it is rewritten).
    fn is_chip_id(&self, k: &str) -> bool {
        self.rows.values().any(|r| r.attrs["chips"].get(k).is_some())
    }

    fn remove(&mut self, id: &str) {
        self.rows.remove(id);
        let kids: Vec<String> = self.rows.values().filter(|r| r.parent == id).map(|r| r.id.clone()).collect();
        for k in kids {
            self.remove(&k);
        }
    }

    fn guard(&self, r: &Row, if_rev: Option<i64>, if_hash: Option<&str>, rev: i64) -> anyhow::Result<()> {
        let (Some(seen), Some(hash)) = (if_rev, if_hash) else {
            return Err(bad_request(format!(
                "replacing or deleting a whole block needs ifRev and ifHash: read block {} first (view=block). (A pending block you \
                 were filling that someone has edited since is theirs now: read it and work around it.)",
                r.id
            )));
        };
        if seen > rev {
            return Err(bad_request(format!("ifRev {seen} is ahead of the document (version {rev})")));
        }
        let now = self.hash(r);
        if now != hash {
            return Err(mismatch(r, &now));
        }
        Ok(())
    }

    fn check_text_block(r: &Row) -> anyhow::Result<()> {
        if matches!(r.kind.as_str(), "slide" | "pending" | "embed") {
            return Err(bad_request(format!("block {} is a {}: replace it with markdown", r.id, r.kind)));
        }
        Ok(())
    }

    /// Parses Markdown for a place: a deck's top level takes slides.
    fn parse_for(&self, parent: &str, markdown: &str, blocks: &Map<String, Value>) -> anyhow::Result<Vec<Parsed>> {
        if self.meta.kind == "deck" && parent.is_empty() { md::parse_deck(markdown, blocks) } else { md::parse(markdown, blocks) }
    }

    // --- Operations -----------------------------------------------------------

    /// Applies a batch as `by`, making version `rev`.
    pub fn apply(&mut self, ops: &[DocOp], by: &str, rev: i64) -> anyhow::Result<Applied> {
        if ops.len() > d::MAX_OPS {
            return Err(bad_request(format!("a batch holds at most {} operations", d::MAX_OPS)));
        }
        let mut done = Applied::default();
        for (n, op) in ops.iter().enumerate() {
            let ids = self.apply_one(op, by, rev, &mut done.keys).map_err(|e| match e.downcast_ref::<crate::UserError>() {
                Some(crate::UserError::Conflict(m)) => conflict(format!("operation {}: {m}", n + 1)),
                Some(crate::UserError::BadRequest(m)) => bad_request(format!("operation {}: {m}", n + 1)),
                Some(crate::UserError::NotFound(m)) => not_found(format!("operation {}: {m}", n + 1)),
                _ => e,
            })?;
            done.ids.push(ids);
        }
        self.check_limits()?;
        Ok(done)
    }

    fn check_limits(&self) -> anyhow::Result<()> {
        if self.tabs.len() > d::MAX_TABS {
            return Err(bad_request(format!("a document has at most {} tabs", d::MAX_TABS)));
        }
        for t in &self.tabs {
            if self.tab_size(&t.id) > d::MAX_TAB_TEXT {
                return Err(bad_request(format!("tab {:?} is over {} MB of text", t.title, d::MAX_TAB_TEXT / (1024 * 1024))));
            }
            if self.meta.kind == "deck" && self.siblings(&t.id, "", "").len() > d::MAX_SLIDES {
                return Err(bad_request(format!("a deck has at most {} slides", d::MAX_SLIDES)));
            }
        }
        if self.meta.kind == "doc" && self.rows.values().any(|r| r.kind == "slide") {
            return Err(bad_request("slides are a deck's"));
        }
        Ok(())
    }

    fn first_tab_id(&self) -> anyhow::Result<String> {
        self.tabs_in_order().first().map(|t| t.id.clone()).ok_or_else(|| bad_request("the document has no tab"))
    }

    fn tab_named(&self, tab: Option<&str>) -> anyhow::Result<String> {
        match tab {
            Some(t) => self.tab(t).map(|t| t.id.clone()).ok_or_else(|| not_found(format!("no tab {t}"))),
            None => self.first_tab_id(),
        }
    }

    /// The end of a heading's section: the last block before the next
    /// heading of its level or higher.
    fn section_end(&self, heading: &Row) -> String {
        let level = md::level(&heading.text).max(1);
        let sib = self.siblings(&heading.tab, &heading.parent, &heading.region);
        let i = sib.iter().position(|r| r.id == heading.id).unwrap_or(0);
        let mut last = heading.id.clone();
        for r in &sib[i + 1..] {
            if r.kind == "heading" && md::level(&r.text) <= level {
                break;
            }
            last = r.id.clone();
        }
        last
    }

    fn apply_one(&mut self, op: &DocOp, by: &str, rev: i64, keys: &mut Map<String, Value>) -> anyhow::Result<Vec<String>> {
        match op {
            DocOp::Insert { markdown, blocks, before, after, section, slide, region, tab } => {
                // Where: (tab, parent, region) and the neighbours.
                let (place, lo, hi) = if let Some(a) = after.as_deref().or(section.as_deref()) {
                    let r = self.row(a)?.clone();
                    let a = if section.is_some() {
                        if r.kind != "heading" {
                            return Err(bad_request(format!("block {} is not a heading", r.id)));
                        }
                        self.section_end(&r)
                    } else {
                        r.id.clone()
                    };
                    let place = (r.tab.clone(), r.parent.clone(), r.region.clone());
                    let hi = self.next_after((&place.0, &place.1, &place.2), Some(&a));
                    (place, Some(a), hi)
                } else if let Some(b) = before {
                    let r = self.row(b)?.clone();
                    let place = (r.tab.clone(), r.parent.clone(), r.region.clone());
                    let lo = self.prev_before((&place.0, &place.1, &place.2), b);
                    (place, lo, Some(b.clone()))
                } else if let Some(s) = slide {
                    let r = self.row(s)?.clone();
                    if r.kind != "slide" {
                        return Err(bad_request(format!("block {s} is not a slide")));
                    }
                    let layout = r.attrs["layout"].as_str().unwrap_or("body");
                    let regions = d::regions(layout);
                    let region = match region {
                        Some(x) if regions.contains(&x.as_str()) => x.clone(),
                        Some(x) => return Err(bad_request(format!("a {layout} slide's regions are {}, not {x:?}", regions.join(", ")))),
                        None => regions.iter().find(|r| **r != "title").copied().unwrap_or(regions[0]).to_owned(),
                    };
                    let place = (r.tab.clone(), r.id.clone(), region);
                    let lo = self.last_in((&place.0, &place.1, &place.2));
                    (place, lo, None)
                } else {
                    let t = self.tab_named(tab.as_deref())?;
                    let place = (t, String::new(), String::new());
                    let lo = self.last_in((&place.0, &place.1, &place.2));
                    (place, lo, None)
                };
                let parsed = self.parse_for(&place.1, markdown, blocks)?;
                if parsed.is_empty() {
                    return Err(bad_request("nothing to insert: the markdown is empty"));
                }
                Ok(self.put(parsed, (&place.0, &place.1, &place.2), lo, hi, None, by, rev, keys))
            }
            DocOp::Replace { id, markdown, blocks, find, with_text, nth, cell, text, layout, notes, hidden, if_rev, if_hash } => {
                let r = self.row(id)?.clone();
                if let Some(markdown) = markdown {
                    // Its writer fills a block still pending without a
                    // guard: once you edit it, it isn't pending any more.
                    let own_pending = r.kind == "pending" && r.attrs["by"] == by && if_hash.is_none();
                    if !own_pending {
                        self.guard(&r, *if_rev, if_hash.as_deref(), rev)?;
                    }
                    let parsed = self.parse_for(&r.parent, markdown, blocks)?;
                    if parsed.is_empty() {
                        return Err(bad_request("the markdown is empty: delete the block instead"));
                    }
                    return Ok(self.replace_with(&r, parsed, by, rev, keys));
                }
                if let Some(find) = find {
                    Self::check_text_block(&r)?;
                    let with = with_text.as_deref().unwrap_or("");
                    let found: Vec<usize> = r.text.match_indices(find.as_str()).map(|(i, _)| i).collect();
                    if find.is_empty() || found.is_empty() {
                        return Err(conflict(format!(
                            "guard_mismatch: {find:?} is not in block {} (now by {}, hash {}): read it again",
                            r.id,
                            r.by,
                            self.hash(&r)
                        )));
                    }
                    let i = match nth {
                        Some(n) if *n >= 1 && (*n as usize) <= found.len() => found[*n as usize - 1],
                        Some(n) => return Err(bad_request(format!("{find:?} is in block {} {} times, not {n}", r.id, found.len()))),
                        None if found.len() > 1 => {
                            return Err(bad_request(format!("{find:?} is in block {} {} times: say which with nth", r.id, found.len())));
                        }
                        None => found[0],
                    };
                    let mut t = r.text.clone();
                    t.replace_range(i..i + find.len(), with);
                    let row = self.rows.get_mut(id).unwrap();
                    row.text = t;
                    row.by = by.to_owned();
                    row.rev = rev;
                    return Ok(vec![id.clone()]);
                }
                if let Some([row_i, col]) = cell {
                    if r.kind != "table" {
                        return Err(bad_request(format!("block {} is not a table", r.id)));
                    }
                    let mut cells = md::table_cells(&r.text);
                    let row = cells.get_mut(*row_i).ok_or_else(|| bad_request(format!("the table has no row {row_i}")))?;
                    while row.len() <= *col {
                        row.push(String::new());
                    }
                    row[*col] = md::cell_text(text.as_deref().unwrap_or(""));
                    self.set_text(id, md::table_text(&cells), by, rev);
                    return Ok(vec![id.clone()]);
                }
                if layout.is_some() || notes.is_some() || hidden.is_some() {
                    if r.kind != "slide" {
                        return Err(bad_request(format!("block {} is not a slide", r.id)));
                    }
                    let row = self.rows.get_mut(id).unwrap();
                    if let Some(l) = layout {
                        if !d::LAYOUTS.iter().any(|(x, _)| x == l) {
                            return Err(bad_request(format!("{l:?} is not a layout")));
                        }
                        row.attrs["layout"] = json!(l);
                    }
                    if let Some(n) = notes {
                        row.attrs["notes"] = json!(n.trim());
                    }
                    if let Some(h) = hidden {
                        row.attrs["hidden"] = json!(h);
                    }
                    row.by = by.to_owned();
                    row.rev = rev;
                    // Blocks in regions the new layout lacks move to its
                    // first other region.
                    let layout = row.attrs["layout"].as_str().unwrap_or("body").to_owned();
                    self.rehome(id, &layout);
                    return Ok(vec![id.clone()]);
                }
                Err(bad_request(
                    "replace takes markdown (with ifRev and ifHash), find and with, cell and text, or a slide's layout, notes or hidden",
                ))
            }
            DocOp::Delete { id, if_rev, if_hash } => {
                let r = self.row(id)?.clone();
                self.guard(&r, *if_rev, if_hash.as_deref(), rev)?;
                self.remove(id);
                Ok(vec![])
            }
            DocOp::Move { id, before, after, tab } => {
                let r = self.row(id)?.clone();
                let (place, lo, hi) = if let Some(a) = after {
                    let x = self.row(a)?.clone();
                    let place = (x.tab.clone(), x.parent.clone(), x.region.clone());
                    let hi = self.next_after((&place.0, &place.1, &place.2), Some(a)).filter(|h| h != id);
                    (place, Some(a.clone()), hi)
                } else if let Some(b) = before {
                    let x = self.row(b)?.clone();
                    let place = (x.tab.clone(), x.parent.clone(), x.region.clone());
                    let lo = self.prev_before((&place.0, &place.1, &place.2), b).filter(|l| l != id);
                    (place, lo, Some(b.clone()))
                } else {
                    let t = self.tab_named(tab.as_deref())?;
                    let place = (t, String::new(), String::new());
                    let lo = self.last_in((&place.0, &place.1, &place.2)).filter(|l| l != id);
                    (place, lo, None)
                };
                if lo.as_deref() == Some(id.as_str()) || hi.as_deref() == Some(id.as_str()) {
                    return Err(bad_request("a block moves relative to another block"));
                }
                if (r.kind == "slide" || r.parent.is_empty() && self.meta.kind == "deck")
                    != (place.1.is_empty() && self.meta.kind == "deck")
                {
                    return Err(bad_request("slides move among slides, and a slide's blocks among blocks"));
                }
                if place.1 == *id {
                    return Err(bad_request("a slide can't move into itself"));
                }
                let pos = self.positions((&place.0, &place.1, &place.2), lo.as_deref(), hi.as_deref(), 1)[0];
                let old_tab = r.tab.clone();
                let row = self.rows.get_mut(id).unwrap();
                row.tab = place.0.clone();
                row.parent = place.1.clone();
                row.region = place.2.clone();
                row.pos = pos;
                row.by = by.to_owned();
                row.rev = rev;
                if old_tab != place.0 {
                    // A slide's blocks go with it.
                    let kids: Vec<String> = self.rows.values().filter(|k| k.parent == *id).map(|k| k.id.clone()).collect();
                    for k in kids {
                        self.rows.get_mut(&k).unwrap().tab = place.0.clone();
                    }
                }
                Ok(vec![id.clone()])
            }
            DocOp::Table { id, action, at, cells } => {
                let r = self.row(id)?.clone();
                if r.kind != "table" {
                    return Err(bad_request(format!("block {} is not a table", r.id)));
                }
                let mut rows = md::table_cells(&r.text);
                let cols = rows.iter().map(Vec::len).max().unwrap_or(1);
                let cells: Vec<String> = cells.iter().map(|c| md::cell_text(c)).collect();
                match action.as_str() {
                    "add_row" => {
                        let at = (*at).clamp(1, rows.len());
                        let mut row = cells.clone();
                        row.resize(cols, String::new());
                        rows.insert(at, row);
                    }
                    "delete_row" => {
                        if *at == 0 || *at >= rows.len() {
                            return Err(bad_request(format!("the table has no row {at} to delete (0 is its header)")));
                        }
                        rows.remove(*at);
                    }
                    "add_column" => {
                        let at = (*at).min(cols);
                        for (i, row) in rows.iter_mut().enumerate() {
                            row.resize(cols, String::new());
                            row.insert(at, cells.get(i).cloned().unwrap_or_default());
                        }
                    }
                    "delete_column" => {
                        if *at >= cols || cols == 1 {
                            return Err(bad_request(format!("the table has no column {at} to delete")));
                        }
                        for row in rows.iter_mut() {
                            if *at < row.len() {
                                row.remove(*at);
                            }
                        }
                    }
                    other => {
                        return Err(bad_request(format!(
                            "{other:?}: a table's actions are add_row, delete_row, add_column, delete_column"
                        )));
                    }
                }
                self.set_text(id, md::table_text(&rows), by, rev);
                Ok(vec![id.clone()])
            }
            DocOp::Set { title, theme, size, tab, tab_title, tabs, block, chip, value } => {
                let mut did = false;
                if let Some(t) = title {
                    self.meta.title = check_title(t)?;
                    did = true;
                }
                if let Some(t) = theme {
                    self.deck_only("a theme")?;
                    self.meta.theme = t.trim().to_owned();
                    did = true;
                }
                if let Some(s) = size {
                    self.deck_only("a size")?;
                    if !matches!(s.as_str(), "16:9" | "4:3") {
                        return Err(bad_request("a deck is 16:9 or 4:3"));
                    }
                    self.meta.size = s.clone();
                    did = true;
                }
                if let Some(t) = tab_title {
                    let id = self.tab_named(tab.as_deref())?;
                    let title = check_title(t)?;
                    self.tabs.iter_mut().find(|x| x.id == id).unwrap().title = title;
                    did = true;
                }
                if let Some(order) = tabs {
                    let mut have: Vec<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
                    let mut want: Vec<&str> = order.iter().map(String::as_str).collect();
                    have.sort_unstable();
                    want.sort_unstable();
                    if have != want {
                        return Err(bad_request("tabs: give every tab's id once, in the new order"));
                    }
                    for (i, id) in order.iter().enumerate() {
                        self.tabs.iter_mut().find(|t| t.id == *id).unwrap().pos = (i as f64 + 1.0) * 1024.0;
                    }
                    did = true;
                }
                if let (Some(b), Some(c)) = (block, chip) {
                    let r = self.row(b)?;
                    if r.attrs["chips"].get(c).is_none() {
                        return Err(not_found(format!("no chip {c} in block {b}")));
                    }
                    let v = value.clone().ok_or_else(|| bad_request("set a chip with its value"))?;
                    let row = self.rows.get_mut(b).unwrap();
                    row.attrs["chips"][c] = v;
                    row.by = by.to_owned();
                    row.rev = rev;
                    did = true;
                }
                if !did {
                    return Err(bad_request("set takes title, theme, size, tab_title (with tab), tabs, or block, chip and value"));
                }
                Ok(vec![])
            }
            DocOp::AddTab { title, markdown, blocks, after } => {
                if self.meta.kind == "deck" {
                    return Err(bad_request("a deck has one tab, its slides"));
                }
                let title = check_title(title)?;
                let order = self.tabs_in_order().iter().map(|t| (t.id.clone(), t.pos)).collect::<Vec<_>>();
                let pos = match after {
                    Some(a) => {
                        let i = order.iter().position(|(id, _)| id == a).ok_or_else(|| not_found(format!("no tab {a}")))?;
                        match order.get(i + 1) {
                            Some((_, next)) => (order[i].1 + next) / 2.0,
                            None => order[i].1 + 1024.0,
                        }
                    }
                    None => order.last().map(|(_, p)| p + 1024.0).unwrap_or(1024.0),
                };
                let id = self.issue(&writer(by));
                self.tabs.push(Tab { id: id.clone(), title, pos });
                let parsed = md::parse(markdown, blocks)?;
                let mut ids = vec![id.clone()];
                ids.extend(self.put(parsed, (&id, "", ""), None, None, None, by, rev, keys));
                Ok(ids)
            }
            DocOp::DeleteTab { tab } => {
                self.tab(tab).ok_or_else(|| not_found(format!("no tab {tab}")))?;
                if self.tabs.len() == 1 {
                    return Err(bad_request("a document keeps at least one tab"));
                }
                self.tabs.retain(|t| t.id != *tab);
                self.rows.retain(|_, r| r.tab != *tab);
                Ok(vec![])
            }
        }
    }

    fn deck_only(&self, what: &str) -> anyhow::Result<()> {
        if self.meta.kind != "deck" {
            return Err(bad_request(format!("{what} is a deck's")));
        }
        Ok(())
    }

    fn set_text(&mut self, id: &str, text: String, by: &str, rev: i64) {
        let row = self.rows.get_mut(id).unwrap();
        row.text = text;
        row.by = by.to_owned();
        row.rev = rev;
    }

    /// A slide's blocks in regions its layout lacks go to its first other one.
    fn rehome(&mut self, slide: &str, layout: &str) {
        let regions = d::regions(layout);
        let default = regions.iter().find(|r| **r != "title").copied().unwrap_or(regions[0]).to_owned();
        let stray: Vec<String> =
            self.rows.values().filter(|r| r.parent == slide && !regions.contains(&r.region.as_str())).map(|r| r.id.clone()).collect();
        for id in stray {
            let tab = self.rows[&id].tab.clone();
            let lo = self.last_in((&tab, slide, &default));
            let pos = self.positions((&tab, slide, &default), lo.as_deref(), None, 1)[0];
            let r = self.rows.get_mut(&id).unwrap();
            r.region = default.clone();
            r.pos = pos;
        }
    }

    /// A block replaced by parsed Markdown: the first keeps its id; a
    /// slide's blocks keep theirs region by region, in order.
    fn replace_with(&mut self, r: &Row, parsed: Vec<Parsed>, by: &str, rev: i64, keys: &mut Map<String, Value>) -> Vec<String> {
        let place = (r.tab.clone(), r.parent.clone(), r.region.clone());
        let hi = self.next_after((&place.0, &place.1, &place.2), Some(&r.id));
        // The old slide's blocks, to reuse their ids.
        let mut old_kids: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if r.kind == "slide" {
            for k in self.children(r) {
                old_kids.entry(k.region.clone()).or_default().push(k.id.clone());
            }
        }
        self.remove(&r.id);
        let mut parsed = parsed.into_iter();
        let first = parsed.next().unwrap();
        let mut ids = vec![r.id.clone()];
        self.write_parsed(&r.id, first, &place.0, &place.1, &place.2, r.pos, by, rev, keys);
        // Region blocks of a rewritten slide take the old ids in order.
        if !old_kids.is_empty() && self.rows[&r.id].kind == "slide" {
            let mut news: Vec<(String, String, f64)> =
                self.rows.values().filter(|k| k.parent == r.id).map(|k| (k.id.clone(), k.region.clone(), k.pos)).collect();
            news.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)));
            let mut used: BTreeMap<String, usize> = BTreeMap::new();
            for (id, region, _) in news {
                let n = used.entry(region.clone()).or_default();
                if let Some(old) = old_kids.get(&region).and_then(|v| v.get(*n)) {
                    let mut row = self.rows.remove(&id).unwrap();
                    row.id = old.clone();
                    self.rows.insert(old.clone(), row);
                }
                *n += 1;
            }
        }
        let rest: Vec<Parsed> = parsed.collect();
        if !rest.is_empty() {
            ids.extend(self.put(rest, (&place.0, &place.1, &place.2), Some(r.id.clone()), hi, None, by, rev, keys));
        }
        ids
    }
}

pub fn check_title(t: &str) -> anyhow::Result<String> {
    let t = t.trim();
    if t.is_empty() || t.chars().count() > 300 {
        return Err(bad_request("a title is 1 to 300 characters"));
    }
    Ok(t.to_owned())
}

fn mismatch(r: &Row, now: &str) -> anyhow::Error {
    conflict(format!(
        "guard_mismatch: block {} changed since you read it (now version {} by {}, hash {now}). Their edit wins: read it again and say what you kept",
        r.id, r.rev, r.by
    ))
}

/// What changed between two states, for the history: each block, tab and
/// the document's fields as they are after (null: gone).
pub fn changes(before: &State, after: &State) -> Value {
    let mut blocks = Map::new();
    for (id, r) in &after.rows {
        if before.rows.get(id) != Some(r) {
            blocks.insert(id.clone(), serde_json::to_value(r).unwrap_or(Value::Null));
        }
    }
    for id in before.rows.keys() {
        if !after.rows.contains_key(id) {
            blocks.insert(id.clone(), Value::Null);
        }
    }
    let mut tabs = Map::new();
    for t in &after.tabs {
        if before.tab(&t.id) != Some(t) {
            tabs.insert(t.id.clone(), serde_json::to_value(t).unwrap_or(Value::Null));
        }
    }
    for t in &before.tabs {
        if after.tab(&t.id).is_none() {
            tabs.insert(t.id.clone(), Value::Null);
        }
    }
    let added: Vec<&String> = after.rows.keys().filter(|id| !before.rows.contains_key(*id)).collect();
    let mut out = json!({"blocks": blocks, "tabs": tabs, "clock": after.clock, "added": added});
    if before.meta != after.meta {
        out["meta"] = serde_json::to_value(&after.meta).unwrap_or(Value::Null);
    }
    out
}

/// A state with a version's changes applied (to rebuild old versions).
pub fn replay(state: &mut State, changes: &Value) {
    for (id, v) in changes["blocks"].as_object().into_iter().flatten() {
        match serde_json::from_value::<Row>(v.clone()) {
            Ok(r) => {
                state.rows.insert(id.clone(), r);
            }
            Err(_) => {
                state.rows.remove(id);
            }
        }
    }
    for (id, v) in changes["tabs"].as_object().into_iter().flatten() {
        state.tabs.retain(|t| t.id != *id);
        if let Ok(t) = serde_json::from_value::<Tab>(v.clone()) {
            state.tabs.push(t);
        }
    }
    if let Ok(m) = serde_json::from_value::<Meta>(changes["meta"].clone()) {
        state.meta = m;
    }
    if let Some(c) = changes["clock"].as_i64() {
        state.clock = c;
    }
}

/// Whether nothing changed.
pub fn is_empty(changes: &Value) -> bool {
    changes["blocks"].as_object().is_none_or(Map::is_empty)
        && changes["tabs"].as_object().is_none_or(Map::is_empty)
        && changes.get("meta").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(kind: &str, md_text: &str, blocks: Value) -> State {
        let mut s = State::new(Meta { kind: kind.into(), title: "T".into(), ..Default::default() });
        s.tabs.push(Tab { id: "u.0".into(), title: "T".into(), pos: 1024.0 });
        let blocks: Map<String, Value> = serde_json::from_value(blocks).unwrap();
        s.apply(
            &[DocOp::Insert {
                markdown: md_text.into(),
                blocks,
                before: None,
                after: None,
                section: None,
                slide: None,
                region: None,
                tab: None,
            }],
            "you",
            1,
        )
        .unwrap();
        s
    }

    fn top(s: &State) -> Vec<String> {
        s.siblings("u.0", "", "").iter().map(|r| r.id.clone()).collect()
    }

    fn replace_md(id: &str, markdown: &str, if_rev: Option<i64>, if_hash: Option<String>) -> DocOp {
        DocOp::Replace {
            id: id.into(),
            markdown: Some(markdown.into()),
            blocks: Map::new(),
            find: None,
            with_text: None,
            nth: None,
            cell: None,
            text: None,
            layout: None,
            notes: None,
            hidden: None,
            if_rev,
            if_hash,
        }
    }

    #[test]
    fn ids_are_stable_and_guards_refuse_stale_edits() {
        let mut s = doc("doc", "# Plan\n\nFirst words.\n\n<?c6 block s1?>", json!({"s1": {"type": "pending", "intent": "Risks"}}));
        let ids = top(&s);
        assert_eq!(ids, ["u.1", "u.2", "u.3"]);
        assert_eq!(s.rows["u.3"].attrs["by"], "you");
        // Words change; the id stays.
        let para = s.rows["u.2"].clone();
        let h = s.hash(&para);
        let sess = "session:0193abcdef";
        s.apply(&[replace_md("u.2", "Other words.", Some(1), Some(h.clone()))], sess, 2).unwrap();
        assert_eq!(top(&s), ids);
        assert_eq!((s.rows["u.2"].text.as_str(), s.rows["u.2"].by.as_str()), ("Other words.", sess));
        // The same guard again: it changed since, refused, nothing lands.
        let before = s.clone();
        let e = s.apply(&[replace_md("u.1", "# New", None, None)], "you", 3).unwrap_err();
        assert!(e.to_string().contains("needs ifRev and ifHash"), "{e}");
        let e = s.apply(&[replace_md("u.2", "Mine.", Some(1), Some(h))], "you", 3).unwrap_err();
        assert!(matches!(e.downcast_ref::<crate::UserError>(), Some(crate::UserError::Conflict(_))), "{e}");
        assert!(e.to_string().contains("guard_mismatch"), "{e}");
        assert_eq!(s, before, "a refused batch changes nothing");
        // Filling a pending section: the first block keeps its id, the
        // rest get the writer's.
        let p = s.rows["u.3"].clone();
        let applied = s.apply(&[replace_md("u.3", "## Risks\n\nSlips.", Some(2), Some(s.hash(&p)))], sess, 3).unwrap();
        assert_eq!(applied.ids, [vec!["u.3".to_owned(), "s0193ab.4".to_owned()]]);
        assert_eq!(s.rows["u.3"].kind, "heading");
        // Find and replace words: refused once they're gone.
        let find = |f: &str, w: &str| DocOp::Replace {
            id: "u.2".into(),
            markdown: None,
            blocks: Map::new(),
            find: Some(f.into()),
            with_text: Some(w.into()),
            nth: None,
            cell: None,
            text: None,
            layout: None,
            notes: None,
            hidden: None,
            if_rev: None,
            if_hash: None,
        };
        s.apply(&[find("Other", "Better")], "you", 4).unwrap();
        assert_eq!(s.rows["u.2"].text, "Better words.");
        assert!(s.apply(&[find("Other", "x")], "you", 5).unwrap_err().to_string().contains("guard_mismatch"));
        // Insert at a section's end, and move.
        s.apply(
            &[DocOp::Insert {
                markdown: "Also.".into(),
                blocks: Map::new(),
                before: None,
                after: None,
                section: Some("u.1".into()),
                slide: None,
                region: None,
                tab: None,
            }],
            "you",
            5,
        )
        .unwrap();
        assert_eq!(top(&s), ["u.1", "u.2", "u.3", "s0193ab.4", "u.5"], "## Risks is inside # Plan's section");
        s.apply(&[DocOp::Move { id: "u.5".into(), before: Some("u.1".into()), after: None, tab: None }], "you", 6).unwrap();
        assert_eq!(top(&s)[0], "u.5");
        let mut blocks = Map::new();
        assert_eq!(s.markdown("u.0", &mut blocks), "Also.\n\n# Plan\n\nBetter words.\n\n## Risks\n\nSlips.");
    }

    #[test]
    fn a_writer_fills_its_own_pending_blocks_until_someone_takes_them() {
        let sess = "session:abc123";
        let mut s = State::new(Meta { kind: "doc".into(), title: "T".into(), ..Default::default() });
        s.first_tab("T", sess);
        let blocks: Map<String, Value> =
            serde_json::from_value(json!({"a": {"type": "pending", "intent": "A"}, "b": {"type": "pending", "intent": "B"}})).unwrap();
        let ins = DocOp::Insert {
            markdown: "<?c6 block a?>\n\n<?c6 block b?>".into(),
            blocks,
            before: None,
            after: None,
            section: None,
            slide: None,
            region: None,
            tab: None,
        };
        let got = s.apply(&[ins], sess, 1).unwrap();
        let (a, b) = (got.keys["a"].as_str().unwrap().to_owned(), got.keys["b"].as_str().unwrap().to_owned());
        assert_eq!(a, "sabc123.2");
        // Someone else can't skip the guard on it.
        assert!(s.apply(&[replace_md(&a, "## A", None, None)], "you", 2).is_err());
        s.apply(&[replace_md(&a, "## A\n\nDone.", None, None)], sess, 2).unwrap();
        // You take B by editing it: the writer's fill now needs a guard.
        let hb = s.hash(&s.rows[&b]);
        s.apply(&[replace_md(&b, "Mine.", Some(2), Some(hb))], "you", 3).unwrap();
        let e = s.apply(&[replace_md(&b, "## B", None, None)], sess, 4).unwrap_err();
        assert!(e.to_string().contains("is theirs now"), "{e}");
    }

    #[test]
    fn decks_keep_slide_and_bullet_ids() {
        let mut s = doc(
            "deck",
            "# Q3\n\nWhere we are\n\n---\n\n## Numbers\n\n- up\n- down\n\n---\n\n<?c6 block p?>",
            json!({"p": {"type": "pending", "intent": "Next steps"}}),
        );
        let slides = top(&s);
        assert_eq!(slides.len(), 3);
        let s2 = s.rows[&slides[1]].clone();
        let bullets: Vec<String> = s.children(&s2).iter().map(|c| c.id.clone()).collect();
        assert_eq!(bullets.len(), 3);
        // A bullet changes: the slide's hash does too.
        let h = s.hash(&s2);
        let b = s.rows[&bullets[1]].clone();
        s.apply(&[replace_md(&bullets[1], "- way up", Some(1), Some(s.hash(&b)))], "you", 2).unwrap();
        assert_ne!(s.hash(&s.rows[&slides[1]]), h);
        // The slide rewritten: its blocks keep their ids, region by region.
        let s2 = s.rows[&slides[1]].clone();
        let new = "<!-- layout: body -->\n## Numbers now\n\n- way up\n- flat\n\n<!-- notes: say it slowly -->";
        s.apply(&[replace_md(&slides[1], new, Some(2), Some(s.hash(&s2)))], "you", 3).unwrap();
        let s2 = s.rows[&slides[1]].clone();
        assert_eq!(s.children(&s2).iter().map(|c| c.id.clone()).collect::<Vec<_>>(), bullets);
        assert_eq!(s2.attrs["notes"], "say it slowly");
        let mut blocks = Map::new();
        assert_eq!(s.markdown_of(&s2, &mut blocks), new.replace("-->\n## ", "-->\n\n## "));
        // Layout changes move blocks out of regions it lacks.
        let op = DocOp::Replace {
            id: slides[1].clone(),
            markdown: None,
            blocks: Map::new(),
            find: None,
            with_text: None,
            nth: None,
            cell: None,
            text: None,
            layout: Some("quote".into()),
            notes: None,
            hidden: Some(true),
            if_rev: None,
            if_hash: None,
        };
        s.apply(&[op], "you", 4).unwrap();
        let s2 = s.rows[&slides[1]].clone();
        assert!(s.children(&s2).iter().all(|c| c.region == "quote"));
        assert!(
            s.apply(&[DocOp::AddTab { title: "x".into(), markdown: String::new(), blocks: Map::new(), after: None }], "you", 5).is_err()
        );
    }

    #[test]
    fn history_replays_to_the_same_state() {
        let mut s = doc("doc", "| a | b |\n|---|---|\n| 1 | 2 |", json!({}));
        let v1 = s.clone();
        let id = top(&s)[0].clone();
        let ops = [
            DocOp::Table { id: id.clone(), action: "add_row".into(), at: 9, cells: vec!["3".into(), "4".into()] },
            DocOp::Replace {
                id: id.clone(),
                markdown: None,
                blocks: Map::new(),
                find: None,
                with_text: None,
                nth: None,
                cell: Some([1, 1]),
                text: Some("two|2".into()),
                layout: None,
                notes: None,
                hidden: None,
                if_rev: None,
                if_hash: None,
            },
            DocOp::AddTab { title: "Sources".into(), markdown: "- one".into(), blocks: Map::new(), after: None },
            DocOp::Set {
                title: Some("New".into()),
                theme: None,
                size: None,
                tab: None,
                tab_title: None,
                tabs: None,
                block: None,
                chip: None,
                value: None,
            },
        ];
        s.apply(&ops, "you", 2).unwrap();
        assert_eq!(s.rows[&id].text, "| a | b |\n| --- | --- |\n| 1 | two\\|2 |\n| 3 | 4 |");
        let ch = changes(&v1, &s);
        let mut again = v1.clone();
        replay(&mut again, &ch);
        assert_eq!(again, s);
        assert!(is_empty(&changes(&s, &s)));
    }
}
